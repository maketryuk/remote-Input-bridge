//! Shared, lock-free-where-it-matters application state.
//!
//! The input hooks read `suppress` on every physical event, so that flag is a plain atomic and
//! flipping it is the *first* thing a target switch does — suppression must never lag the switch.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Instant;

use crate::config::{Config, KeyStore};
use crate::net::NetMsg;
use crate::telemetry::Telemetry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    LocalWindows = 0,
    RemoteMac = 1,
}

impl Target {
    fn from_u8(v: u8) -> Target {
        if v == 1 {
            Target::RemoteMac
        } else {
            Target::LocalWindows
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Target::LocalWindows => "Windows",
            Target::RemoteMac => "Mac",
        }
    }
}

/// Spec §41 connection lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Disconnected = 0,
    Connecting = 1,
    Authenticating = 2,
    Connected = 3,
}

impl LinkState {
    fn from_u8(v: u8) -> LinkState {
        match v {
            1 => LinkState::Connecting,
            2 => LinkState::Authenticating,
            3 => LinkState::Connected,
            _ => LinkState::Disconnected,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            LinkState::Disconnected => "Disconnected",
            LinkState::Connecting => "Connecting",
            LinkState::Authenticating => "Authenticating",
            LinkState::Connected => "Connected",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Status {
    pub remote_name: String,
    pub last_error: String,
    pub pairing_required: bool,
}

pub struct AppState {
    target: AtomicU8,
    link: AtomicU8,
    /// Read by the low-level hooks on every event. `true` = swallow local input.
    pub suppress: AtomicBool,
    pub session_id: AtomicU64,
    /// Cumulative raw mouse counts since process start; the realtime sender samples these and
    /// never resets them, which is what makes a lost datagram self-healing (spec §10.2).
    pub total_x: AtomicI32,
    pub total_y: AtomicI32,
    /// Scroll accumulates in HID units (120 = one notch) and drains on the aggregation tick.
    pub scroll_x: AtomicI32,
    pub scroll_y: AtomicI32,
    /// Physical modifier mask maintained by the raw-input path.
    pub modifiers: AtomicU16,
    /// Set once the UDP socket is bound and the session id is known.
    pub udp_ready: AtomicBool,
    /// Hot copies of two config flags. The low-level hooks read them on every physical event, and
    /// cloning the whole config there (five String allocations per mouse move) is not acceptable.
    pub edge_switch: AtomicBool,
    suppress_preference: AtomicBool,
    pub cfg: RwLock<Config>,
    pub keys: Mutex<KeyStore>,
    pub status: Mutex<Status>,
    pub tel: Telemetry,
    tx: Sender<NetMsg>,
    started: Instant,
}

static STATE: OnceLock<&'static AppState> = OnceLock::new();

/// The hooks are `extern "system"` callbacks with no user pointer, so the state has to be
/// reachable from a static. It is created once at startup and leaked deliberately.
pub fn state() -> &'static AppState {
    STATE.get().expect("state() called before init_state()")
}

pub fn try_state() -> Option<&'static AppState> {
    STATE.get().copied()
}

pub fn init_state(cfg: Config, keys: KeyStore, tx: Sender<NetMsg>) -> &'static AppState {
    let leaked: &'static AppState = Box::leak(Box::new(AppState {
        target: AtomicU8::new(Target::LocalWindows as u8),
        link: AtomicU8::new(LinkState::Disconnected as u8),
        suppress: AtomicBool::new(false),
        session_id: AtomicU64::new(0),
        total_x: AtomicI32::new(0),
        total_y: AtomicI32::new(0),
        scroll_x: AtomicI32::new(0),
        scroll_y: AtomicI32::new(0),
        modifiers: AtomicU16::new(0),
        udp_ready: AtomicBool::new(false),
        edge_switch: AtomicBool::new(cfg.edge_switch),
        suppress_preference: AtomicBool::new(cfg.suppress_local_input),
        cfg: RwLock::new(cfg),
        keys: Mutex::new(keys),
        status: Mutex::new(Status::default()),
        tel: Telemetry::default(),
        tx,
        started: Instant::now(),
    }));
    let _ = STATE.set(leaked);
    leaked
}

impl AppState {
    pub fn target(&self) -> Target {
        Target::from_u8(self.target.load(Ordering::Acquire))
    }

    pub fn link(&self) -> LinkState {
        LinkState::from_u8(self.link.load(Ordering::Acquire))
    }

    pub fn now_us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    pub fn send(&self, msg: NetMsg) {
        let _ = self.tx.send(msg);
    }

    pub fn sender(&self) -> Sender<NetMsg> {
        self.tx.clone()
    }

    pub fn config(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    /// The only way config should be replaced: it keeps the hot atomics in step.
    pub fn set_config(&self, cfg: Config) {
        self.edge_switch.store(cfg.edge_switch, Ordering::Release);
        self.suppress_preference.store(cfg.suppress_local_input, Ordering::Release);
        *self.cfg.write().unwrap() = cfg;
    }

    pub fn set_link(&self, link: LinkState) {
        self.link.store(link as u8, Ordering::Release);
        // Fail-safe (spec §17/§18): the moment the link is anything other than Connected the
        // physical keyboard and mouse must be back on Windows. This is the single choke point
        // for every failure mode - TCP close, heartbeat timeout, receiver crash, Wi-Fi loss.
        if link != LinkState::Connected && self.target() == Target::RemoteMac {
            self.force_local("link is no longer connected");
        }
        crate::ui::refresh();
    }

    /// Returns true when the switch actually happened.
    pub fn request_target(&self, target: Target) -> bool {
        if target == Target::RemoteMac && self.link() != LinkState::Connected {
            self.set_status_error("Cannot switch to Mac: not connected");
            crate::ui::refresh();
            return false;
        }
        self.apply_target(target)
    }

    pub fn force_local(&self, reason: &str) -> bool {
        if self.target() == Target::RemoteMac {
            crate::log::info(&format!("forcing input back to Windows: {reason}"));
        }
        self.apply_target(Target::LocalWindows)
    }

    fn apply_target(&self, target: Target) -> bool {
        let previous = self.target.swap(target as u8, Ordering::AcqRel);
        if previous == target as u8 {
            return false;
        }
        // Order matters: stop letting Windows see the input before telling the Mac to take over.
        let suppress = target == Target::RemoteMac
            && self.suppress_preference.load(Ordering::Acquire);
        self.suppress.store(suppress, Ordering::Release);
        crate::input::on_target_changed(target);
        // Only when Windows is actually meant to stop reacting: someone who turned suppression off
        // asked for local input to keep working, and stealing the focus would contradict that.
        crate::ui::input_hidden_from_windows(suppress);
        self.send(NetMsg::TargetChanged(target));
        crate::log::info(&format!("ACTIVE TARGET: {}", target.label().to_uppercase()));
        crate::ui::refresh();
        true
    }

    pub fn set_status_error(&self, msg: &str) {
        self.status.lock().unwrap().last_error = msg.to_string();
    }

    pub fn set_remote_name(&self, name: &str) {
        self.status.lock().unwrap().remote_name = name.to_string();
    }

    pub fn status_line(&self) -> String {
        let status = self.status.lock().unwrap();
        let cfg = self.cfg.read().unwrap();
        let host = if cfg.mac_host.is_empty() { "<not set>" } else { cfg.mac_host.as_str() };
        let remote = if status.remote_name.is_empty() {
            host.to_string()
        } else {
            format!("{} ({host})", status.remote_name)
        };
        format!(
            "{} | Mac: {remote} | Active input: {}",
            self.link().label(),
            self.target().label()
        )
    }
}
