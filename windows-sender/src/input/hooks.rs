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
//! * **Keys** are swallowed here and forwarded *from here*, because by the same rule this callback
//!   is the last place they exist. They are discrete and low-rate, so the hook's view of them is
//!   exactly as good as Raw Input's.
//! * **Buttons and the wheel** are swallowed here and *not* forwarded, because - and this is the
//!   part that looks wrong until you measure it - a mouse button swallowed by this hook still
//!   arrives as Raw Input. The rule that holds for movement and for keys does not hold for buttons.
//!
//! That asymmetry is not a guess. One press with the button held produced one WM_INPUT message and
//! two forwarded events while the hook forwarded buttons as well: every click reached the Mac
//! twice, which a menu bar reads as open-then-close. A keystroke in the same conditions produces no
//! WM_INPUT at all, which is why the keyboard has to be forwarded from here and the mouse must not
//! be.
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
    LLMHF_INJECTED, MSLLHOOKSTRUCT, SM_XVIRTUALSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN,
    WM_MOUSEMOVE, WM_SYSKEYDOWN,
};

use crate::input::{self, keymap, KeyDecision};
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

/// Where the local cursor is pinned while the Mac owns the input.
static CLIP_ACTIVE: AtomicBool = AtomicBool::new(false);
static CLIP_X: AtomicI32 = AtomicI32::new(0);
static CLIP_Y: AtomicI32 = AtomicI32::new(0);
/// When the clip was last re-applied, in microseconds since process start.
static LAST_CLIP_US: AtomicU64 = AtomicU64::new(0);
const CLIP_REFRESH_US: u64 = 50_000;

pub fn install() -> bool {
    refresh_screen_bounds();
    let keyboard = install_keyboard();
    apply_mouse_hook();
    keyboard
}

fn install_keyboard() -> bool {
    if !KEYBOARD_HOOK.load(Relaxed).is_null() {
        return true;
    }
    let handle =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), ptr::null_mut(), 0) };
    KEYBOARD_HOOK.store(handle, Relaxed);
    if handle.is_null() {
        crate::log::warn(
            "could not install the low-level keyboard hook; hotkeys still work through Raw Input, \
             but they will also reach whatever has focus, and keystrokes will not be suppressed",
        );
    }
    update_active();
    !handle.is_null()
}

/// Adds or removes the mouse hook so that it only exists while something needs it.
///
/// This is the hook that costs something. It sits on the path of every report a 1000 Hz mouse
/// produces, and Windows delivers that input to nobody until the callback returns - so while a game
/// is running and the input belongs to Windows, the cheapest thing this app can do is not be there
/// at all. It is needed only while the Mac owns the input (to swallow buttons and the wheel) or
/// while edge switching is on (to notice the pointer reaching the edge).
///
/// The keyboard hook is not treated this way on purpose. It fires at typing speed, so it costs
/// nothing worth measuring, and it is what stops `Ctrl+Alt+Left` from also reaching whatever has
/// focus - on an Intel graphics driver that same chord rotates the screen.
///
/// Must be called from the thread that owns the message loop: a low-level hook is delivered to the
/// thread that installed it, and a hook installed on a thread that does not pump messages never
/// fires.
pub fn apply_mouse_hook() {
    let st = state();
    let needed = (st.target() == Target::RemoteMac && st.suppress.load(Relaxed))
        || st.edge_switch.load(Relaxed);
    let present = !MOUSE_HOOK.load(Relaxed).is_null();
    if needed == present {
        return;
    }
    if needed {
        let handle =
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), ptr::null_mut(), 0) };
        MOUSE_HOOK.store(handle, Relaxed);
        if handle.is_null() {
            crate::log::warn(
                "could not install the low-level mouse hook; local clicks and the wheel will not \
                 be suppressed while the Mac has the input",
            );
        } else {
            crate::log::debug("mouse hook installed");
        }
    } else {
        let handle = MOUSE_HOOK.swap(ptr::null_mut(), Relaxed);
        if !handle.is_null() {
            unsafe { UnhookWindowsHookEx(handle) };
            crate::log::debug("mouse hook removed; nothing of ours is on the input path");
        }
    }
    update_active();
}

/// Suppression needs both hooks: one swallows keys, the other buttons and the wheel.
fn update_active() {
    let active =
        !KEYBOARD_HOOK.load(Relaxed).is_null() && !MOUSE_HOOK.load(Relaxed).is_null();
    input::set_hooks_active(active);
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
                    // Hidden from Windows and deliberately not forwarded: Raw Input delivers this
                    // button or wheel notch anyway, and sending it from here as well is what made
                    // every click arrive on the Mac twice.
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

