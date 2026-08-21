//! Low-level hooks: local input suppression (spec §20) and hotkey capture (spec §16/§19).
//!
//! These callbacks run on the message-loop thread and are on the critical path of every physical
//! event, so they do the absolute minimum: read one atomic, compare against the parsed hotkeys,
//! and return. They deliberately keep their own modifier mask instead of reaching for the shared
//! tracker's lock, and they never call `GetAsyncKeyState` - once a key is swallowed the async key
//! state stops being updated, so it would be lying to us.
//!
//! Known limitation: a low-level hook cannot hide input from an application that reads Raw
//! Input or DirectInput directly, which includes most games and anything behind an anti-cheat
//! (spec §20 "Ограничение", §21).

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU16, AtomicU64, Ordering::Relaxed};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetSystemMetrics, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION,
    KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSLLHOOKSTRUCT, SM_XVIRTUALSCREEN,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_MOUSEMOVE, WM_SYSKEYDOWN,
};

use crate::input;
use crate::protocol::modmask;
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
                return 1;
            }
        }
    }
    CallNextHookEx(ptr::null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam != 0 {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        if info.flags & LLMHF_INJECTED == 0 {
            let st = state();
            if st.suppress.load(Relaxed) {
                // Swallowing WM_MOUSEMOVE is also what pins the Windows cursor in place while
                // the Mac has the input.
                return 1;
            }
            if wparam as u32 == WM_MOUSEMOVE
                && st.target() == Target::LocalWindows
                && info.pt.x <= VIRTUAL_LEFT.load(Relaxed)
                && st.config().edge_switch
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
