//! Counters and the per-second diagnostics snapshot (spec §38 Diagnostics, §39 overlay).
//!
//! Everything here is a relaxed atomic bumped from the hot paths; the sampler thread turns the
//! monotonic counters into rates once per second, so the input and network paths never format
//! strings or take a lock.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

#[derive(Default)]
pub struct Telemetry {
    /// WM_INPUT messages received, regardless of whether any event could be read out of them.
    pub wm_input_messages: AtomicU64,
    pub raw_mouse_events: AtomicU64,
    pub raw_kbd_events: AtomicU64,
    pub udp_packets_sent: AtomicU64,
    pub udp_bytes_sent: AtomicU64,
    pub reliable_sent: AtomicU64,
    pub dropped_no_link: AtomicU64,
    pub coalesced_ticks_idle: AtomicU64,
    pub reconnects: AtomicU64,
    /// Last measured round trip, microseconds.
    pub rtt_us: AtomicU64,
    /// Exponentially weighted mean RTT, microseconds.
    pub rtt_ewma_us: AtomicU64,
    /// Exponentially weighted mean absolute RTT deviation, microseconds.
    pub jitter_us: AtomicU64,
    /// Values reported by the receiver in PONG.
    pub remote_applied: AtomicU64,
    pub remote_udp_recv: AtomicU64,
    pub remote_udp_dropped: AtomicU64,
    snapshot: Mutex<Snapshot>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub wm_input_hz: f64,
    pub raw_mouse_hz: f64,
    pub raw_kbd_hz: f64,
    pub udp_send_hz: f64,
    pub udp_kbps: f64,
    pub reliable_hz: f64,
    pub loss_percent: f64,
    pub rtt_ms: f64,
    pub jitter_ms: f64,
    pub remote_event_hz: f64,
    pub reconnects: u64,
}

impl Telemetry {
    /// Folds a fresh RTT sample into the EWMA mean and mean-absolute-deviation estimate.
    /// Jitter, not absolute RTT, is the KPI that decides whether the cursor feels smooth (§32).
    pub fn record_rtt(&self, rtt_us: u64) {
        self.rtt_us.store(rtt_us, Relaxed);
        let prev = self.rtt_ewma_us.load(Relaxed);
        let mean = if prev == 0 { rtt_us } else { (prev * 7 + rtt_us) / 8 };
        self.rtt_ewma_us.store(mean, Relaxed);
        let deviation = rtt_us.abs_diff(mean);
        let prev_jitter = self.jitter_us.load(Relaxed);
        let jitter =
            if prev_jitter == 0 { deviation } else { (prev_jitter * 7 + deviation) / 8 };
        self.jitter_us.store(jitter, Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        *self.snapshot.lock().unwrap()
    }

    pub fn publish(&self, snapshot: Snapshot) {
        *self.snapshot.lock().unwrap() = snapshot;
    }
}

/// Per-second differencer: keeps the previous counter values and turns them into rates.
#[derive(Default)]
pub struct RateSampler {
    prev: [u64; 9],
    prev_instant: Option<std::time::Instant>,
}

impl RateSampler {
    pub fn sample(&mut self, tel: &Telemetry) -> Snapshot {
        let now = std::time::Instant::now();
        let dt = self.prev_instant.map(|p| now.duration_since(p).as_secs_f64()).unwrap_or(1.0);
        self.prev_instant = Some(now);
        let dt = if dt <= 0.0 { 1.0 } else { dt };

        let cur = [
            tel.raw_mouse_events.load(Relaxed),
            tel.wm_input_messages.load(Relaxed),
            tel.raw_kbd_events.load(Relaxed),
            tel.udp_packets_sent.load(Relaxed),
            tel.udp_bytes_sent.load(Relaxed),
            tel.reliable_sent.load(Relaxed),
            tel.remote_udp_recv.load(Relaxed),
            tel.remote_applied.load(Relaxed),
            tel.remote_udp_dropped.load(Relaxed),
        ];
        let mut delta = [0u64; 9];
        for i in 0..cur.len() {
            delta[i] = cur[i].saturating_sub(self.prev[i]);
        }
        self.prev = cur;

        // Loss is measured against what the receiver acknowledges having seen, not against a
        // local guess, so Wi-Fi drops show up honestly.
        let sent = delta[3] as f64;
        let received = delta[6] as f64;
        let loss = if sent > 0.0 { ((sent - received) / sent * 100.0).clamp(0.0, 100.0) } else { 0.0 };

        Snapshot {
            wm_input_hz: delta[1] as f64 / dt,
            raw_mouse_hz: delta[0] as f64 / dt,
            raw_kbd_hz: delta[2] as f64 / dt,
            udp_send_hz: sent / dt,
            udp_kbps: (delta[4] as f64 * 8.0 / 1000.0) / dt,
            reliable_hz: delta[5] as f64 / dt,
            loss_percent: loss,
            rtt_ms: tel.rtt_ewma_us.load(Relaxed) as f64 / 1000.0,
            jitter_ms: tel.jitter_us.load(Relaxed) as f64 / 1000.0,
            remote_event_hz: delta[7] as f64 / dt,
            reconnects: tel.reconnects.load(Relaxed),
        }
    }
}

impl Snapshot {
    pub fn render(&self, link: &str, target: &str) -> String {
        format!(
            "link {link:<13} target {target:<8} \
             wm_input {:>6.0} Hz  mouse in {:>6.0} Hz  keys {:>3.0} Hz  net out {:>5.0} Hz ({:>4.0} Hz reliable) \
             {:>5.1} kbit/s  loss {:>4.1}%  rtt {:>5.2} ms  jitter {:>4.2} ms  \
             mac events {:>5.0} Hz  reconnects {}",
            self.wm_input_hz,
            self.raw_mouse_hz,
            self.raw_kbd_hz,
            self.udp_send_hz,
            self.reliable_hz,
            self.udp_kbps,
            self.loss_percent,
            self.rtt_ms,
            self.jitter_ms,
            self.remote_event_hz,
            self.reconnects,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_is_derived_from_receiver_counts() {
        let tel = Telemetry::default();
        let mut s = RateSampler::default();
        s.sample(&tel);
        tel.udp_packets_sent.store(1000, Relaxed);
        tel.remote_udp_recv.store(970, Relaxed);
        let snap = s.sample(&tel);
        assert!((snap.loss_percent - 3.0).abs() < 0.01, "{:?}", snap.loss_percent);
    }

    #[test]
    fn jitter_tracks_deviation_not_magnitude() {
        let tel = Telemetry::default();
        for _ in 0..40 {
            tel.record_rtt(3_000);
        }
        let steady = tel.jitter_us.load(Relaxed);
        assert!(steady < 200, "steady RTT must read as low jitter, got {steady}");
        for rtt in [3_000, 12_000, 3_000, 15_000] {
            tel.record_rtt(rtt);
        }
        assert!(tel.jitter_us.load(Relaxed) > steady);
    }
}
