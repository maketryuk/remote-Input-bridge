//! The single Win32 window. It is the message pump for Raw Input, the owner of the tray icon,
//! and - when shown - the status and settings UI (spec §36, §38).

use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering::Relaxed};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, GetStockObject, GetSysColorBrush, COLOR_BTNFACE, DEFAULT_GUI_FONT, HFONT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::BST_CHECKED;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Config, MOUSE_INTERVAL_CHOICES_MS};
use crate::input::{hooks, raw_input};
use crate::net::NetMsg;
use crate::state::{state, LinkState, Target};
use crate::ui::{cmd, tray};

const WM_APP_REFRESH: u32 = WM_APP + 2;
const WM_APP_INPUT_HIDDEN: u32 = WM_APP + 3;
const REFRESH_TIMER: usize = 1;

const ID_STATUS: u32 = 200;
const ID_DIAG: u32 = 201;
const ID_ERROR: u32 = 202;
const ID_HOST: u32 = 203;
const ID_TCP: u32 = 204;
const ID_UDP: u32 = 205;
const ID_INTERVAL: u32 = 206;
const ID_EDGE: u32 = 207;
const ID_SUPPRESS: u32 = 208;
const ID_AUTOCONNECT: u32 = 209;
const ID_USE_UDP: u32 = 210;
const ID_DIAG_ON: u32 = 211;
const ID_PAIR_CODE: u32 = 212;
const ID_AUTOSTART: u32 = 213;
const ID_NAME: u32 = 214;
const ID_HOTKEY_MAC: u32 = 215;
const ID_HOTKEY_WINDOWS: u32 = 216;
const ID_HOTKEY_EMERGENCY: u32 = 217;
const ID_DIAG2: u32 = 218;
const ID_AUTO_UPDATE: u32 = 219;
const ID_UPDATE: u32 = 220;

const WIDTH: i32 = 470;
const HEIGHT: i32 = 758;

static MAIN_WINDOW: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Ordinal of the icon `build.rs` embeds. `winresource` names the first icon "1", and the shell
/// shows whichever icon has the lowest ordinal, so this is also the file's icon in Explorer.
const APP_ICON: u32 = 1;

/// Loads the embedded icon at the exact size the caller needs. `LoadIconW` would only ever return
/// the large size and let the tray shrink it, which looks visibly worse than the hand-drawn 16 px
/// frame in the .ico.
pub fn app_icon(size: IconSize) -> HICON {
    let metric = match size {
        IconSize::Small => (SM_CXSMICON, SM_CYSMICON),
        IconSize::Large => (SM_CXICON, SM_CYICON),
    };
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let icon = unsafe {
        LoadImageW(
            instance,
            APP_ICON as *const u16,
            IMAGE_ICON,
            GetSystemMetrics(metric.0),
            GetSystemMetrics(metric.1),
            LR_DEFAULTCOLOR,
        )
    };
    if icon.is_null() {
        // No embedded resource (a build where rc.exe was missing): the generic application icon
        // is ugly but keeps the window and the tray from having no icon at all.
        return unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) };
    }
    icon as HICON
}

#[derive(Clone, Copy)]
pub enum IconSize {
    Small,
    Large,
}

pub fn hwnd() -> HWND {
    MAIN_WINDOW.load(Relaxed)
}

/// Delivers a menu/button command to the window from any thread. Used by the updater to ask for
/// a clean shutdown once the installer has taken over.
pub fn post_command(command: u32) {
    let window = hwnd();
    if !window.is_null() {
        unsafe { PostMessageW(window, WM_COMMAND, command as WPARAM, 0) };
    }
}

/// Brings the already-running copy's window to the front. Called from a second instance, which
/// has no window of its own, so it goes through the window class rather than a stored handle.
pub fn show_other_instance() {
    let class = wide("RemoteInputBridgeWindow");
    let existing = unsafe { FindWindowW(class.as_ptr(), ptr::null()) };
    if !existing.is_null() {
        unsafe { PostMessageW(existing, WM_COMMAND, cmd::SHOW_WINDOW as WPARAM, 0) };
    }
}

/// Posted rather than acted on directly: a target switch can be decided on the network thread,
/// and windows may only be shown and focused from the thread that owns them.
pub fn post_input_hidden(hidden: bool) {
    let window = hwnd();
    if !window.is_null() {
        unsafe { PostMessageW(window, WM_APP_INPUT_HIDDEN, hidden as WPARAM, 0) };
    }
}

pub fn request_refresh() {
    let window = hwnd();
    if !window.is_null() {
        unsafe { PostMessageW(window, WM_APP_REFRESH, 0, 0) };
    }
}

pub fn show() {
    let window = hwnd();
    if !window.is_null() {
        unsafe {
            ShowWindow(window, SW_SHOW);
            SetForegroundWindow(window);
        }
        refresh_controls(window);
    }
}

pub fn create(with_tray: bool, visible: bool) -> HWND {
    let class = wide("RemoteInputBridgeWindow");
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let mut wc: WNDCLASSEXW = unsafe { std::mem::zeroed() };
    wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    wc.lpfnWndProc = Some(wnd_proc);
    wc.hInstance = instance;
    wc.lpszClassName = class.as_ptr();
    wc.hCursor = unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) };
    wc.hIcon = app_icon(IconSize::Large);
    wc.hIconSm = app_icon(IconSize::Small);
    wc.hbrBackground = unsafe { GetSysColorBrush(COLOR_BTNFACE as i32) };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        crate::log::error("RegisterClassExW failed");
        return ptr::null_mut();
    }

    // Fixed size: the layout is absolute, and a resizable window would only ever look broken.
    let style = (WS_OVERLAPPEDWINDOW & !WS_THICKFRAME & !WS_MAXIMIZEBOX) | WS_CLIPCHILDREN;
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            wide("Remote Input Bridge").as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            WIDTH,
            HEIGHT,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if window.is_null() {
        crate::log::error("CreateWindowExW failed");
        return window;
    }
    MAIN_WINDOW.store(window, Relaxed);

    if !raw_input::register(window) {
        crate::log::error("Raw Input registration failed; no input will be forwarded");
    }
    if with_tray {
        tray::add(window);
    }
    unsafe { SetTimer(window, REFRESH_TIMER, 500, None) };
    refresh_controls(window);
    if visible {
        show();
    }
    window
}

// ---------------------------------------------------------------------------
// Control helpers
// ---------------------------------------------------------------------------

fn control(parent: HWND, id: u32) -> HWND {
    unsafe { GetDlgItem(parent, id as i32) }
}

fn set_text(parent: HWND, id: u32, text: &str) {
    let handle = control(parent, id);
    if !handle.is_null() {
        unsafe { SetWindowTextW(handle, wide(text).as_ptr()) };
    }
}

fn get_text(parent: HWND, id: u32) -> String {
    let handle = control(parent, id);
    if handle.is_null() {
        return String::new();
    }
    let len = unsafe { GetWindowTextLengthW(handle) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let written = unsafe { GetWindowTextW(handle, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..written.max(0) as usize])
}

fn set_check(parent: HWND, id: u32, checked: bool) {
    let handle = control(parent, id);
    if !handle.is_null() {
        unsafe {
            SendMessageW(handle, BM_SETCHECK, if checked { 1 } else { 0 }, 0);
        }
    }
}

fn get_check(parent: HWND, id: u32) -> bool {
    let handle = control(parent, id);
    !handle.is_null()
        && unsafe { SendMessageW(handle, BM_GETCHECK, 0, 0) } == BST_CHECKED as isize
}

/// The font the rest of Windows uses for dialogs - Segoe UI on anything current. The stock
/// `DEFAULT_GUI_FONT` is still the 1995 bitmap face, which next to the themed controls the
/// manifest turns on looks like a different decade.
pub fn gui_font() -> HFONT {
    static FONT: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
    let cached = FONT.load(Relaxed);
    if !cached.is_null() {
        return cached as HFONT;
    }
    let mut metrics: NONCLIENTMETRICSW = unsafe { std::mem::zeroed() };
    metrics.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
    let font = unsafe {
        if SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            (&mut metrics as *mut NONCLIENTMETRICSW).cast(),
            0,
        ) == 0
        {
            GetStockObject(DEFAULT_GUI_FONT) as HFONT
        } else {
            CreateFontIndirectW(&metrics.lfMessageFont)
        }
    };
    FONT.store(font as *mut core::ffi::c_void, Relaxed);
    font
}

fn apply_font(handle: HWND) {
    unsafe { SendMessageW(handle, WM_SETFONT, gui_font() as WPARAM, 1) };
}

fn add_control(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: u32,
) -> HWND {
    let handle = unsafe {
        CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            w,
            h,
            parent,
            id as HMENU,
            GetModuleHandleW(ptr::null()),
            ptr::null(),
        )
    };
    if !handle.is_null() {
        apply_font(handle);
    }
    handle
}

fn build_controls(parent: HWND) {
    const LABEL_X: i32 = 14;
    const FIELD_X: i32 = 170;
    const LABEL_W: i32 = 150;
    const FIELD_W: i32 = 260;
    const ROW: i32 = 28;
    let edit_style = WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32;
    let check_style = WS_TABSTOP | BS_AUTOCHECKBOX as u32;
    let mut y = 12;

    add_control(parent, "STATIC", "", 0, LABEL_X, y, 430, 20, ID_STATUS);
    y += 22;
    add_control(parent, "STATIC", "", 0, LABEL_X, y, 430, 20, ID_DIAG);
    y += 20;
    add_control(parent, "STATIC", "", 0, LABEL_X, y, 430, 20, ID_DIAG2);
    y += 22;
    add_control(parent, "STATIC", "", 0, LABEL_X, y, 430, 34, ID_ERROR);
    y += 44;

    add_control(parent, "STATIC", "Mac IP or host name", 0, LABEL_X, y + 3, LABEL_W, 20, 0);
    add_control(parent, "EDIT", "", edit_style, FIELD_X, y, FIELD_W, 22, ID_HOST);
    y += ROW;
    add_control(parent, "STATIC", "TCP port (control)", 0, LABEL_X, y + 3, LABEL_W, 20, 0);
    add_control(
        parent,
        "EDIT",
        "",
        edit_style | ES_NUMBER as u32,
        FIELD_X,
        y,
        80,
        22,
        ID_TCP,
    );
    y += ROW;
    add_control(parent, "STATIC", "UDP port (movement)", 0, LABEL_X, y + 3, LABEL_W, 20, 0);
    add_control(
        parent,
        "EDIT",
        "",
        edit_style | ES_NUMBER as u32,
        FIELD_X,
        y,
        80,
        22,
        ID_UDP,
    );
    y += ROW;
    add_control(parent, "STATIC", "This device name", 0, LABEL_X, y + 3, LABEL_W, 20, 0);
    add_control(parent, "EDIT", "", edit_style, FIELD_X, y, FIELD_W, 22, ID_NAME);
    y += ROW;
    add_control(parent, "STATIC", "Mouse update interval", 0, LABEL_X, y + 3, LABEL_W, 20, 0);
    let combo = add_control(
        parent,
        "COMBOBOX",
        "",
        WS_TABSTOP | CBS_DROPDOWNLIST as u32,
        FIELD_X,
        y,
        120,
        140,
        ID_INTERVAL,
    );
    for ms in MOUSE_INTERVAL_CHOICES_MS {
        let label = wide(&format!("{ms} ms"));
        unsafe { SendMessageW(combo, CB_ADDSTRING, 0, label.as_ptr() as LPARAM) };
    }
    y += ROW;
    for (index, (label, field)) in [
        ("Switch to Mac hotkey", ID_HOTKEY_MAC),
        ("Switch to Windows hotkey", ID_HOTKEY_WINDOWS),
        ("Emergency hotkey", ID_HOTKEY_EMERGENCY),
    ]
    .into_iter()
    .enumerate()
    {
        add_control(parent, "STATIC", label, 0, LABEL_X, y + 3, LABEL_W, 20, 0);
        add_control(parent, "EDIT", "", edit_style, FIELD_X, y, 180, 22, field);
        add_control(
            parent,
            "BUTTON",
            "Record",
            WS_TABSTOP,
            FIELD_X + 186,
            y - 1,
            92,
            24,
            cmd::RECORD_BASE + index as u32,
        );
        y += ROW;
    }
    y += 8;

    for (id, text) in [
        (ID_EDGE, "Switch by screen edge (Mac is on the left)"),
        (ID_SUPPRESS, "Suppress local Windows input while the Mac is active"),
        (ID_AUTOCONNECT, "Connect automatically"),
        (ID_USE_UDP, "Send mouse movement over UDP (recommended)"),
        (ID_DIAG_ON, "Write the diagnostics line to the log every second"),
        (ID_AUTOSTART, "Start with Windows"),
        (ID_AUTO_UPDATE, "Check for updates automatically"),
    ] {
        add_control(parent, "BUTTON", text, check_style, LABEL_X, y, 420, 22, id);
        y += 24;
    }
    y += 12;

    add_control(parent, "STATIC", "Pairing code from the Mac", 0, LABEL_X, y + 3, LABEL_W, 20, 0);
    add_control(parent, "EDIT", "", edit_style, FIELD_X, y, 140, 22, ID_PAIR_CODE);
    add_control(parent, "BUTTON", "Pair", WS_TABSTOP, FIELD_X + 150, y - 1, 100, 24, cmd::PAIR);
    y += 36;

    add_control(parent, "BUTTON", "Save and apply", WS_TABSTOP, LABEL_X, y, 150, 26, cmd::SAVE);
    add_control(parent, "BUTTON", "Reconnect", WS_TABSTOP, LABEL_X + 160, y, 130, 26, cmd::RECONNECT);
    add_control(
        parent,
        "BUTTON",
        "Config folder",
        WS_TABSTOP,
        LABEL_X + 300,
        y,
        130,
        26,
        cmd::OPEN_CONFIG_DIR,
    );
    y += 34;
    add_control(
        parent,
        "BUTTON",
        "Switch input to Mac",
        WS_TABSTOP,
        LABEL_X,
        y,
        200,
        26,
        cmd::SWITCH_TO_MAC,
    );
    add_control(
        parent,
        "BUTTON",
        "Switch input to Windows",
        WS_TABSTOP,
        LABEL_X + 210,
        y,
        220,
        26,
        cmd::SWITCH_TO_WINDOWS,
    );
    y += 42;
    add_control(parent, "STATIC", "", 0, LABEL_X, y, 430, 20, ID_UPDATE);
    y += 22;
    add_control(
        parent,
        "BUTTON",
        "Check for updates",
        WS_TABSTOP,
        LABEL_X,
        y,
        170,
        26,
        cmd::CHECK_UPDATES,
    );
}

fn refresh_controls(parent: HWND) {
    if parent.is_null() {
        return;
    }
    let st = state();
    let cfg = st.config();
    set_text(parent, ID_HOST, &cfg.mac_host);
    set_text(parent, ID_TCP, &cfg.tcp_port.to_string());
    set_text(parent, ID_UDP, &cfg.udp_port.to_string());
    set_text(parent, ID_NAME, &cfg.device_name);
    set_text(parent, ID_HOTKEY_MAC, &cfg.hotkey_switch_to_mac);
    set_text(parent, ID_HOTKEY_WINDOWS, &cfg.hotkey_switch_to_windows);
    set_text(parent, ID_HOTKEY_EMERGENCY, &cfg.hotkey_emergency_local);
    let index = MOUSE_INTERVAL_CHOICES_MS
        .iter()
        .position(|ms| *ms == cfg.mouse_interval_ms)
        .unwrap_or(1);
    let combo = control(parent, ID_INTERVAL);
    if !combo.is_null() {
        unsafe { SendMessageW(combo, CB_SETCURSEL, index, 0) };
    }
    set_check(parent, ID_EDGE, cfg.edge_switch);
    set_check(parent, ID_SUPPRESS, cfg.suppress_local_input);
    set_check(parent, ID_AUTOCONNECT, cfg.auto_connect);
    set_check(parent, ID_USE_UDP, cfg.use_udp);
    set_check(parent, ID_DIAG_ON, cfg.diagnostics);
    set_check(parent, ID_AUTO_UPDATE, cfg.auto_check_updates);
    // Read from the registry rather than the config file: the installer can add the Run entry on
    // its own, and a checkbox that disagrees with what Windows will actually do is worse than no
    // checkbox at all.
    set_check(parent, ID_AUTOSTART, crate::autostart::is_enabled());
    refresh_status(parent);
}

fn refresh_status(parent: HWND) {
    let st = state();
    let snapshot = st.tel.snapshot();
    let status = st.status_line();
    set_text(parent, ID_STATUS, &status);
    set_text(
        parent,
        ID_DIAG,
        &format!(
            "in {:.0} Hz | out {:.0} Hz | loss {:.1}% | rtt {:.2} ms | jitter {:.2} ms",
            snapshot.raw_mouse_hz,
            snapshot.udp_send_hz,
            snapshot.loss_percent,
            snapshot.rtt_ms,
            snapshot.jitter_ms
        ),
    );
    set_text(
        parent,
        ID_DIAG2,
        &format!(
            "raw input {:.0} Hz | keys {:.0} Hz | reliable {:.0} Hz | mac events {:.0} Hz | reconnects {}",
            snapshot.raw_mouse_hz,
            snapshot.raw_kbd_hz,
            snapshot.reliable_hz,
            snapshot.remote_event_hz,
            snapshot.reconnects
        ),
    );
    let info = st.status.lock().unwrap();
    let message = if info.pairing_required {
        format!("Pairing required. {}", info.last_error)
    } else if !info.last_error.is_empty() && st.link() != LinkState::Connected {
        info.last_error.clone()
    } else {
        let cfg = st.config();
        format!(
            "Switch to Mac: {}    Switch to Windows: {}    Emergency: {}",
            cfg.hotkey_switch_to_mac, cfg.hotkey_switch_to_windows, cfg.hotkey_emergency_local
        )
    };
    drop(info);
    set_text(parent, ID_ERROR, &message);

    if let Some((control, text)) = crate::input::take_capture() {
        set_text(parent, control, &text);
    }
    for (index, field) in [ID_HOTKEY_MAC, ID_HOTKEY_WINDOWS, ID_HOTKEY_EMERGENCY]
        .into_iter()
        .enumerate()
    {
        let recording = crate::input::capture_target() == Some(field);
        set_text(
            parent,
            cmd::RECORD_BASE + index as u32,
            if recording { "Press a key..." } else { "Record" },
        );
    }

    set_text(parent, ID_UPDATE, &crate::update::summary());
    let button = control(parent, cmd::CHECK_UPDATES);
    if !button.is_null() {
        let (label, enabled) = match crate::update::stage() {
            crate::update::Stage::Available(version) => (format!("Install {version}"), true),
            crate::update::Stage::Checking => ("Checking...".to_string(), false),
            crate::update::Stage::Downloading(_) | crate::update::Stage::Installing => {
                ("Updating...".to_string(), false)
            }
            _ => ("Check for updates".to_string(), true),
        };
        set_text(parent, cmd::CHECK_UPDATES, &label);
        unsafe { EnableWindow(button, i32::from(enabled)) };
    }

    if !hwnd().is_null() {
        tray::update(hwnd(), &format!("Remote Input Bridge - {status}"));
    }
}

fn save_settings(parent: HWND) {
    let st = state();
    let previous = st.config();
    let mut cfg = Config {
        mac_host: get_text(parent, ID_HOST).trim().to_string(),
        device_name: get_text(parent, ID_NAME).trim().to_string(),
        edge_switch: get_check(parent, ID_EDGE),
        suppress_local_input: get_check(parent, ID_SUPPRESS),
        auto_connect: get_check(parent, ID_AUTOCONNECT),
        use_udp: get_check(parent, ID_USE_UDP),
        diagnostics: get_check(parent, ID_DIAG_ON),
        auto_check_updates: get_check(parent, ID_AUTO_UPDATE),
        start_with_system: get_check(parent, ID_AUTOSTART),
        ..previous.clone()
    };
    if let Ok(port) = get_text(parent, ID_TCP).trim().parse::<u16>() {
        if port != 0 {
            cfg.tcp_port = port;
        }
    }
    if let Ok(port) = get_text(parent, ID_UDP).trim().parse::<u16>() {
        if port != 0 {
            cfg.udp_port = port;
        }
    }
    for (id, field, label) in [
        (ID_HOTKEY_MAC, 0usize, "Switch to Mac"),
        (ID_HOTKEY_WINDOWS, 1, "Switch to Windows"),
        (ID_HOTKEY_EMERGENCY, 2, "Emergency"),
    ] {
        let text = get_text(parent, id).trim().to_string();
        if text.is_empty() {
            continue;
        }
        if crate::config::parse_hotkey(&text).is_none() {
            message_box(&format!(
                "\"{text}\" is not a hotkey this build understands.\n\n\
                 Use modifiers Ctrl, Alt, Shift, Win plus one key, for example:\n\
                 Ctrl+Alt+Left, Ctrl+Alt+Shift+Escape, Win+F5.\n\n\
                 Keeping the previous {label} hotkey."
            ));
            continue;
        }
        match field {
            0 => cfg.hotkey_switch_to_mac = text,
            1 => cfg.hotkey_switch_to_windows = text,
            _ => cfg.hotkey_emergency_local = text,
        }
    }
    let combo = control(parent, ID_INTERVAL);
    if !combo.is_null() {
        let index = unsafe { SendMessageW(combo, CB_GETCURSEL, 0, 0) };
        if index >= 0 && (index as usize) < MOUSE_INTERVAL_CHOICES_MS.len() {
            cfg.mouse_interval_ms = MOUSE_INTERVAL_CHOICES_MS[index as usize];
        }
    }

    let cfg = cfg.sanitized();
    // Compared against the registry, not against the previous config: the two can legitimately
    // disagree after an install, and the checkbox is what the user just looked at.
    if cfg.start_with_system != crate::autostart::is_enabled() {
        if let Err(e) = crate::autostart::set(cfg.start_with_system) {
            crate::log::warn(&format!("could not change the autostart entry: {e}"));
        }
    }
    crate::log::set_level(&cfg.log_level);
    crate::input::set_hotkeys(&cfg);
    if st.target() == Target::RemoteMac {
        st.suppress.store(cfg.suppress_local_input, Relaxed);
    }
    st.set_config(cfg.clone());
    if let Err(e) = cfg.save() {
        crate::log::warn(&format!("could not write config.json: {e}"));
    }
    st.send(NetMsg::ConfigChanged);
    refresh_controls(parent);
}

fn handle_command(parent: HWND, id: u32) {
    let st = state();
    match id {
        cmd::SHOW_WINDOW => show(),
        cmd::SWITCH_TO_MAC => {
            st.request_target(Target::RemoteMac);
        }
        cmd::SWITCH_TO_WINDOWS | cmd::FORCE_LOCAL => {
            st.force_local("requested from the UI");
        }
        cmd::RECONNECT => st.send(NetMsg::Reconnect),
        cmd::SAVE => save_settings(parent),
        cmd::PAIR => {
            let code = get_text(parent, ID_PAIR_CODE).trim().to_string();
            if code.is_empty() {
                message_box("Enter the pairing code shown on the Mac first.");
            } else {
                save_settings(parent);
                st.send(NetMsg::Pair(code));
                set_text(parent, ID_PAIR_CODE, "");
            }
        }
        _ if (cmd::RECORD_BASE..cmd::RECORD_BASE + 3).contains(&id) => {
            let field = [ID_HOTKEY_MAC, ID_HOTKEY_WINDOWS, ID_HOTKEY_EMERGENCY]
                [(id - cmd::RECORD_BASE) as usize];
            if crate::input::capture_target() == Some(field) {
                crate::input::cancel_capture();
            } else {
                // Press the combination you want; whatever the keyboard actually sends is what
                // gets written down. For anything behind an Fn key that is the only way to know.
                crate::input::begin_capture(field);
            }
            refresh_status(parent);
        }
        cmd::OPEN_CONFIG_DIR => open_config_dir(),
        cmd::CHECK_UPDATES => {
            // The same button installs what a previous check found, so the common case is one
            // click to look and one click to update.
            if crate::update::update_ready() {
                crate::update::install();
            } else {
                crate::update::check(true);
            }
            refresh_status(parent);
        }
        cmd::TOGGLE_EDGE => {
            let mut cfg = st.config();
            cfg.edge_switch = !cfg.edge_switch;
            let enabled = cfg.edge_switch;
            let _ = cfg.save();
            st.set_config(cfg);
            crate::log::info(&format!("edge switching {}", if enabled { "on" } else { "off" }));
            refresh_controls(parent);
        }
        cmd::QUIT => unsafe {
            DestroyWindow(parent);
        },
        _ if (cmd::INTERVAL_BASE..cmd::INTERVAL_BASE + 16).contains(&id) => {
            let index = (id - cmd::INTERVAL_BASE) as usize;
            if let Some(ms) = MOUSE_INTERVAL_CHOICES_MS.get(index).copied() {
                {
                    let mut cfg = st.config();
                    cfg.mouse_interval_ms = ms;
                    let _ = cfg.save();
                    st.set_config(cfg);
                }
                crate::log::info(&format!("mouse interval set to {ms} ms"));
                st.send(NetMsg::ConfigChanged);
                refresh_controls(parent);
            }
        }
        _ => {}
    }
}

fn message_box(text: &str) {
    unsafe {
        MessageBoxW(
            hwnd(),
            wide(text).as_ptr(),
            wide("Remote Input Bridge").as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        )
    };
}

fn open_config_dir() {
    let dir = crate::config::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
}

unsafe extern "system" fn wnd_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            build_controls(window);
            0
        }
        WM_INPUT => {
            // The whole reason this window exists: drain Raw Input as fast as it arrives.
            raw_input::on_wm_input(lparam);
            DefWindowProcW(window, message, wparam, lparam)
        }
        WM_COMMAND => {
            handle_command(window, (wparam & 0xffff) as u32);
            0
        }
        WM_APP_REFRESH => {
            refresh_status(window);
            0
        }
        WM_APP_INPUT_HIDDEN => {
            if wparam != 0 {
                super::banner::show();
            } else {
                super::banner::hide();
            }
            // Here rather than at the switch itself: a low-level hook is delivered to the thread
            // that installed it, and this is the only thread with a message loop.
            hooks::apply_mouse_hook();
            0
        }
        WM_TIMER => {
            if wparam == REFRESH_TIMER {
                refresh_status(window);
                // The system clears the clip on focus changes, so it has to be re-applied.
                hooks::refresh_cursor_clip();
                // Idempotent, and cheap when nothing has changed: whatever route altered the
                // target or the settings, the hooks converge on what is actually needed.
                hooks::apply_mouse_hook();
            }
            0
        }
        WM_DISPLAYCHANGE => {
            hooks::refresh_screen_bounds();
            0
        }
        WM_POWERBROADCAST => {
            if wparam as u32 == PBT_APMRESUMEAUTOMATIC {
                // Spec §43: after wake the adapter may have a new address; force a fresh attempt
                // instead of waiting for the backoff to expire.
                crate::log::info("system resumed; reconnecting");
                state().force_local("system resume");
                state().send(NetMsg::Reconnect);
            }
            1
        }
        tray::TRAY_CALLBACK_MSG => {
            match (lparam as u32) & 0xffff {
                WM_RBUTTONUP | WM_CONTEXTMENU => tray::show_menu(window),
                WM_LBUTTONDBLCLK => show(),
                WM_LBUTTONUP => tray::show_menu(window),
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            // Closing the window only hides it; the bridge keeps running in the tray.
            ShowWindow(window, SW_HIDE);
            0
        }
        WM_DESTROY => {
            tray::remove(window);
            PostQuitMessage(0);
            0
        }
        WM_SETFOCUS => {
            let host = control(window, ID_HOST);
            if !host.is_null() {
                SetFocus(host);
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}
