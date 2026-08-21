//! Realtime mouse channel: coalesce raw counts, emit one authenticated datagram per tick.
//!
//! Spec §8.2/§9: a 1000 Hz mouse must not become 1000 packets per second. The Raw Input path
//! only ever does `fetch_add` on two atomics; this thread samples them on a high-resolution
//! periodic timer and sends the *cumulative* total, which is what makes packet loss self-healing.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::crypto::Key;
use crate::protocol::{MouseMovePacket, Reliable};
use crate::state::{state, Target};

pub struct RealtimeParams {
    pub session_id: u64,
    pub udp_key: Key,
    pub udp_addr: SocketAddr,
    pub interval_us: u32,
    pub use_udp: bool,
    pub keepalive_stream: bool,
    pub keepalive_interval_us: u64,
}

pub fn spawn(params: RealtimeParams, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("rib-realtime".into())
        .spawn(move || {
            platform::raise_thread_priority();
            run(params, stop);
        })
        .expect("cannot spawn the realtime thread")
}

fn run(params: RealtimeParams, stop: Arc<AtomicBool>) {
    let st = state();
    let socket = if params.use_udp {
        match bind_udp(params.udp_addr) {
            Ok(s) => Some(s),
            Err(e) => {
                crate::log::error(&format!("UDP socket unavailable ({e}); falling back to TCP movement"));
                None
            }
        }
    } else {
        crate::log::info("UDP movement disabled by config; sending movement over TCP");
        None
    };
    if socket.is_some() {
        st.udp_ready.store(true, Relaxed);
    }

    let mut timer = platform::PreciseTimer::new(params.interval_us);
    let mut sequence: u64 = 0;
    let mut last_x = st.total_x.load(Relaxed);
    let mut last_y = st.total_y.load(Relaxed);
    let mut last_sent_us = 0u64;
    let trace = crate::log::trace_enabled();

    while !stop.load(Relaxed) {
        timer.wait();

        let x = st.total_x.load(Relaxed);
        let y = st.total_y.load(Relaxed);

        if st.target() != Target::RemoteMac {
            // Stay rebased while Windows owns the input so switching over never replays the
            // movement that happened locally.
            last_x = x;
            last_y = y;
            st.scroll_x.store(0, Relaxed);
            st.scroll_y.store(0, Relaxed);
            continue;
        }

        // Movement always goes out immediately. On top of that a keep-alive packet goes out
        // whenever the stream has been quiet for too long: gaps are what let the Wi-Fi radio doze
        // off and deliver the next few packets in a clump. The payload is cumulative, so a
        // "nothing changed" packet costs the receiver nothing - it computes a zero delta and
        // posts no event.
        let now_us = st.now_us();
        let keepalive_due = params.keepalive_stream
            && params.keepalive_interval_us > 0
            && now_us.saturating_sub(last_sent_us) >= params.keepalive_interval_us;
        if x != last_x || y != last_y || keepalive_due {
            last_sent_us = now_us;
            // Compute the delta before rebasing: the TCP fallback path needs it.
            let dx = x.wrapping_sub(last_x);
            let dy = y.wrapping_sub(last_y);
            last_x = x;
            last_y = y;
            sequence += 1;
            match &socket {
                Some(socket) => {
                    let packet = MouseMovePacket {
                        session_id: params.session_id,
                        sequence,
                        timestamp_us: now_us,
                        total_x: x,
                        total_y: y,
                    }
                    .encode(&params.udp_key);
                    match socket.send(&packet) {
                        Ok(n) => {
                            st.tel.udp_packets_sent.fetch_add(1, Relaxed);
                            st.tel.udp_bytes_sent.fetch_add(n as u64, Relaxed);
                            if trace {
                                crate::log::trace(&format!(
                                    "mouse seq={sequence} total=({x},{y})"
                                ));
                            }
                        }
                        Err(e) => {
                            // A send failure here is not fatal: the control channel decides
                            // whether the link is alive.
                            crate::log::debug(&format!("UDP send failed: {e}"));
                        }
                    }
                }
                None => {
                    // TCP fallback carries deltas, since a reliable stream cannot skip one.
                    // A keep-alive is pointless here: TCP keeps its own connection alive.
                    if dx != 0 || dy != 0 {
                        st.send(crate::net::NetMsg::Input(Reliable::MouseMoveRel { dx, dy }));
                    }
                }
            }
        } else {
            st.tel.coalesced_ticks_idle.fetch_add(1, Relaxed);
        }

        // Scroll rides the reliable channel but is coalesced on the same tick, so a fast wheel
        // spin becomes a few big deltas instead of a hundred tiny frames.
        let sx = st.scroll_x.swap(0, Relaxed);
        let sy = st.scroll_y.swap(0, Relaxed);
        if sx != 0 || sy != 0 {
            st.send(crate::net::NetMsg::Input(Reliable::Scroll { units_x: sx, units_y: sy }));
        }
    }

    st.udp_ready.store(false, Relaxed);
}

fn bind_udp(remote: SocketAddr) -> std::io::Result<UdpSocket> {
    let local: SocketAddr = if remote.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = UdpSocket::bind(local)?;
    socket.connect(remote)?;
    socket.set_nonblocking(false)?;
    Ok(socket)
}

#[cfg(windows)]
mod platform {
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
    use windows_sys::Win32::System::Threading::{
        CreateWaitableTimerExW, GetCurrentThread, SetThreadPriority, SetWaitableTimer,
        WaitForSingleObject, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE,
        THREAD_PRIORITY_TIME_CRITICAL,
    };

    const TIMER_ALL_ACCESS: u32 = 0x1F0003;

    pub fn raise_thread_priority() {
        // The tick does almost no work; what matters is that it is never late, because a late
        // tick is exactly what a user perceives as a stutter.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        }
    }

    pub struct PreciseTimer {
        handle: HANDLE,
        fallback: Option<std::time::Duration>,
        raised_period: bool,
    }

    impl PreciseTimer {
        pub fn new(interval_us: u32) -> PreciseTimer {
            let interval_ms = (interval_us / 1000).max(1);
            // timeBeginPeriod(1) keeps the fallback path (and the rest of the process) honest on
            // builds where the high-resolution timer is unavailable.
            unsafe { timeBeginPeriod(1) };
            let handle = unsafe {
                let h = CreateWaitableTimerExW(
                    ptr::null(),
                    ptr::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS,
                );
                if h.is_null() {
                    // Windows older than 10 1803 rejects the high-resolution flag.
                    CreateWaitableTimerExW(ptr::null(), ptr::null(), 0, TIMER_ALL_ACCESS)
                } else {
                    h
                }
            };
            if handle.is_null() {
                crate::log::warn("waitable timer unavailable; using sleep-based pacing");
                return PreciseTimer {
                    handle: ptr::null_mut(),
                    fallback: Some(std::time::Duration::from_micros(interval_us as u64)),
                    raised_period: true,
                };
            }
            let due: i64 = -(interval_us as i64) * 10; // relative, 100 ns units
            let ok = unsafe {
                SetWaitableTimer(handle, &due, interval_ms as i32, None, ptr::null(), 0)
            };
            if ok == 0 {
                crate::log::warn("SetWaitableTimer failed; using sleep-based pacing");
                unsafe { CloseHandle(handle) };
                return PreciseTimer {
                    handle: ptr::null_mut(),
                    fallback: Some(std::time::Duration::from_micros(interval_us as u64)),
                    raised_period: true,
                };
            }
            PreciseTimer { handle, fallback: None, raised_period: true }
        }

        pub fn wait(&mut self) {
            match self.fallback {
                Some(d) => std::thread::sleep(d),
                None => unsafe {
                    WaitForSingleObject(self.handle, INFINITE);
                },
            }
        }
    }

    impl Drop for PreciseTimer {
        fn drop(&mut self) {
            unsafe {
                if !self.handle.is_null() {
                    CloseHandle(self.handle);
                }
                if self.raised_period {
                    timeEndPeriod(1);
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use std::time::{Duration, Instant};

    pub fn raise_thread_priority() {}

    /// Absolute-deadline pacing so the interval does not drift by the work done per tick.
    pub struct PreciseTimer {
        period: Duration,
        next: Instant,
    }

    impl PreciseTimer {
        pub fn new(interval_us: u32) -> PreciseTimer {
            let period = Duration::from_micros(interval_us as u64);
            PreciseTimer { period, next: Instant::now() + period }
        }

        pub fn wait(&mut self) {
            let now = Instant::now();
            if self.next > now {
                std::thread::sleep(self.next - now);
            }
            self.next += self.period;
            if self.next < now {
                self.next = now + self.period;
            }
        }
    }
}
