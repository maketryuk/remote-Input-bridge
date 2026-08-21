//! On-disk configuration and the paired-device store.
//!
//! Both live in `%APPDATA%\RemoteInputBridge\`. Config is hand-editable JSON; unknown keys are
//! preserved only in the sense that missing keys fall back to defaults, so a config written by
//! an older build keeps working.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;

pub const DEFAULT_TCP_PORT: u16 = 47821;
pub const DEFAULT_UDP_PORT: u16 = 47822;

/// Spec §9: 2 ms default, 1-8 ms allowed.
pub const MOUSE_INTERVAL_CHOICES_MS: [u32; 4] = [1, 2, 4, 8];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mac_host: String,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub device_name: String,
    /// Mouse aggregation/send interval in milliseconds (1, 2, 4 or 8).
    pub mouse_interval_ms: u32,
    pub heartbeat_ms: u64,
    pub heartbeat_timeout_ms: u64,
    pub auto_connect: bool,
    pub start_with_system: bool,
    /// Spec §6.4: off by default.
    pub edge_switch: bool,
    /// Suppress local Windows input while the Mac owns the input (spec §20).
    pub suppress_local_input: bool,
    /// Keep sending a movement packet every tick while the Mac holds the input, even when the
    /// mouse has not moved. Wi-Fi radios enter power saving during gaps and then deliver the next
    /// packets in a clump, which is what a user perceives as tearing; a steady cadence keeps the
    /// link awake and makes arrival times uniform. Costs ~22 kB/s while switched over, nothing
    /// while idle on Windows.
    pub keepalive_stream: bool,
    /// Send movement over UDP. Turning this off falls back to MOUSE_MOVE_REL over TCP, which is
    /// useful when diagnosing whether jitter is a UDP problem (spec §53).
    pub use_udp: bool,
    pub hotkey_switch_to_mac: String,
    pub hotkey_switch_to_windows: String,
    pub hotkey_emergency_local: String,
    /// ERROR | WARN | INFO | DEBUG | TRACE (spec §40).
    pub log_level: String,
    /// Emit the diagnostics line once per second (spec §39). It goes to the log file and, while
    /// connected, to the receiver's log as well - which is the only practical way to see the
    /// sender's numbers without sitting at the Windows machine.
    pub diagnostics: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mac_host: String::new(),
            tcp_port: DEFAULT_TCP_PORT,
            udp_port: DEFAULT_UDP_PORT,
            device_name: hostname(),
            mouse_interval_ms: 2,
            heartbeat_ms: 300,
            heartbeat_timeout_ms: 1000,
            auto_connect: true,
            start_with_system: false,
            edge_switch: false,
            suppress_local_input: true,
            keepalive_stream: true,
            use_udp: true,
            hotkey_switch_to_mac: "Ctrl+Alt+Left".into(),
            hotkey_switch_to_windows: "Ctrl+Alt+Right".into(),
            hotkey_emergency_local: "Ctrl+Alt+Shift+Escape".into(),
            log_level: "INFO".into(),
            diagnostics: true,
        }
    }
}

impl Config {
    pub fn sanitized(mut self) -> Self {
        if !MOUSE_INTERVAL_CHOICES_MS.contains(&self.mouse_interval_ms) {
            self.mouse_interval_ms = 2;
        }
        self.heartbeat_ms = self.heartbeat_ms.clamp(50, 5_000);
        self.heartbeat_timeout_ms = self.heartbeat_timeout_ms.clamp(self.heartbeat_ms * 2, 30_000);
        if self.device_name.trim().is_empty() {
            self.device_name = hostname();
        }
        self
    }

    pub fn mouse_interval_us(&self) -> u32 {
        self.mouse_interval_ms * 1000
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(config_path()) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(cfg) => cfg.sanitized(),
                Err(e) => {
                    eprintln!("[WARN] config.json is not valid JSON ({e}); using defaults");
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap() + "\n")
    }
}

/// Identity of this Windows install plus the key handed out by each paired Mac.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyStore {
    /// Stable random id (hex, 16 bytes) sent as `client_id` in HELLO.
    pub client_id: String,
    /// host -> device key (hex, 32 bytes).
    pub device_keys: std::collections::BTreeMap<String, String>,
}

impl Default for KeyStore {
    fn default() -> Self {
        let mut id = [0u8; 16];
        crate::crypto::random_bytes(&mut id);
        Self { client_id: crate::crypto::hex_encode(&id), device_keys: Default::default() }
    }
}

impl KeyStore {
    pub fn load() -> Self {
        match std::fs::read_to_string(keys_path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => {
                let fresh = KeyStore::default();
                let _ = fresh.save();
                fresh
            }
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let path = keys_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap() + "\n")
    }

    pub fn device_key(&self, host: &str) -> Option<crate::crypto::Key> {
        crate::crypto::hex_key(self.device_keys.get(host)?)
    }

    pub fn set_device_key(&mut self, host: &str, key: &crate::crypto::Key) {
        self.device_keys.insert(host.to_string(), crate::crypto::hex_encode(key));
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata).join("RemoteInputBridge")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("remote-input-bridge")
    } else {
        PathBuf::from(".")
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn keys_path() -> PathBuf {
    config_dir().join("keys.json")
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "windows-pc".into())
}

// ---------------------------------------------------------------------------
// Hotkey parsing
// ---------------------------------------------------------------------------

/// A hotkey is a set of required modifiers plus one trigger virtual-key code. Parsing lives here
/// (rather than in the hook) so it can be unit-tested without Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    /// Bitmask over [`crate::protocol::modmask`], left/right agnostic (`ANY_*` groups).
    pub mods: u16,
    pub vk: u16,
}

const VK_NAMES: &[(&str, u16)] = &[
    ("LEFT", 0x25),
    ("UP", 0x26),
    ("RIGHT", 0x27),
    ("DOWN", 0x28),
    ("ESCAPE", 0x1B),
    ("ESC", 0x1B),
    ("SPACE", 0x20),
    ("TAB", 0x09),
    ("ENTER", 0x0D),
    ("RETURN", 0x0D),
    ("BACKSPACE", 0x08),
    ("DELETE", 0x2E),
    ("INSERT", 0x2D),
    ("HOME", 0x24),
    ("END", 0x23),
    ("PAGEUP", 0x21),
    ("PAGEDOWN", 0x22),
    ("PAUSE", 0x13),
    ("SCROLLLOCK", 0x91),
    ("PRINTSCREEN", 0x2C),
];

pub fn parse_hotkey(spec: &str) -> Option<Hotkey> {
    use crate::protocol::modmask;
    let mut mods = 0u16;
    let mut vk = None;
    for part in spec.split('+') {
        let token = part.trim().to_ascii_uppercase();
        if token.is_empty() {
            continue;
        }
        match token.as_str() {
            "CTRL" | "CONTROL" => mods |= modmask::ANY_CTRL,
            "ALT" | "OPTION" => mods |= modmask::ANY_ALT,
            "SHIFT" => mods |= modmask::ANY_SHIFT,
            "WIN" | "SUPER" | "META" | "CMD" | "GUI" => mods |= modmask::ANY_GUI,
            other => {
                let code = if let Some((_, code)) = VK_NAMES.iter().find(|(n, _)| *n == other) {
                    *code
                } else if other.len() == 1 {
                    let c = other.as_bytes()[0];
                    if c.is_ascii_alphanumeric() {
                        c as u16
                    } else {
                        return None;
                    }
                } else if let Some(rest) = other.strip_prefix('F') {
                    let n: u16 = rest.parse().ok()?;
                    if (1..=24).contains(&n) {
                        0x70 + n - 1
                    } else {
                        return None;
                    }
                } else {
                    return None;
                };
                if vk.replace(code).is_some() {
                    return None; // two trigger keys is not a hotkey we can express
                }
            }
        }
    }
    Some(Hotkey { mods, vk: vk? })
}

/// True when `pressed` (a left/right aware mask) satisfies every modifier group in `required`
/// and carries no extra modifier group.
pub fn mods_match(required: u16, pressed: u16) -> bool {
    use crate::protocol::modmask;
    const GROUPS: [u16; 4] =
        [modmask::ANY_CTRL, modmask::ANY_SHIFT, modmask::ANY_ALT, modmask::ANY_GUI];
    for group in GROUPS {
        let want = required & group != 0;
        let have = pressed & group != 0;
        if want != have {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::modmask;

    #[test]
    fn parses_default_hotkeys() {
        assert_eq!(
            parse_hotkey("Ctrl+Alt+Left"),
            Some(Hotkey { mods: modmask::ANY_CTRL | modmask::ANY_ALT, vk: 0x25 })
        );
        assert_eq!(
            parse_hotkey("Ctrl+Alt+Shift+Escape"),
            Some(Hotkey {
                mods: modmask::ANY_CTRL | modmask::ANY_ALT | modmask::ANY_SHIFT,
                vk: 0x1B
            })
        );
        assert_eq!(parse_hotkey("Win+F5").unwrap().vk, 0x74);
        assert_eq!(parse_hotkey("Ctrl+Q").unwrap().vk, b'Q' as u16);
        assert_eq!(parse_hotkey("Ctrl+Alt"), None, "a hotkey needs a trigger key");
        assert_eq!(parse_hotkey("Ctrl+A+B"), None);
    }

    #[test]
    fn modifiers_must_match_exactly() {
        let need = modmask::ANY_CTRL | modmask::ANY_ALT;
        assert!(mods_match(need, modmask::L_CTRL | modmask::R_ALT));
        assert!(mods_match(need, modmask::L_CTRL | modmask::L_ALT | modmask::R_CTRL));
        assert!(!mods_match(need, modmask::L_CTRL));
        assert!(
            !mods_match(need, modmask::L_CTRL | modmask::L_ALT | modmask::L_SHIFT),
            "Ctrl+Alt+Shift+Left must not fire the Ctrl+Alt+Left hotkey"
        );
    }

    #[test]
    fn interval_is_clamped_to_the_allowed_set() {
        let cfg = Config { mouse_interval_ms: 3, ..Default::default() }.sanitized();
        assert_eq!(cfg.mouse_interval_ms, 2);
        let cfg = Config { mouse_interval_ms: 8, ..Default::default() }.sanitized();
        assert_eq!(cfg.mouse_interval_ms, 8);
    }
}
