//! Input tracking that is shared by the Raw Input path and the low-level hooks.
//!
//! The rules that keep keys from sticking (spec §51) live here, in portable code, so they can be
//! unit-tested without a Windows box: a key or button that was physically held across a target
//! switch is marked *stale*, and its release is never forwarded — the remote side was told to
//! release everything at the moment of the switch, so an unpaired release would be a phantom.

pub mod keymap;

#[cfg(windows)]
pub mod hooks;
#[cfg(windows)]
pub mod raw_input;

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, RwLock};

use crate::config::{mods_match, parse_hotkey, Config, Hotkey};
use crate::protocol::button;
use crate::state::{state, Target};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    SwitchToMac,
    SwitchToWindows,
    ForceLocal,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Hotkeys {
    pub to_mac: Option<Hotkey>,
    pub to_windows: Option<Hotkey>,
    pub emergency: Option<Hotkey>,
}

impl Hotkeys {
    pub fn from_config(cfg: &Config) -> Hotkeys {
        Hotkeys {
            to_mac: parse_hotkey(&cfg.hotkey_switch_to_mac),
            to_windows: parse_hotkey(&cfg.hotkey_switch_to_windows),
            emergency: parse_hotkey(&cfg.hotkey_emergency_local),
        }
    }

    /// The emergency shortcut is checked first so a mis-configured pair of switch hotkeys can
    /// never shadow it (spec §19).
    pub fn match_vk(&self, vk: u16, mods: u16) -> Option<HotkeyAction> {
        let hit = |hk: &Option<Hotkey>| {
            hk.map(|h| h.vk == vk && mods_match(h.mods, mods)).unwrap_or(false)
        };
        if hit(&self.emergency) {
            Some(HotkeyAction::ForceLocal)
        } else if hit(&self.to_mac) {
            Some(HotkeyAction::SwitchToMac)
        } else if hit(&self.to_windows) {
            Some(HotkeyAction::SwitchToWindows)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDecision {
    /// Forward to the receiver; `repeat` marks an auto-repeat press.
    Forward { repeat: bool },
    /// Consume silently (stale release, synthetic key, or a hotkey we own).
    Drop,
    /// A configured hotkey fired. Never forwarded (spec §16).
    Hotkey(HotkeyAction),
}

pub struct InputTracker {
    keys_down: [bool; 256],
    keys_stale: [bool; 256],
    buttons_down: [bool; button::COUNT],
    buttons_stale: [bool; button::COUNT],
    pub modifiers: u16,
}

impl Default for InputTracker {
    fn default() -> Self {
        Self {
            keys_down: [false; 256],
            keys_stale: [false; 256],
            buttons_down: [false; button::COUNT],
            buttons_stale: [false; button::COUNT],
            modifiers: 0,
        }
    }
}

impl InputTracker {
    pub fn on_key(&mut self, hid: u16, down: bool, hotkeys: &Hotkeys) -> KeyDecision {
        if hid == 0 || hid as usize >= self.keys_down.len() {
            return KeyDecision::Drop;
        }
        let idx = hid as usize;
        let bit = keymap::modifier_bit(hid);
        if down {
            let repeat = self.keys_down[idx];
            self.keys_down[idx] = true;
            if bit != 0 {
                self.modifiers |= bit;
            } else {
                let vk = keymap::hid_to_vk(hid);
                if vk != 0 {
                    if let Some(action) = hotkeys.match_vk(vk, self.modifiers) {
                        // Swallow the matching release too, so the hotkey never leaks as a
                        // lone key-up on either machine.
                        self.keys_stale[idx] = true;
                        return KeyDecision::Hotkey(action);
                    }
                }
            }
            if self.keys_stale[idx] {
                KeyDecision::Drop
            } else {
                KeyDecision::Forward { repeat }
            }
        } else {
            self.keys_down[idx] = false;
            if bit != 0 {
                self.modifiers &= !bit;
            }
            if self.keys_stale[idx] {
                self.keys_stale[idx] = false;
                KeyDecision::Drop
            } else {
                KeyDecision::Forward { repeat: false }
            }
        }
    }

    pub fn on_button(&mut self, index: usize, down: bool) -> KeyDecision {
        if index >= button::COUNT {
            return KeyDecision::Drop;
        }
        if down {
            self.buttons_down[index] = true;
            if self.buttons_stale[index] {
                KeyDecision::Drop
            } else {
                KeyDecision::Forward { repeat: false }
            }
        } else {
            self.buttons_down[index] = false;
            if self.buttons_stale[index] {
                self.buttons_stale[index] = false;
                KeyDecision::Drop
            } else {
                KeyDecision::Forward { repeat: false }
            }
        }
    }

    /// Everything physically held at the moment of a switch becomes stale in both directions:
    /// the machine losing focus is told to release all, and the machine gaining focus never saw
    /// the press.
    pub fn mark_all_stale(&mut self) {
        for i in 0..self.keys_down.len() {
            if self.keys_down[i] {
                self.keys_stale[i] = true;
            }
        }
        for i in 0..button::COUNT {
            if self.buttons_down[i] {
                self.buttons_stale[i] = true;
            }
        }
    }

}

static TRACKER: Mutex<Option<InputTracker>> = Mutex::new(None);
static HOTKEYS: RwLock<Hotkeys> =
    RwLock::new(Hotkeys { to_mac: None, to_windows: None, emergency: None });
/// True once the low-level hooks are live. When they are, they own hotkey dispatch (they can
/// also swallow the keystroke); otherwise the Raw Input path dispatches instead, so hotkeys keep
/// working even if hook installation was refused.
static HOOKS_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn with_tracker<R>(f: impl FnOnce(&mut InputTracker) -> R) -> R {
    let mut guard = TRACKER.lock().unwrap();
    f(guard.get_or_insert_with(InputTracker::default))
}

pub fn hotkeys() -> Hotkeys {
    *HOTKEYS.read().unwrap()
}

pub fn set_hotkeys(cfg: &Config) {
    let parsed = Hotkeys::from_config(cfg);
    if parsed.to_mac.is_none() {
        crate::log::warn(&format!("unparsable hotkey: {}", cfg.hotkey_switch_to_mac));
    }
    if parsed.to_windows.is_none() {
        crate::log::warn(&format!("unparsable hotkey: {}", cfg.hotkey_switch_to_windows));
    }
    if parsed.emergency.is_none() {
        crate::log::warn(&format!("unparsable hotkey: {}", cfg.hotkey_emergency_local));
    }
    *HOTKEYS.write().unwrap() = parsed;
}

pub fn hooks_active() -> bool {
    HOOKS_ACTIVE.load(Relaxed)
}

pub fn set_hooks_active(active: bool) {
    HOOKS_ACTIVE.store(active, Relaxed);
}

pub fn modifiers() -> u16 {
    with_tracker(|t| t.modifiers)
}

pub fn dispatch(action: HotkeyAction) {
    let st = state();
    match action {
        HotkeyAction::SwitchToMac => {
            st.request_target(Target::RemoteMac);
        }
        HotkeyAction::SwitchToWindows => {
            st.request_target(Target::LocalWindows);
        }
        HotkeyAction::ForceLocal => {
            st.force_local("emergency hotkey");
        }
    }
}

/// Called by [`crate::state::AppState`] on every target transition.
pub fn on_target_changed(_target: Target) {
    with_tracker(|t| t.mark_all_stale());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hotkeys_default() -> Hotkeys {
        Hotkeys::from_config(&Config::default())
    }

    const HID_LCTRL: u16 = 0xE0;
    const HID_LALT: u16 = 0xE2;
    const HID_LEFT: u16 = 0x50;
    const HID_A: u16 = 0x04;

    #[test]
    fn ctrl_alt_left_fires_and_is_never_forwarded() {
        let hk = hotkeys_default();
        let mut t = InputTracker::default();
        assert_eq!(t.on_key(HID_LCTRL, true, &hk), KeyDecision::Forward { repeat: false });
        assert_eq!(t.on_key(HID_LALT, true, &hk), KeyDecision::Forward { repeat: false });
        assert_eq!(
            t.on_key(HID_LEFT, true, &hk),
            KeyDecision::Hotkey(HotkeyAction::SwitchToMac)
        );
        // The release of the trigger key must not leak to the remote either.
        assert_eq!(t.on_key(HID_LEFT, false, &hk), KeyDecision::Drop);
    }

    #[test]
    fn modifiers_held_across_a_switch_do_not_stick() {
        let hk = hotkeys_default();
        let mut t = InputTracker::default();
        t.on_key(HID_LCTRL, true, &hk);
        t.on_key(HID_LALT, true, &hk);
        t.on_key(HID_LEFT, true, &hk);
        // The switch happens here; Ctrl and Alt are still physically down.
        t.mark_all_stale();
        assert_eq!(t.on_key(HID_LCTRL, false, &hk), KeyDecision::Drop);
        assert_eq!(t.on_key(HID_LALT, false, &hk), KeyDecision::Drop);
        assert_eq!(t.modifiers, 0, "physical modifier state must be empty again");
        // A fresh press after the switch is forwarded normally.
        assert_eq!(t.on_key(HID_LCTRL, true, &hk), KeyDecision::Forward { repeat: false });
    }

    #[test]
    fn autorepeat_is_flagged_not_duplicated() {
        let hk = hotkeys_default();
        let mut t = InputTracker::default();
        assert_eq!(t.on_key(HID_A, true, &hk), KeyDecision::Forward { repeat: false });
        assert_eq!(t.on_key(HID_A, true, &hk), KeyDecision::Forward { repeat: true });
        assert_eq!(t.on_key(HID_A, false, &hk), KeyDecision::Forward { repeat: false });
    }

    #[test]
    fn emergency_hotkey_wins_over_a_shadowing_switch_hotkey() {
        let cfg = Config {
            hotkey_switch_to_mac: "Ctrl+Alt+Shift+Escape".into(),
            ..Config::default()
        };
        let hk = Hotkeys::from_config(&cfg);
        let mut t = InputTracker::default();
        t.on_key(HID_LCTRL, true, &hk);
        t.on_key(HID_LALT, true, &hk);
        t.on_key(0xE1, true, &hk); // Left Shift
        assert_eq!(
            t.on_key(0x29, true, &hk), // Escape
            KeyDecision::Hotkey(HotkeyAction::ForceLocal)
        );
    }

    #[test]
    fn buttons_held_across_a_switch_do_not_stick() {
        let mut t = InputTracker::default();
        assert_eq!(t.on_button(button::LEFT as usize, true), KeyDecision::Forward { repeat: false });
        t.mark_all_stale();
        assert_eq!(t.on_button(button::LEFT as usize, false), KeyDecision::Drop);
        assert_eq!(t.on_button(button::LEFT as usize, true), KeyDecision::Forward { repeat: false });
    }

    #[test]
    fn plain_arrow_key_is_forwarded() {
        let hk = hotkeys_default();
        let mut t = InputTracker::default();
        assert_eq!(t.on_key(HID_LEFT, true, &hk), KeyDecision::Forward { repeat: false });
    }
}
