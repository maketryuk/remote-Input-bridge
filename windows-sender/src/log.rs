//! Minimal levelled logger (spec §40). Mouse packets are only ever logged at TRACE.

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

const ERROR: u8 = 0;
const WARN: u8 = 1;
const INFO: u8 = 2;
const DEBUG: u8 = 3;
const TRACE: u8 = 4;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

pub fn set_level(name: &str) {
    let level = match name.to_ascii_uppercase().as_str() {
        "ERROR" => ERROR,
        "WARN" => WARN,
        "DEBUG" => DEBUG,
        "TRACE" => TRACE,
        _ => INFO,
    };
    LEVEL.store(level, Relaxed);
}

pub fn enabled(level: u8) -> bool {
    LEVEL.load(Relaxed) >= level
}

static FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

thread_local! {
    /// Guards against a log line produced while forwarding a log line.
    static FORWARDING: Cell<bool> = const { Cell::new(false) };
}

/// A tray app has no console unless it was started with `--console`, so everything also goes to
/// a file. Same reasoning as on the receiver: the most useful log is the one that exists.
fn file() -> &'static Mutex<Option<File>> {
    FILE.get_or_init(|| {
        let path = crate::config::config_dir().join("rib-sender.log");
        let _ = std::fs::create_dir_all(crate::config::config_dir());
        // Truncate rather than rotate: this is a debugging aid, not an audit trail.
        if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
            let _ = std::fs::remove_file(&path);
        }
        Mutex::new(OpenOptions::new().create(true).append(true).open(&path).ok())
    })
}

fn emit(level: u8, tag: &str, msg: &str) {
    let uptime = crate::state::try_state().map(|s| s.now_us()).unwrap_or(0);
    let line = format!("[{:>9.3}s] {tag:<5} {msg}", uptime as f64 / 1_000_000.0);
    println!("{line}");
    if let Ok(mut guard) = file().lock() {
        if let Some(handle) = guard.as_mut() {
            let _ = writeln!(handle, "{line}");
        }
    }
    // Mirror onto the Mac so one log file covers both machines. DEBUG and TRACE stay local:
    // forwarding a 1000 Hz trace would be its own performance problem.
    if level <= INFO && !FORWARDING.get() {
        FORWARDING.set(true);
        if let Some(state) = crate::state::try_state() {
            state.send(crate::net::NetMsg::LogLine(level, msg.to_string()));
        }
        FORWARDING.set(false);
    }
}

pub fn error(msg: &str) {
    if enabled(ERROR) {
        emit(ERROR, "ERROR", msg);
    }
}
pub fn warn(msg: &str) {
    if enabled(WARN) {
        emit(WARN, "WARN", msg);
    }
}
pub fn info(msg: &str) {
    if enabled(INFO) {
        emit(INFO, "INFO", msg);
    }
}
pub fn debug(msg: &str) {
    if enabled(DEBUG) {
        emit(DEBUG, "DEBUG", msg);
    }
}
pub fn trace(msg: &str) {
    if enabled(TRACE) {
        emit(TRACE, "TRACE", msg);
    }
}

pub fn trace_enabled() -> bool {
    enabled(TRACE)
}
