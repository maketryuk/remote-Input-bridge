//! Wire protocol v1. See ../../docs/PROTOCOL.md.
//!
//! Two channels: an authenticated TCP control channel carrying every event that must not be
//! lost, and an authenticated UDP channel carrying nothing but cumulative mouse position.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const UDP_MAGIC: u16 = 0x5249;
pub const UDP_VERSION: u8 = 1;
pub const UDP_TYPE_MOUSE_MOVE: u8 = 1;
pub const UDP_PACKET_LEN: usize = 44;
pub const MAX_FRAME_LEN: usize = 65536;
pub const TAG_LEN: usize = 16;

pub const CAP_MOUSE_MOVE_UDP: &str = "mouse_move_udp";
pub const CAP_MOUSE_BUTTONS: &str = "mouse_buttons";
pub const CAP_SCROLL_HIRES: &str = "scroll_hires";
pub const CAP_KEYBOARD: &str = "keyboard";
pub const CAP_HEARTBEAT: &str = "heartbeat";
pub const CAP_EDGE_SWITCH: &str = "edge_switch";

pub fn capabilities() -> Vec<String> {
    [
        CAP_MOUSE_MOVE_UDP,
        CAP_MOUSE_BUTTONS,
        CAP_SCROLL_HIRES,
        CAP_KEYBOARD,
        CAP_HEARTBEAT,
        CAP_EDGE_SWITCH,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ---------------------------------------------------------------------------
// Handshake (JSON frames, before the session key exists)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Hello {
    pub t: &'static str,
    pub protocol_version: u32,
    pub client_name: String,
    pub client_id: String,
    pub client_nonce: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct HelloAck {
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub server_nonce: String,
    #[serde(default)]
    pub known_client: bool,
    #[serde(default)]
    pub pairing_mode: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PairRequest {
    pub t: &'static str,
    pub proof: String,
}

#[derive(Debug, Deserialize)]
pub struct PairResponse {
    #[serde(default)]
    pub wrapped_key: String,
    #[serde(default)]
    pub tag: String,
}

#[derive(Debug, Serialize)]
pub struct Auth {
    pub t: &'static str,
    pub proof: String,
}

#[derive(Debug, Deserialize)]
pub struct AuthOk {
    #[serde(default)]
    pub session_id: u64,
    #[serde(default)]
    pub server_proof: String,
    #[serde(default)]
    pub udp_port: u16,
}

#[derive(Debug, Deserialize)]
pub struct ProtoError {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

/// Peeks at the `"t"` discriminator without committing to a concrete shape.
#[derive(Debug, Deserialize)]
pub struct AnyFrame {
    #[serde(default)]
    pub t: String,
}

// ---------------------------------------------------------------------------
// Reliable messages (binary, after AUTH_OK)
// ---------------------------------------------------------------------------

pub mod msg {
    pub const SESSION_START: u8 = 0x01;
    pub const PING: u8 = 0x02;
    pub const PONG: u8 = 0x03;
    pub const TARGET_ACTIVE: u8 = 0x04;
    pub const MOUSE_BUTTON: u8 = 0x05;
    pub const SCROLL: u8 = 0x06;
    pub const KEY: u8 = 0x07;
    pub const MODIFIER_SYNC: u8 = 0x08;
    pub const RELEASE_ALL: u8 = 0x09;
    pub const MOUSE_MOVE_REL: u8 = 0x0A;
    pub const BYE: u8 = 0x0B;
    pub const EDGE_HIT: u8 = 0x0C;
}

pub mod button {
    pub const LEFT: u8 = 0;
    pub const RIGHT: u8 = 1;
    pub const MIDDLE: u8 = 2;
    pub const BACK: u8 = 3;
    pub const FORWARD: u8 = 4;
    pub const COUNT: usize = 5;
}

/// Physical modifier bits. Deliberately left/right aware so the receiver can honour a remap
/// without guessing which physical key produced the modifier.
pub mod modmask {
    pub const L_CTRL: u16 = 1 << 0;
    pub const L_SHIFT: u16 = 1 << 1;
    pub const L_ALT: u16 = 1 << 2;
    pub const L_GUI: u16 = 1 << 3;
    pub const R_CTRL: u16 = 1 << 4;
    pub const R_SHIFT: u16 = 1 << 5;
    pub const R_ALT: u16 = 1 << 6;
    pub const R_GUI: u16 = 1 << 7;

    pub const ANY_CTRL: u16 = L_CTRL | R_CTRL;
    pub const ANY_SHIFT: u16 = L_SHIFT | R_SHIFT;
    pub const ANY_ALT: u16 = L_ALT | R_ALT;
    pub const ANY_GUI: u16 = L_GUI | R_GUI;
}

pub mod bye {
    pub const USER: u8 = 0;
    pub const SHUTDOWN: u8 = 1;
    pub const PROTOCOL_ERROR: u8 = 2;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reliable {
    SessionStart { mouse_interval_us: u32, flags: u8 },
    Ping { t_send_us: u64 },
    Pong { t_send_us: u64, applied_events: u64, udp_recv: u32, udp_dropped: u32 },
    TargetActive(bool),
    MouseButton { button: u8, down: bool },
    Scroll { units_x: i32, units_y: i32 },
    Key { hid_usage: u16, down: bool, repeat: bool },
    ModifierSync(u16),
    ReleaseAll,
    MouseMoveRel { dx: i32, dy: i32 },
    Bye(u8),
    EdgeHit(u8),
}

impl Reliable {
    pub fn msg_type(&self) -> u8 {
        match self {
            Reliable::SessionStart { .. } => msg::SESSION_START,
            Reliable::Ping { .. } => msg::PING,
            Reliable::Pong { .. } => msg::PONG,
            Reliable::TargetActive(_) => msg::TARGET_ACTIVE,
            Reliable::MouseButton { .. } => msg::MOUSE_BUTTON,
            Reliable::Scroll { .. } => msg::SCROLL,
            Reliable::Key { .. } => msg::KEY,
            Reliable::ModifierSync(_) => msg::MODIFIER_SYNC,
            Reliable::ReleaseAll => msg::RELEASE_ALL,
            Reliable::MouseMoveRel { .. } => msg::MOUSE_MOVE_REL,
            Reliable::Bye(_) => msg::BYE,
            Reliable::EdgeHit(_) => msg::EDGE_HIT,
        }
    }

    pub fn encode_body(&self, out: &mut Vec<u8>) {
        match *self {
            Reliable::SessionStart { mouse_interval_us, flags } => {
                out.extend_from_slice(&mouse_interval_us.to_be_bytes());
                out.push(flags);
            }
            Reliable::Ping { t_send_us } => out.extend_from_slice(&t_send_us.to_be_bytes()),
            Reliable::Pong { t_send_us, applied_events, udp_recv, udp_dropped } => {
                out.extend_from_slice(&t_send_us.to_be_bytes());
                out.extend_from_slice(&applied_events.to_be_bytes());
                out.extend_from_slice(&udp_recv.to_be_bytes());
                out.extend_from_slice(&udp_dropped.to_be_bytes());
            }
            Reliable::TargetActive(active) => out.push(active as u8),
            Reliable::MouseButton { button, down } => {
                out.push(button);
                out.push(down as u8);
            }
            Reliable::Scroll { units_x, units_y } => {
                out.extend_from_slice(&units_x.to_be_bytes());
                out.extend_from_slice(&units_y.to_be_bytes());
            }
            Reliable::Key { hid_usage, down, repeat } => {
                out.extend_from_slice(&hid_usage.to_be_bytes());
                out.push(down as u8);
                out.push(repeat as u8);
            }
            Reliable::ModifierSync(mask) => out.extend_from_slice(&mask.to_be_bytes()),
            Reliable::ReleaseAll => {}
            Reliable::MouseMoveRel { dx, dy } => {
                out.extend_from_slice(&dx.to_be_bytes());
                out.extend_from_slice(&dy.to_be_bytes());
            }
            Reliable::Bye(reason) => out.push(reason),
            Reliable::EdgeHit(edge) => out.push(edge),
        }
    }

    pub fn decode(msg_type: u8, body: &[u8]) -> Option<Reliable> {
        fn u64_at(b: &[u8], off: usize) -> Option<u64> {
            b.get(off..off + 8)?.try_into().ok().map(u64::from_be_bytes)
        }
        fn u32_at(b: &[u8], off: usize) -> Option<u32> {
            b.get(off..off + 4)?.try_into().ok().map(u32::from_be_bytes)
        }
        fn i32_at(b: &[u8], off: usize) -> Option<i32> {
            b.get(off..off + 4)?.try_into().ok().map(i32::from_be_bytes)
        }
        Some(match msg_type {
            msg::SESSION_START => Reliable::SessionStart {
                mouse_interval_us: u32_at(body, 0)?,
                flags: *body.get(4)?,
            },
            msg::PING => Reliable::Ping { t_send_us: u64_at(body, 0)? },
            msg::PONG => Reliable::Pong {
                t_send_us: u64_at(body, 0)?,
                applied_events: u64_at(body, 8)?,
                udp_recv: u32_at(body, 16)?,
                udp_dropped: u32_at(body, 20)?,
            },
            msg::TARGET_ACTIVE => Reliable::TargetActive(*body.first()? != 0),
            msg::MOUSE_BUTTON => Reliable::MouseButton {
                button: *body.first()?,
                down: *body.get(1)? != 0,
            },
            msg::SCROLL => Reliable::Scroll { units_x: i32_at(body, 0)?, units_y: i32_at(body, 4)? },
            msg::KEY => Reliable::Key {
                hid_usage: u16::from_be_bytes(body.get(0..2)?.try_into().ok()?),
                down: *body.get(2)? != 0,
                repeat: *body.get(3)? != 0,
            },
            msg::MODIFIER_SYNC => {
                Reliable::ModifierSync(u16::from_be_bytes(body.get(0..2)?.try_into().ok()?))
            }
            msg::RELEASE_ALL => Reliable::ReleaseAll,
            msg::MOUSE_MOVE_REL => {
                Reliable::MouseMoveRel { dx: i32_at(body, 0)?, dy: i32_at(body, 4)? }
            }
            msg::BYE => Reliable::Bye(*body.first().unwrap_or(&0)),
            msg::EDGE_HIT => Reliable::EdgeHit(*body.first()?),
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Realtime mouse packet
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct MouseMovePacket {
    pub session_id: u64,
    pub sequence: u64,
    pub timestamp_us: u64,
    pub total_x: i32,
    pub total_y: i32,
}

impl MouseMovePacket {
    /// Serialises into the fixed 44-byte layout; `tag` covers the first 36 bytes.
    pub fn encode(&self, udp_key: &[u8; 32]) -> [u8; UDP_PACKET_LEN] {
        let mut buf = [0u8; UDP_PACKET_LEN];
        buf[0..2].copy_from_slice(&UDP_MAGIC.to_be_bytes());
        buf[2] = UDP_VERSION;
        buf[3] = UDP_TYPE_MOUSE_MOVE;
        buf[4..12].copy_from_slice(&self.session_id.to_be_bytes());
        buf[12..20].copy_from_slice(&self.sequence.to_be_bytes());
        buf[20..28].copy_from_slice(&self.timestamp_us.to_be_bytes());
        buf[28..32].copy_from_slice(&self.total_x.to_be_bytes());
        buf[32..36].copy_from_slice(&self.total_y.to_be_bytes());
        let tag = crate::crypto::hmac(udp_key, &buf[0..36]);
        buf[36..44].copy_from_slice(&tag[0..8]);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reliable_round_trip() {
        let cases = [
            Reliable::SessionStart { mouse_interval_us: 2000, flags: 1 },
            Reliable::Ping { t_send_us: 123456789 },
            Reliable::Pong {
                t_send_us: 5,
                applied_events: 9,
                udp_recv: 100,
                udp_dropped: 2,
            },
            Reliable::TargetActive(true),
            Reliable::MouseButton { button: button::BACK, down: true },
            Reliable::Scroll { units_x: -120, units_y: 360 },
            Reliable::Key { hid_usage: 0x29, down: true, repeat: true },
            Reliable::ModifierSync(modmask::L_CTRL | modmask::R_ALT),
            Reliable::ReleaseAll,
            Reliable::MouseMoveRel { dx: -3, dy: 7 },
            Reliable::Bye(bye::USER),
            Reliable::EdgeHit(1),
        ];
        for c in cases {
            let mut body = Vec::new();
            c.encode_body(&mut body);
            assert_eq!(Reliable::decode(c.msg_type(), &body), Some(c), "{c:?}");
        }
    }

    #[test]
    fn udp_packet_layout() {
        let key = [7u8; 32];
        let p = MouseMovePacket {
            session_id: 0x0102030405060708,
            sequence: 42,
            timestamp_us: 1_000_000,
            total_x: -5,
            total_y: 9,
        };
        let buf = p.encode(&key);
        assert_eq!(&buf[0..2], &UDP_MAGIC.to_be_bytes());
        assert_eq!(buf[2], UDP_VERSION);
        assert_eq!(i32::from_be_bytes(buf[28..32].try_into().unwrap()), -5);
        assert_eq!(i32::from_be_bytes(buf[32..36].try_into().unwrap()), 9);
        assert_eq!(&buf[36..44], &crate::crypto::hmac(&key, &buf[0..36])[0..8]);
    }
}
