//! Minimal levelled logger (spec §40). Mouse packets are only ever logged at TRACE.

use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

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

fn emit(tag: &str, msg: &str) {
    let uptime = crate::state::try_state().map(|s| s.now_us()).unwrap_or(0);
    println!("[{:>9.3}s] {tag:<5} {msg}", uptime as f64 / 1_000_000.0);
}

pub fn error(msg: &str) {
    if enabled(ERROR) {
        emit("ERROR", msg);
    }
}
pub fn warn(msg: &str) {
    if enabled(WARN) {
        emit("WARN", msg);
    }
}
pub fn info(msg: &str) {
    if enabled(INFO) {
        emit("INFO", msg);
    }
}
pub fn debug(msg: &str) {
    if enabled(DEBUG) {
        emit("DEBUG", msg);
    }
}
pub fn trace(msg: &str) {
    if enabled(TRACE) {
        emit("TRACE", msg);
    }
}

pub fn trace_enabled() -> bool {
    enabled(TRACE)
}
