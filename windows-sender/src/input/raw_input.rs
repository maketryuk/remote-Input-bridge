//! Windows Raw Input: buffered reads on `WM_INPUT` (spec §8.1).
//!
//! Movement never allocates and never touches a lock: it is two `fetch_add`s on the cumulative
//! counters that the realtime thread samples. Buttons, wheel and keys go through the shared
//! tracker and out on the reliable queue.

use std::cell::RefCell;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::Relaxed};

use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HID_USAGE_GENERIC_KEYBOARD, HID_USAGE_GENERIC_MOUSE, HID_USAGE_PAGE_GENERIC,
};
use windows_sys::Win32::Foundation::{GetLastError, HWND};
use windows_sys::Win32::UI::Input::{
    GetRawInputBuffer, RegisterRawInputDevices, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RAWKEYBOARD, RAWMOUSE, MOUSE_MOVE_ABSOLUTE, RIDEV_INPUTSINK, RIM_TYPEKEYBOARD,
    RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    RI_KEY_BREAK, RI_KEY_E0, RI_KEY_E1, RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP, RI_MOUSE_BUTTON_5_DOWN,
    RI_MOUSE_BUTTON_5_UP, RI_MOUSE_HWHEEL, RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP,
    RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP, RI_MOUSE_RIGHT_BUTTON_DOWN,
    RI_MOUSE_RIGHT_BUTTON_UP, RI_MOUSE_WHEEL,
};

use crate::input::{self, keymap, KeyDecision};
use crate::net::NetMsg;
use crate::protocol::{button, Reliable};
use crate::state::{state, Target};

/// 16 KiB holds roughly 340 mouse events, far more than a 1000 Hz mouse produces between two
/// message-loop iterations. Declared as `u64` so the buffer is 8-byte aligned, which
/// `RAWINPUT` requires on x64.
const BUFFER_QWORDS: usize = 2048;
const MAX_BUFFER_QWORDS: usize = 64 * 1024;

thread_local! {
    static BUFFER: RefCell<Vec<u64>> = RefCell::new(vec![0u64; BUFFER_QWORDS]);
}

static ABS_VALID: AtomicBool = AtomicBool::new(false);
static ABS_X: AtomicI32 = AtomicI32::new(0);
static ABS_Y: AtomicI32 = AtomicI32::new(0);

pub fn register(hwnd: HWND) -> bool {
    // RIDEV_INPUTSINK: keep receiving input while some game holds the foreground. We do not use
    // RIDEV_NOLEGACY - it would break every other application on the machine.
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_MOUSE,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
        RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_KEYBOARD,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
    ];
    let ok = unsafe {
        RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if ok == 0 {
        crate::log::error(&format!(
            "RegisterRawInputDevices failed (error {})",
            unsafe { GetLastError() }
        ));
        return false;
    }
    true
}

/// Drains every queued raw event. Called from `WM_INPUT`; buffered reads keep a 1000 Hz mouse
/// from turning into 1000 window messages per second.
pub fn drain() {
    let header_size = size_of::<RAWINPUTHEADER>() as u32;
    BUFFER.with(|cell| {
        let mut buffer = cell.borrow_mut();
        loop {
            let mut size = (buffer.len() * size_of::<u64>()) as u32;
            let count = unsafe {
                GetRawInputBuffer(buffer.as_mut_ptr() as *mut RAWINPUT, &mut size, header_size)
            };
            if count == 0 {
                return; // queue drained
            }
            if count == u32::MAX {
                // Only documented failure worth recovering from: the buffer was too small.
                let needed_qwords = (size as usize / size_of::<u64>()) + 2;
                if needed_qwords > buffer.len() && needed_qwords <= MAX_BUFFER_QWORDS {
                    buffer.resize(needed_qwords.next_power_of_two(), 0);
                    continue;
                }
                crate::log::warn(&format!(
                    "GetRawInputBuffer failed (error {})",
                    unsafe { GetLastError() }
                ));
                return;
            }

            let mut ptr = buffer.as_ptr() as *const RAWINPUT;
            for _ in 0..count {
                let event = unsafe { &*ptr };
                match event.header.dwType {
                    RIM_TYPEMOUSE => handle_mouse(unsafe { &event.data.mouse }),
                    RIM_TYPEKEYBOARD => handle_keyboard(unsafe { &event.data.keyboard }),
                    _ => {}
                }
                // NEXTRAWINPUTBLOCK: dwSize rounded up to the pointer alignment.
                let advance = (event.header.dwSize as usize + 7) & !7;
                ptr = unsafe { (ptr as *const u8).add(advance) } as *const RAWINPUT;
            }
        }
    });
}

fn handle_mouse(mouse: &RAWMOUSE) {
    let st = state();
    st.tel.raw_mouse_events.fetch_add(1, Relaxed);

    if mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0 {
        // Tablets, RDP and some KVMs report absolute coordinates; difference them so the wire
        // format stays purely relative.
        let (x, y) = (mouse.lLastX, mouse.lLastY);
        if ABS_VALID.swap(true, Relaxed) {
            let dx = x - ABS_X.load(Relaxed);
            let dy = y - ABS_Y.load(Relaxed);
            if dx != 0 || dy != 0 {
                st.total_x.fetch_add(dx, Relaxed);
                st.total_y.fetch_add(dy, Relaxed);
            }
        }
        ABS_X.store(x, Relaxed);
        ABS_Y.store(y, Relaxed);
    } else if mouse.lLastX != 0 || mouse.lLastY != 0 {
        st.total_x.fetch_add(mouse.lLastX, Relaxed);
        st.total_y.fetch_add(mouse.lLastY, Relaxed);
    }

    let buttons = unsafe { mouse.Anonymous.Anonymous };
    let flags = buttons.usButtonFlags as u32;
    if flags == 0 {
        return;
    }

    if flags & RI_MOUSE_WHEEL != 0 {
        // usButtonData is a signed WHEEL_DELTA multiple; 120 == one notch. High-resolution mice
        // report fractions of that, which is exactly the resolution we want to keep (spec §13).
        st.scroll_y.fetch_add(buttons.usButtonData as i16 as i32, Relaxed);
    }
    if flags & RI_MOUSE_HWHEEL != 0 {
        st.scroll_x.fetch_add(buttons.usButtonData as i16 as i32, Relaxed);
    }

    const BUTTON_FLAGS: [(u32, u8, bool); 10] = [
        (RI_MOUSE_LEFT_BUTTON_DOWN, button::LEFT, true),
        (RI_MOUSE_LEFT_BUTTON_UP, button::LEFT, false),
        (RI_MOUSE_RIGHT_BUTTON_DOWN, button::RIGHT, true),
        (RI_MOUSE_RIGHT_BUTTON_UP, button::RIGHT, false),
        (RI_MOUSE_MIDDLE_BUTTON_DOWN, button::MIDDLE, true),
        (RI_MOUSE_MIDDLE_BUTTON_UP, button::MIDDLE, false),
        (RI_MOUSE_BUTTON_4_DOWN, button::BACK, true),
        (RI_MOUSE_BUTTON_4_UP, button::BACK, false),
        (RI_MOUSE_BUTTON_5_DOWN, button::FORWARD, true),
        (RI_MOUSE_BUTTON_5_UP, button::FORWARD, false),
    ];
    for (flag, index, down) in BUTTON_FLAGS {
        if flags & flag == 0 {
            continue;
        }
        let decision = input::with_tracker(|t| t.on_button(index as usize, down));
        if matches!(decision, KeyDecision::Forward { .. }) && st.target() == Target::RemoteMac {
            st.send(NetMsg::Input(Reliable::MouseButton { button: index, down }));
        }
    }
}

fn handle_keyboard(key: &RAWKEYBOARD) {
    let st = state();
    st.tel.raw_kbd_events.fetch_add(1, Relaxed);

    // 0xFF is the placeholder Windows uses for the first half of a multi-scan-code sequence.
    if key.VKey == 0xFF {
        return;
    }
    let flags = key.Flags as u32;
    let e1 = flags & RI_KEY_E1 != 0;
    if e1 && key.MakeCode == 0x1D {
        return; // Pause arrives as E1 1D 45; only the 45 half carries meaning.
    }
    let down = flags & RI_KEY_BREAK == 0;
    let hid = keymap::hid_usage(key.MakeCode, flags & RI_KEY_E0 != 0, key.VKey);
    if hid == keymap::HID_NONE {
        crate::log::debug(&format!(
            "unmapped key: scan {:#x} vk {:#x} flags {:#x}",
            key.MakeCode, key.VKey, flags
        ));
        return;
    }

    let decision = input::with_tracker(|t| t.on_key(hid, down, &input::hotkeys()));
    st.modifiers.store(input::modifiers(), Relaxed);

    match decision {
        KeyDecision::Hotkey(action) => {
            // The hook normally dispatches (it can also swallow the keystroke). This branch is
            // the safety net for when hook installation was refused.
            if !input::hooks_active() {
                input::dispatch(action);
            }
        }
        KeyDecision::Drop => {}
        KeyDecision::Forward { repeat } => {
            if st.target() == Target::RemoteMac {
                st.send(NetMsg::Input(Reliable::Key { hid_usage: hid, down, repeat }));
            }
        }
    }
}
