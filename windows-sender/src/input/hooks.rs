//! Low-level hooks: local input suppression (spec §20) and hotkey capture (spec §16/§19).
//!
//! ## Why movement is handled differently from everything else
//!
//! A low-level hook that returns non-zero does not merely hide the event from other
//! applications - the event is dropped before the system turns it into Raw Input, so *this*
//! process stops seeing it too. Swallowing mouse movement therefore starves the very pipeline
//! that makes the remote cursor smooth: instead of 1000 raw deltas per second we were left with
//! the handful of events per second that slipped through while the hook was busy.
//!
//! So the two kinds of input take different routes while the Mac owns the input:
//!
//! * **Movement** is never swallowed. Raw Input keeps delivering unaccelerated device deltas at
//!   full rate, and the local cursor is held still with `ClipCursor` instead. Local windows do
//!   receive `WM_MOUSEMOVE`, but the pointer cannot move, so nothing reacts to it.
//! * **Buttons, wheel and keys** are swallowed here and forwarded *from here*, because by the
//!   same rule this callback is the last place they exist. They are discrete and low-rate, so the
//!   hook's view of them is exactly as good as Raw Input's.
//!
//! Nothing is ever forwarded twice: an event this hook swallows never reaches Raw Input, and an
//! event it does not swallow - including everything aimed at a window of higher integrity level,
//! which skips the hook entirely - is delivered by Raw Input as usual.
//!
//! These callbacks sit on the critical path of every physical event, so they touch nothing but
//! atomics and the parsed hotkey table. They keep their own modifier mask rather than reaching
//! for the shared tracker's lock on every event, and they never call `GetAsyncKeyState` - once a
//! key is swallowed the async key state stops being updated, so it would be lying to us.
//!
//! Known limitation: a low-level hook cannot hide input from an application that reads Raw Input
//! or DirectInput directly, which includes most games and anything behind an anti-cheat
//! (spec §20 "Ограничение", §21).

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU16, AtomicU64, Ordering::Relaxed};

use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, ClipCursor, GetCursorPos, GetSystemMetrics, SetWindowsHookExW,
    UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
    LLMHF_INJECTED, MSLLHOOKSTRUCT, SM_XVIRTUALSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_XBUTTONDOWN,
    WM_XBUTTONUP,
};

use crate::input::{self, keymap, KeyDecision};
use crate::protocol::{button, modmask};
use crate::state::{state, Target};

static KEYBOARD_HOOK: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
static MOUSE_HOOK: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

/// Modifier state as seen by the hook chain, independent of the Raw Input tracker.
static HOOK_MODS: AtomicU16 = AtomicU16::new(0);
/// Trigger keys whose release must be swallowed because their press was eaten as a hotkey.
static SWALLOWED: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
/// Left edge of the virtual desktop, refreshed on display changes.
static VIRTUAL_LEFT: AtomicI32 = AtomicI32::new(0);
/// Debounce for edge switching, in microseconds since process start.
static LAST_EDGE_SWITCH_US: AtomicU64 = AtomicU64::new(0);
const EDGE_COOLDOWN_US: u64 = 700_000;

/// Where the local cursor is pinned while the Mac owns the input.
static CLIP_ACTIVE: AtomicBool = AtomicBool::new(false);
static CLIP_X: AtomicI32 = AtomicI32::new(0);
static CLIP_Y: AtomicI32 = AtomicI32::new(0);
/// When the clip was last re-applied, in microseconds since process start.
static LAST_CLIP_US: AtomicU64 = AtomicU64::new(0);
const CLIP_REFRESH_US: u64 = 50_000;

pub fn install() -> bool {
    refresh_screen_bounds();
    let keyboard =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), ptr::null_mut(), 0) };
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), ptr::null_mut(), 0) };
    KEYBOARD_HOOK.store(keyboard, Relaxed);
    MOUSE_HOOK.store(mouse, Relaxed);
    let ok = !keyboard.is_null() && !mouse.is_null();
    input::set_hooks_active(ok);
    if !ok {
        crate::log::warn(
            "could not install the low-level input hooks; hotkeys still work through Raw Input, \
             but local input will not be suppressed",
        );
    }
    ok
}

pub fn uninstall() {
    release_cursor_clip();
    for hook in [&KEYBOARD_HOOK, &MOUSE_HOOK] {
        let handle = hook.swap(ptr::null_mut(), Relaxed);
        if !handle.is_null() {
            unsafe { UnhookWindowsHookEx(handle) };
        }
    }
    input::set_hooks_active(false);
}

pub fn refresh_screen_bounds() {
    VIRTUAL_LEFT.store(unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) }, Relaxed);
}

// ---------------------------------------------------------------------------
// Cursor pinning
// ---------------------------------------------------------------------------

pub fn on_target_changed(target: Target) {
    match target {
        Target::RemoteMac if state().suppress.load(Relaxed) => {
            let mut point = POINT { x: 0, y: 0 };
            unsafe { GetCursorPos(&mut point) };
            CLIP_X.store(point.x, Relaxed);
            CLIP_Y.store(point.y, Relaxed);
            CLIP_ACTIVE.store(true, Relaxed);
            apply_cursor_clip();
        }
        _ => release_cursor_clip(),
    }
}

fn apply_cursor_clip() {
    let x = CLIP_X.load(Relaxed);
    let y = CLIP_Y.load(Relaxed);
    let rect = RECT { left: x, top: y, right: x + 1, bottom: y + 1 };
    unsafe { ClipCursor(&rect) };
}

fn release_cursor_clip() {
    if CLIP_ACTIVE.swap(false, Relaxed) {
        unsafe { ClipCursor(ptr::null()) };
    }
}

/// The system drops the clip whenever the foreground window changes, so it has to be re-applied
/// periodically. Called from the UI timer.
pub fn refresh_cursor_clip() {
    if CLIP_ACTIVE.load(Relaxed) {
        apply_cursor_clip();
    }
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

fn modifier_bit_for_vk(vk: u16) -> u16 {
    match vk {
        0xA0 => modmask::L_SHIFT,
        0xA1 => modmask::R_SHIFT,
        0xA2 => modmask::L_CTRL,
        0xA3 => modmask::R_CTRL,
        0xA4 => modmask::L_ALT,
        0xA5 => modmask::R_ALT,
        0x5B => modmask::L_GUI,
        0x5C => modmask::R_GUI,
        _ => 0,
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam != 0 {
        let info = &*(lparam as *const KBDLLHOOKSTRUCT);
        // Let synthetic input (accessibility tools, remote desktop) through untouched.
        if info.flags & LLKHF_INJECTED == 0 {
            let vk = info.vkCode as u16;
            let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let bit = modifier_bit_for_vk(vk);
            let mods = if bit != 0 {
                let previous = HOOK_MODS.load(Relaxed);
                let next = if down { previous | bit } else { previous & !bit };
                HOOK_MODS.store(next, Relaxed);
                next
            } else {
                HOOK_MODS.load(Relaxed)
            };

            if down {
                if bit == 0 {
                    if let Some(action) = input::hotkeys().match_vk(vk, mods) {
                        SWALLOWED[vk as usize].store(true, Relaxed);
                        input::dispatch(action);
                        return 1;
                    }
                }
            } else if SWALLOWED[vk as usize].swap(false, Relaxed) {
                return 1;
            }

            if state().suppress.load(Relaxed) {
                forward_swallowed_key(info, down);
                return 1;
            }
        }
    }
    CallNextHookEx(ptr::null_mut(), code, wparam, lparam)
}

/// Feeds a keystroke this hook is about to swallow into the forwarding path.
///
/// It has to happen here. Returning non-zero does not merely hide the event from other
/// applications - the system drops it before it becomes Raw Input, so a key swallowed and not
/// forwarded from this callback is simply gone. That is what left the Mac with a working mouse and
/// a dead keyboard while local suppression was on.
fn forward_swallowed_key(info: &KBDLLHOOKSTRUCT, down: bool) {
    let hid = keymap::hid_usage(
        info.scanCode as u16,
        info.flags & LLKHF_EXTENDED != 0,
        info.vkCode as u16,
    );
    if hid == keymap::HID_NONE {
        return;
    }
    state().tel.raw_kbd_events.fetch_add(1, Relaxed);
    if let KeyDecision::Hotkey(action) = input::handle_key_event(hid, down) {
        input::dispatch(action);
    }
}

/// The same for a button, wheel notch or horizontal wheel notch.
fn forward_swallowed_mouse(info: &MSLLHOOKSTRUCT, message: u32) {
    let st = state();
    // Both the wheel delta and the X button index live in the high word of mouseData.
    let high = (info.mouseData >> 16) as u16;
    let (index, down) = match message {
        WM_LBUTTONDOWN => (button::LEFT, true),
        WM_LBUTTONUP => (button::LEFT, false),
        WM_RBUTTONDOWN => (button::RIGHT, true),
        WM_RBUTTONUP => (button::RIGHT, false),
        WM_MBUTTONDOWN => (button::MIDDLE, true),
        WM_MBUTTONUP => (button::MIDDLE, false),
        // XBUTTON1 is "back", XBUTTON2 is "forward".
        WM_XBUTTONDOWN | WM_XBUTTONUP => (
            if high == 2 { button::FORWARD } else { button::BACK },
            message == WM_XBUTTONDOWN,
        ),
        // A signed WHEEL_DELTA multiple, 120 to the notch, exactly as Raw Input reports it.
        WM_MOUSEWHEEL => {
            st.tel.raw_mouse_events.fetch_add(1, Relaxed);
            input::handle_scroll(0, high as i16 as i32);
            return;
        }
        WM_MOUSEHWHEEL => {
            st.tel.raw_mouse_events.fetch_add(1, Relaxed);
            input::handle_scroll(high as i16 as i32, 0);
            return;
        }
        _ => return,
    };
    st.tel.raw_mouse_events.fetch_add(1, Relaxed);
    input::handle_button_event(index, down);
}

// ---------------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------------

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam != 0 {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        if info.flags & LLMHF_INJECTED == 0 {
            let st = state();
            let message = wparam as u32;

            if st.suppress.load(Relaxed) {
                if message == WM_MOUSEMOVE {
                    // Movement is deliberately passed through: swallowing it would also stop Raw
                    // Input from delivering the deltas we forward. ClipCursor keeps the local
                    // pointer still instead - but the system drops the clip on every foreground
                    // change, and waiting up to half a second for the UI timer to notice is long
                    // enough for the local cursor to visibly run away. Movement is the moment it
                    // matters, so it is re-applied from here, rate limited so a 1000 Hz mouse does
                    // not mean 1000 calls a second.
                    if CLIP_ACTIVE.load(Relaxed) {
                        let now = st.now_us();
                        if now.saturating_sub(LAST_CLIP_US.load(Relaxed)) > CLIP_REFRESH_US {
                            LAST_CLIP_US.store(now, Relaxed);
                            apply_cursor_clip();
                        }
                    }
                } else {
                    // Buttons and the wheel are hidden from Windows here, so - like keys - this is
                    // the last place they exist and the place they have to be forwarded from.
                    forward_swallowed_mouse(info, message);
                    return 1;
                }
            } else if message == WM_MOUSEMOVE
                && st.target() == Target::LocalWindows
                && st.edge_switch.load(Relaxed)
                && info.pt.x <= VIRTUAL_LEFT.load(Relaxed)
            {
                let now = st.now_us();
                if now.saturating_sub(LAST_EDGE_SWITCH_US.load(Relaxed)) > EDGE_COOLDOWN_US {
                    LAST_EDGE_SWITCH_US.store(now, Relaxed);
                    input::dispatch(input::HotkeyAction::SwitchToMac);
                }
            }
        }
    }
    CallNextHookEx(ptr::null_mut(), code, wparam, lparam)
}

