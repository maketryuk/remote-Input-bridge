//! Tray icon and its context menu.

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, SetForegroundWindow,
    TrackPopupMenu, IDI_APPLICATION, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    TPM_LEFTALIGN, TPM_RIGHTBUTTON,
};

use crate::config::MOUSE_INTERVAL_CHOICES_MS;
use crate::state::{state, LinkState, Target};
use crate::ui::cmd;

pub const TRAY_ID: u32 = 1;
pub const TRAY_CALLBACK_MSG: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 1;

static PRESENT: AtomicBool = AtomicBool::new(false);

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
    data.hIcon = unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) };
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
        crate::log::warn("could not create the tray icon");
    } else {
        PRESENT.store(true, Relaxed);
    }
}

pub fn update(hwnd: HWND, tooltip: &str) {
    if !PRESENT.load(Relaxed) {
        return;
    }
    let mut data = base(hwnd);
    set_tip(&mut data, tooltip);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
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
        item(MF_SEPARATOR, 0, "");
        item(MF_STRING, cmd::QUIT, "Quit");

        // A tray menu only dismisses correctly if the owner window is foreground first.
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
        DestroyMenu(menu);
    }
}
