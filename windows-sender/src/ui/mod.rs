//! Tray + status/settings window (spec §36, §38). On non-Windows targets everything here is a
//! no-op so the portable modules stay unit-testable.

#[cfg(windows)]
pub mod banner;
#[cfg(windows)]
pub mod tray;
#[cfg(windows)]
pub mod window;

/// Command ids shared by the tray menu and the window's buttons.
pub mod cmd {
    pub const SHOW_WINDOW: u32 = 100;
    pub const SWITCH_TO_MAC: u32 = 101;
    pub const SWITCH_TO_WINDOWS: u32 = 102;
    pub const RECONNECT: u32 = 103;
    pub const PAIR: u32 = 104;
    pub const SAVE: u32 = 105;
    pub const QUIT: u32 = 106;
    pub const OPEN_CONFIG_DIR: u32 = 107;
    pub const TOGGLE_EDGE: u32 = 108;
    pub const FORCE_LOCAL: u32 = 109;
    pub const CHECK_UPDATES: u32 = 110;
    /// `RECORD_BASE + index` into the three hotkey fields, in the order they appear.
    pub const RECORD_BASE: u32 = 111;
    /// `INTERVAL_BASE + index` into [`crate::config::MOUSE_INTERVAL_CHOICES_MS`].
    pub const INTERVAL_BASE: u32 = 120;
}

/// Ask the UI to redraw. Safe to call from any thread.
#[cfg(windows)]
pub fn refresh() {
    window::request_refresh();
}

#[cfg(not(windows))]
pub fn refresh() {}

/// Tell the UI that the input has moved. `hidden` is true while the Mac owns it *and* local
/// suppression is on - the case where Windows should stop reacting, and so the case where the
/// banner takes the focus away from whatever was running. Safe to call from any thread.
#[cfg(windows)]
pub fn input_hidden_from_windows(hidden: bool) {
    window::post_input_hidden(hidden);
}

#[cfg(not(windows))]
pub fn input_hidden_from_windows(_hidden: bool) {}

/// Ask the app to shut down through the normal path - hooks uninstalled, input handed back to
/// Windows, the Mac told - rather than exiting from under itself. Safe to call from any thread.
#[cfg(windows)]
pub fn quit() {
    window::post_command(cmd::QUIT);
}

#[cfg(not(windows))]
pub fn quit() {}
