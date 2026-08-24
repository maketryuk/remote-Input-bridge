//! The small always-on-top window shown while the Mac owns the input.
//!
//! Its real job is to hold the keyboard focus. An application that reads Raw Input or DirectInput
//! directly - which is most games - cannot be hidden from by any low-level hook: swallowing an
//! event in a hook would drop it for this process too, taking with it the 1000 raw deltas a second
//! that make the remote cursor smooth. But virtually every game ignores input while it is not the
//! foreground window, so taking the foreground is the one thing in user space that stops the
//! character running around while the mouse is driving the Mac.
//!
//! It doubles as the only visible sign on the Windows screen that the input has gone somewhere
//! else, which a tray icon Windows 11 hides in the overflow flyout is not.
//!
//! The window that had the focus is remembered and given it back on the way out, so switching to
//! the Mac and back leaves the desktop exactly as it was - the editor you were typing in is still
//! the one you are typing in.

use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering::Relaxed};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, InvalidateRect,
    SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_SINGLELINE, DT_VCENTER, PAINTSTRUCT,
    TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WIDTH: i32 = 360;
const HEIGHT: i32 = 76;

/// 0x00BBGGRR, not RGB.
const BACKGROUND: u32 = 0x0026_1C15;
const TITLE_COLOUR: u32 = 0x00FF_FFFF;
const HINT_COLOUR: u32 = 0x00C8_B4A8;

static BANNER: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
/// Whatever had the focus when the input left, so it can have it back.
static PREVIOUS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ensure() -> HWND {
    let existing = BANNER.load(Relaxed);
    if !existing.is_null() {
        return existing;
    }
    let class = wide("RemoteInputBridgeBanner");
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let mut wc: WNDCLASSEXW = unsafe { std::mem::zeroed() };
    wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    wc.lpfnWndProc = Some(banner_proc);
    wc.hInstance = instance;
    wc.lpszClassName = class.as_ptr();
    wc.hCursor = unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) };
    // A second registration of the same class fails harmlessly; the window creation below is what
    // actually decides whether this worked.
    unsafe { RegisterClassExW(&wc) };

    let x = (unsafe { GetSystemMetrics(SM_CXSCREEN) } - WIDTH) / 2;
    let y = unsafe { GetSystemMetrics(SM_CYSCREEN) } / 12;
    let window = unsafe {
        CreateWindowExW(
            // TOOLWINDOW keeps it out of the task bar and out of Alt+Tab; TOPMOST puts it over a
            // borderless full screen game.
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            wide("Remote Input Bridge").as_ptr(),
            WS_POPUP,
            x,
            y,
            WIDTH,
            HEIGHT,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if window.is_null() {
        crate::log::warn("could not create the status banner; games may keep reading the mouse");
    }
    BANNER.store(window, Relaxed);
    window
}

/// Shows the banner and takes the foreground away from whatever is running.
pub fn show() {
    let banner = ensure();
    if banner.is_null() {
        return;
    }
    let previous = unsafe { GetForegroundWindow() };
    if !previous.is_null() && previous != banner {
        PREVIOUS.store(previous, Relaxed);
    }
    unsafe {
        ShowWindow(banner, SW_SHOW);
        SetWindowPos(
            banner,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        // Permitted because this process received the last input event - the hotkey that got us
        // here. Nothing to do if the system refuses beyond saying so.
        if SetForegroundWindow(banner) == 0 {
            crate::log::debug("the system refused to move the focus to the status banner");
        }
        InvalidateRect(banner, ptr::null(), 1);
    }
}

/// Hides it and hands the focus back to the window that had it.
pub fn hide() {
    let banner = BANNER.load(Relaxed);
    if banner.is_null() {
        return;
    }
    unsafe { ShowWindow(banner, SW_HIDE) };
    let previous = PREVIOUS.swap(ptr::null_mut(), Relaxed);
    if !previous.is_null() && unsafe { IsWindow(previous) } != 0 {
        unsafe { SetForegroundWindow(previous) };
    }
}

unsafe extern "system" fn banner_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_PAINT => {
            paint(window);
            0
        }
        // Never destroyed by a stray close: the app owns its lifetime.
        WM_CLOSE => {
            ShowWindow(window, SW_HIDE);
            0
        }
        WM_DESTROY => {
            BANNER.store(ptr::null_mut(), Relaxed);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

unsafe fn paint(window: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(window, &mut ps);
    let mut area = RECT { left: 0, top: 0, right: WIDTH, bottom: HEIGHT };
    let brush = CreateSolidBrush(BACKGROUND);
    FillRect(dc, &area, brush);
    DeleteObject(brush as _);

    SetBkMode(dc, TRANSPARENT as i32);
    let font = super::window::gui_font();
    let previous_font = SelectObject(dc, font as _);

    SetTextColor(dc, TITLE_COLOUR);
    area.top = 14;
    area.bottom = 38;
    let title = wide("Input is on the Mac");
    DrawTextW(dc, title.as_ptr(), -1, &mut area, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    SetTextColor(dc, HINT_COLOUR);
    area.top = 38;
    area.bottom = 64;
    let hint = wide(&format!(
        "{} brings it back to Windows",
        crate::state::state().config().hotkey_switch_to_windows
    ));
    DrawTextW(dc, hint.as_ptr(), -1, &mut area, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    SelectObject(dc, previous_font);
    EndPaint(window, &ps);
}
