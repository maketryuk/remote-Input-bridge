//! Tray icon and its context menu.

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, PostMessageW, RegisterWindowMessageW,
    SetForegroundWindow, TrackPopupMenu, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_NULL,
};

use crate::config::MOUSE_INTERVAL_CHOICES_MS;
use crate::state::{state, LinkState, Target};
use crate::ui::cmd;

pub const TRAY_ID: u32 = 1;
pub const TRAY_CALLBACK_MSG: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 1;

static PRESENT: AtomicBool = AtomicBool::new(false);
/// Id of the shell's "TaskbarCreated" broadcast, resolved once.
static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The message the shell broadcasts to every top-level window when it creates the task bar - at
/// logon, and again whenever Explorer restarts.
///
/// This matters most for an app started from the Run key: it can easily be running before the
/// shell is, in which case the icon it added belongs to a task bar that no longer exists. The
/// entry left behind looks like an icon and answers no clicks, which is exactly what "the tray
/// icon is broken and so is its menu" looks like from the outside.
pub fn taskbar_created_message() -> u32 {
    let cached = TASKBAR_CREATED.load(Relaxed);
    if cached != 0 {
        return cached;
    }
    let name: Vec<u16> = "TaskbarCreated".encode_utf16().chain(std::iter::once(0)).collect();
    let id = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    TASKBAR_CREATED.store(id, Relaxed);
    id
}

/// Puts the icon back after the shell has replaced the task bar under us.
pub fn readd(hwnd: HWND) {
    // Removing first is deliberate: the old entry may or may not still exist, and NIM_DELETE on
    // something that is already gone is harmless, while NIM_ADD over a live entry is not.
    let data = base(hwnd);
    unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    PRESENT.store(false, Relaxed);
    add(hwnd);
    crate::log::info("the shell rebuilt the task bar; the tray icon was added again");
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn base(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ID;
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = TRAY_CALLBACK_MSG;
    data.hIcon = super::window::app_icon(super::window::IconSize::Small);
    data
}

fn set_tip(data: &mut NOTIFYICONDATAW, text: &str) {
    let encoded: Vec<u16> = text.encode_utf16().take(data.szTip.len() - 1).collect();
    data.szTip[..encoded.len()].copy_from_slice(&encoded);
    data.szTip[encoded.len()] = 0;
}

pub fn add(hwnd: HWND) {
    let mut data = base(hwnd);
    set_tip(&mut data, "Remote Input Bridge");
    if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
        // Not a warning: at logon this simply means the shell is not up yet, and the
        // TaskbarCreated broadcast will tell us when it is.
        crate::log::info("no tray icon yet; waiting for the shell to announce the task bar");
    } else {
        PRESENT.store(true, Relaxed);
    }
}

/// Called from the UI timer, which makes it the natural place to keep trying: an icon that could
/// not be added at logon, or one the shell dropped without announcing it, comes back within half a
/// second instead of staying missing until the app is restarted.
pub fn update(hwnd: HWND, tooltip: &str) {
    if !PRESENT.load(Relaxed) {
        add(hwnd);
        return;
    }
    let mut data = base(hwnd);
    set_tip(&mut data, tooltip);
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
        // The entry is gone even though we believe it is there. Say so once and let the next tick
        // add it back.
        PRESENT.store(false, Relaxed);
        crate::log::info("the tray icon disappeared; adding it again");
    }
}

pub fn remove(hwnd: HWND) {
    if PRESENT.swap(false, Relaxed) {
        let data = base(hwnd);
        unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    }
}

pub fn show_menu(hwnd: HWND) {
    let st = state();
    let cfg = st.config();
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    let connected = st.link() == LinkState::Connected;
    let remote = st.target() == Target::RemoteMac;

    unsafe {
        let item = |flags: u32, id: u32, text: &str| {
            AppendMenuW(menu, flags, id as usize, wide(text).as_ptr());
        };
        item(MF_STRING | MF_GRAYED, 0, &format!("Remote Input Bridge - {}", st.link().label()));
        item(MF_STRING | MF_GRAYED, 0, &format!("Active input: {}", st.target().label()));
        item(MF_SEPARATOR, 0, "");
        item(
            MF_STRING | if connected && !remote { 0 } else { MF_GRAYED },
            cmd::SWITCH_TO_MAC,
            &format!("Switch to Mac\t{}", cfg.hotkey_switch_to_mac),
        );
        item(
            MF_STRING | if remote { 0 } else { MF_GRAYED },
            cmd::SWITCH_TO_WINDOWS,
            &format!("Switch to Windows\t{}", cfg.hotkey_switch_to_windows),
        );
        item(MF_SEPARATOR, 0, "");
        for (index, ms) in MOUSE_INTERVAL_CHOICES_MS.iter().enumerate() {
            let checked = if *ms == cfg.mouse_interval_ms { MF_CHECKED } else { 0 };
            item(
                MF_STRING | checked,
                cmd::INTERVAL_BASE + index as u32,
                &format!("Mouse interval: {ms} ms"),
            );
        }
        item(MF_SEPARATOR, 0, "");
        item(
            MF_STRING | if cfg.edge_switch { MF_CHECKED } else { 0 },
            cmd::TOGGLE_EDGE,
            "Switch by screen edge",
        );
        item(MF_STRING, cmd::SHOW_WINDOW, "Status and settings...");
        item(MF_STRING, cmd::RECONNECT, "Reconnect now");
        item(MF_STRING, cmd::OPEN_CONFIG_DIR, "Open config folder");
        item(
            MF_STRING,
            cmd::CHECK_UPDATES,
            &if let crate::update::Stage::Available(version) = crate::update::stage() {
                format!("Install version {version}...")
            } else {
                "Check for updates...".to_string()
            },
        );
        item(MF_SEPARATOR, 0, "");
        item(MF_STRING, cmd::QUIT, "Quit");

        // A tray menu only dismisses correctly if the owner window is foreground first, and only
        // behaves afterwards if the owner is given a message to chew on - without the WM_NULL the
        // menu can survive the click that should have closed it. Both halves are needed; this app
        // usually has no visible window, which is the case they were documented for.
        SetForegroundWindow(hwnd);
        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}
