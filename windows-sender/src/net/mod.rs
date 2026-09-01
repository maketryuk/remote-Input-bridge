//! The network thread: one place that owns the connection lifecycle (spec §41), the heartbeat,
//! and the reliable event queue. Nothing here ever blocks the Raw Input path — input arrives as
//! messages on an unbounded channel.

pub mod control;
pub mod discovery;
pub mod realtime;

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::protocol::{bye, Reliable};
use crate::state::{state, LinkState, Target};
use control::{ConnectError, Session};

#[derive(Debug)]
pub enum NetMsg {
    /// A reliable input event produced by the input pipeline.
    Input(Reliable),
    /// The active target changed; the receiver has to be told and everything released.
    TargetChanged(Target),
    /// Decoded frame from the receiver, tagged with the connection generation it belongs to.
    Incoming(u64, Reliable),
    LinkLost(u64, String),
    /// User asked to (re)connect now.
    Reconnect,
    /// User typed a pairing code.
    Pair(String),
    /// A log line to mirror onto the receiver, so one log file covers both machines.
    LogLine(u8, String),
    /// Config was edited; reconnect if the transport parameters changed.
    ConfigChanged,
    Shutdown,
}

/// Spec §42.
const BACKOFF_MS: [u64; 5] = [500, 1_000, 2_000, 5_000, 5_000];

struct Connection {
    session: Session,
    generation: u64,
    stop_realtime: Arc<AtomicBool>,
    realtime: Option<JoinHandle<()>>,
    last_ping: Instant,
    last_pong: Instant,
}

impl Connection {
    fn close(mut self, reason: u8) {
        let _ = self.session.send(Reliable::Bye(reason));
        self.stop_realtime.store(true, Relaxed);
        let _ = self.session.stream.shutdown(std::net::Shutdown::Both);
        if let Some(handle) = self.realtime.take() {
            let _ = handle.join();
        }
    }
}

/// Transport parameters that require a reconnect when edited.
#[derive(PartialEq, Eq, Clone)]
struct TransportKey {
    host: String,
    tcp_port: u16,
    udp_port: u16,
    interval_ms: u32,
    use_udp: bool,
    keepalive_stream: bool,
    keepalive_interval_ms: u32,
}

fn transport_key(cfg: &crate::config::Config) -> TransportKey {
    TransportKey {
        host: cfg.mac_host.clone(),
        tcp_port: cfg.tcp_port,
        udp_port: cfg.udp_port,
        interval_ms: cfg.mouse_interval_ms,
        use_udp: cfg.use_udp,
        keepalive_stream: cfg.keepalive_stream,
        keepalive_interval_ms: cfg.keepalive_interval_ms,
    }
}

pub fn spawn(rx: Receiver<NetMsg>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("rib-net".into())
        .spawn(move || run(rx))
        .expect("cannot spawn the network thread")
}

fn run(rx: Receiver<NetMsg>) {
    let st = state();
    let mut conn: Option<Connection> = None;
    let mut generation: u64 = 0;
    let mut attempt: usize = 0;
    let mut next_attempt = Instant::now();
    let mut pair_code: Option<String> = None;
    let mut manual_request = false;
    let mut last_reported_error = String::new();
    let mut transport = transport_key(&st.config());
    let mut connected_once = false;

    loop {
        // ---- (re)connect ---------------------------------------------------
        if conn.is_none() {
            let cfg = st.config();
            let want = !cfg.mac_host.is_empty()
                && (cfg.auto_connect || manual_request || pair_code.is_some());
            if want && Instant::now() >= next_attempt {
                manual_request = false;
                generation += 1;
                st.set_link(LinkState::Connecting);
                let code = pair_code.take();
                let result = {
                    let mut keys = st.keys.lock().unwrap();
                    st.set_link(LinkState::Authenticating);
                    control::connect(&cfg, &mut keys, code.as_deref())
                };
                match result {
                    Ok(session) => {
                        attempt = 0;
                        last_reported_error.clear();
                        conn = Some(start_session(session, generation, &cfg, connected_once));
                        connected_once = true;
                    }
                    Err(err) => {
                        if matches!(err, ConnectError::AuthRejected) {
                            // Drop the key we know to be wrong, so the next attempt asks for a
                            // pairing code instead of retrying a proof that can never verify.
                            let mut keys = st.keys.lock().unwrap();
                            keys.device_keys.remove(&cfg.mac_host);
                            if let Err(e) = keys.save() {
                                crate::log::warn(&format!("could not clear the stale key: {e}"));
                            }
                        }
                        let text = err.to_string();
                        if text != last_reported_error {
                            crate::log::warn(&format!("connect failed: {text}"));
                            last_reported_error = text.clone();
                        }
                        st.set_status_error(&text);
                        st.status.lock().unwrap().pairing_required = matches!(
                            err,
                            ConnectError::NeedsPairing
                                | ConnectError::PairingDisabled
                                | ConnectError::BadPairingCode
                                | ConnectError::AuthRejected
                        );
                        st.set_link(LinkState::Disconnected);
                        crate::ui::refresh();
                        let wait = if st.status.lock().unwrap().pairing_required {
                            5_000
                        } else {
                            BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)]
                        };
                        attempt += 1;
                        next_attempt = Instant::now() + Duration::from_millis(wait);
                    }
                }
            }
        }

        // ---- wait for work ------------------------------------------------
        let cfg = st.config();
        let timeout = match &conn {
            Some(_) => Duration::from_millis(cfg.heartbeat_ms / 3 + 1),
            None => Duration::from_millis(100),
        };
        match rx.recv_timeout(timeout) {
            Ok(NetMsg::Shutdown) => {
                if let Some(c) = conn.take() {
                    c.close(bye::SHUTDOWN);
                }
                st.set_link(LinkState::Disconnected);
                return;
            }
            Ok(NetMsg::Input(msg)) => {
                if let Some(c) = conn.as_mut() {
                    if st.target() == Target::RemoteMac {
                        if let Err(e) = c.session.send(msg) {
                            drop_connection(&mut conn, format!("write failed: {e}"));
                        } else {
                            st.tel.reliable_sent.fetch_add(1, Relaxed);
                        }
                    }
                } else {
                    st.tel.dropped_no_link.fetch_add(1, Relaxed);
                }
            }
            Ok(NetMsg::LogLine(level, text)) => {
                if let Some(c) = conn.as_mut() {
                    // Best effort: a failed log write must not tear down a working session.
                    let mut body = Vec::with_capacity(1 + text.len());
                    body.push(level);
                    body.extend_from_slice(text.as_bytes());
                    let _ = c.session.send_raw(crate::protocol::msg::LOG, &body);
                }
            }
            Ok(NetMsg::TargetChanged(target)) => {
                if let Some(c) = conn.as_mut() {
                    let active = target == Target::RemoteMac;
                    // Release first when handing input back, announce first when taking it over,
                    // so the receiver never holds a key it cannot see released.
                    let sequence: [Reliable; 3] = if active {
                        [
                            Reliable::ReleaseAll,
                            Reliable::ModifierSync(0),
                            Reliable::TargetActive(true),
                        ]
                    } else {
                        [
                            Reliable::TargetActive(false),
                            Reliable::ReleaseAll,
                            Reliable::ModifierSync(0),
                        ]
                    };
                    for msg in sequence {
                        if let Err(e) = c.session.send(msg) {
                            drop_connection(&mut conn, format!("write failed: {e}"));
                            break;
                        }
                    }
                }
            }
            Ok(NetMsg::Incoming(gen, msg)) => {
                if conn.as_ref().map(|c| c.generation) == Some(gen) {
                    handle_incoming(&mut conn, msg);
                }
            }
            Ok(NetMsg::LinkLost(gen, reason)) => {
                if conn.as_ref().map(|c| c.generation) == Some(gen) {
                    drop_connection(&mut conn, reason);
                    attempt = 0;
                    next_attempt = Instant::now() + Duration::from_millis(BACKOFF_MS[0]);
                }
            }
            Ok(NetMsg::Reconnect) => {
                manual_request = true;
                attempt = 0;
                next_attempt = Instant::now();
                if let Some(c) = conn.take() {
                    c.close(bye::USER);
                    st.set_link(LinkState::Disconnected);
                }
            }
            Ok(NetMsg::Pair(code)) => {
                pair_code = Some(code);
                attempt = 0;
                next_attempt = Instant::now();
                last_reported_error.clear();
                if let Some(c) = conn.take() {
                    c.close(bye::USER);
                    st.set_link(LinkState::Disconnected);
                }
            }
            Ok(NetMsg::ConfigChanged) => {
                let fresh = transport_key(&st.config());
                if fresh != transport {
                    transport = fresh;
                    crate::log::info("transport settings changed; reconnecting");
                    if let Some(c) = conn.take() {
                        c.close(bye::USER);
                        st.set_link(LinkState::Disconnected);
                    }
                    attempt = 0;
                    next_attempt = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(c) = conn.take() {
                    c.close(bye::SHUTDOWN);
                }
                return;
            }
        }

        // ---- heartbeat (spec §18) -----------------------------------------
        if let Some(c) = conn.as_mut() {
            let now = Instant::now();
            if now.duration_since(c.last_pong) > Duration::from_millis(cfg.heartbeat_timeout_ms) {
                drop_connection(&mut conn, "heartbeat timeout".into());
                next_attempt = Instant::now() + Duration::from_millis(BACKOFF_MS[0]);
            } else if now.duration_since(c.last_ping) >= Duration::from_millis(cfg.heartbeat_ms) {
                c.last_ping = now;
                let msg = Reliable::Ping { t_send_us: st.now_us() };
                if let Err(e) = c.session.send(msg) {
                    drop_connection(&mut conn, format!("heartbeat write failed: {e}"));
                }
            }
        }
    }
}

fn start_session(
    mut session: Session,
    generation: u64,
    cfg: &crate::config::Config,
    is_reconnect: bool,
) -> Connection {
    let st = state();
    st.session_id.store(session.session_id, Relaxed);
    st.set_remote_name(&session.server_name);
    st.set_status_error("");
    st.status.lock().unwrap().pairing_required = false;

    let flags = if cfg.edge_switch { 1 } else { 0 };
    let _ = session.send(Reliable::SessionStart {
        mouse_interval_us: cfg.mouse_interval_us(),
        flags,
    });
    // The receiver must start in the "Windows owns the input" state: after a reconnect control
    // never jumps to the Mac on its own (spec §42).
    let _ = session.send(Reliable::TargetActive(false));
    let _ = session.send(Reliable::ReleaseAll);

    let stop_realtime = Arc::new(AtomicBool::new(false));
    let realtime = realtime::spawn(
        realtime::RealtimeParams {
            session_id: session.session_id,
            udp_key: session.udp_key,
            udp_addr: session.udp_addr,
            interval_us: cfg.mouse_interval_us(),
            use_udp: cfg.use_udp,
            keepalive_stream: cfg.keepalive_stream,
            keepalive_interval_us: cfg.keepalive_interval_ms as u64 * 1000,
        },
        stop_realtime.clone(),
    );

    match session.try_clone_stream() {
        Ok(read_half) => {
            spawn_reader(read_half, session.tcp_key, generation, st.sender());
        }
        Err(e) => crate::log::error(&format!("cannot clone the control socket: {e}")),
    }

    if is_reconnect {
        st.tel.reconnects.fetch_add(1, Relaxed);
    }
    crate::log::info(&format!(
        "connected to {} ({}), session {:#x}, mouse interval {} ms",
        if session.server_name.is_empty() { "Mac" } else { &session.server_name },
        cfg.mac_host,
        session.session_id,
        cfg.mouse_interval_ms
    ));
    st.set_link(LinkState::Connected);

    let now = Instant::now();
    Connection {
        session,
        generation,
        stop_realtime,
        realtime: Some(realtime),
        last_ping: now,
        last_pong: now,
    }
}

fn spawn_reader(
    mut stream: std::net::TcpStream,
    tcp_key: crate::crypto::Key,
    generation: u64,
    tx: Sender<NetMsg>,
) {
    std::thread::Builder::new()
        .name("rib-net-read".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let mut counter = 0u64;
            loop {
                match control::read_frame(&mut stream, &mut buf) {
                    Ok(()) => match control::decode_frame(&tcp_key, &buf, counter) {
                        Ok((next, msg)) => {
                            counter = next;
                            if tx.send(NetMsg::Incoming(generation, msg)).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(NetMsg::LinkLost(generation, e));
                            return;
                        }
                    },
                    Err(e) => {
                        let _ = tx.send(NetMsg::LinkLost(generation, format!("read failed: {e}")));
                        return;
                    }
                }
            }
        })
        .expect("cannot spawn the control reader thread");
}

fn handle_incoming(conn: &mut Option<Connection>, msg: Reliable) {
    let st = state();
    match msg {
        Reliable::Pong { t_send_us, applied_events, udp_recv, udp_dropped } => {
            if let Some(c) = conn.as_mut() {
                c.last_pong = Instant::now();
            }
            let rtt = st.now_us().saturating_sub(t_send_us);
            st.tel.record_rtt(rtt);
            st.tel.remote_applied.store(applied_events, Relaxed);
            st.tel.remote_udp_recv.store(udp_recv as u64, Relaxed);
            st.tel.remote_udp_dropped.store(udp_dropped as u64, Relaxed);
        }
        Reliable::EdgeHit(_) => {
            // Spec §6.4: the Mac's right edge hands input back to Windows.
            if st.config().edge_switch {
                st.request_target(Target::LocalWindows);
            }
        }
        Reliable::Bye(reason) => {
            drop_connection(conn, format!("receiver said goodbye (reason {reason})"));
        }
        other => crate::log::debug(&format!("ignoring unexpected message from receiver: {other:?}")),
    }
}

fn drop_connection(conn: &mut Option<Connection>, reason: String) {
    if let Some(c) = conn.take() {
        crate::log::warn(&format!("link down: {reason}"));
        c.close(bye::PROTOCOL_ERROR);
    }
    let st = state();
    st.set_status_error(&reason);
    // set_link is the fail-safe: it forces the active target back to Windows.
    st.set_link(LinkState::Disconnected);
}
