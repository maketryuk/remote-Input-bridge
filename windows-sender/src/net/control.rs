//! Control channel: framing, handshake, pairing, authentication, reliable events.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::config::{Config, KeyStore};
use crate::crypto::{self, Key};
use crate::protocol::{self, Reliable, MAX_FRAME_LEN, TAG_LEN};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum ConnectError {
    Io(io::Error),
    Protocol(String),
    /// The Mac has no key for us and we were not given a pairing code.
    NeedsPairing,
    /// The Mac is not currently showing a pairing code.
    PairingDisabled,
    BadPairingCode,
    /// The Mac has a device key for us, but not the one we hold: the two sides diverged and only
    /// a fresh pairing can fix it. Retrying with the stored key would loop forever.
    AuthRejected,
    VersionMismatch(u32),
    Remote { code: String, message: String },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Io(e) => write!(f, "network error: {e}"),
            ConnectError::Protocol(m) => write!(f, "protocol error: {m}"),
            ConnectError::NeedsPairing => write!(f, "not paired with this Mac yet"),
            ConnectError::PairingDisabled => {
                write!(f, "the Mac is not in pairing mode (click Pair on the Mac first)")
            }
            ConnectError::BadPairingCode => write!(f, "wrong pairing code"),
            ConnectError::AuthRejected => {
                write!(f, "the Mac rejected our key - pair again (show a new code on the Mac)")
            }
            ConnectError::VersionMismatch(v) => {
                write!(f, "the Mac speaks protocol {v}, this build speaks {}", protocol::PROTOCOL_VERSION)
            }
            ConnectError::Remote { code, message } => write!(f, "{code}: {message}"),
        }
    }
}

impl From<io::Error> for ConnectError {
    fn from(e: io::Error) -> Self {
        ConnectError::Io(e)
    }
}

pub struct Session {
    pub stream: TcpStream,
    pub session_id: u64,
    pub tcp_key: Key,
    pub udp_key: Key,
    pub udp_addr: SocketAddr,
    pub server_name: String,
    send_counter: u64,
}

impl Session {
    pub fn send(&mut self, msg: Reliable) -> io::Result<()> {
        self.send_counter += 1;
        let frame = encode_frame(&self.tcp_key, self.send_counter, msg);
        self.stream.write_all(&frame)
    }

    /// Sends a message whose body is not a fixed-size `Reliable` (currently only log lines).
    pub fn send_raw(&mut self, msg_type: u8, body: &[u8]) -> io::Result<()> {
        self.send_counter += 1;
        let mut signed = Vec::with_capacity(9 + body.len());
        signed.extend_from_slice(&self.send_counter.to_be_bytes());
        signed.push(msg_type);
        signed.extend_from_slice(body);
        let tag = crypto::hmac(&self.tcp_key, &signed);
        let mut frame = Vec::with_capacity(4 + signed.len() + TAG_LEN);
        frame.extend_from_slice(&((signed.len() + TAG_LEN) as u32).to_be_bytes());
        frame.extend_from_slice(&signed);
        frame.extend_from_slice(&tag[..TAG_LEN]);
        self.stream.write_all(&frame)
    }

    pub fn try_clone_stream(&self) -> io::Result<TcpStream> {
        self.stream.try_clone()
    }
}

pub fn encode_frame(tcp_key: &Key, counter: u64, msg: Reliable) -> Vec<u8> {
    let mut body = Vec::with_capacity(32);
    body.extend_from_slice(&counter.to_be_bytes());
    body.push(msg.msg_type());
    msg.encode_body(&mut body);
    let tag = crypto::hmac(tcp_key, &body);
    body.extend_from_slice(&tag[..TAG_LEN]);
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

/// Verifies the tag and monotonic counter before decoding, so a forged frame never reaches the
/// message handlers.
pub fn decode_frame(tcp_key: &Key, payload: &[u8], last_counter: u64) -> Result<(u64, Reliable), String> {
    if payload.len() < 8 + 1 + TAG_LEN {
        return Err("short frame".into());
    }
    let split = payload.len() - TAG_LEN;
    let (signed, tag) = payload.split_at(split);
    let expected = crypto::hmac(tcp_key, signed);
    if !crypto::ct_eq(&expected[..TAG_LEN], tag) {
        return Err("bad frame authentication tag".into());
    }
    let counter = u64::from_be_bytes(signed[0..8].try_into().unwrap());
    if counter <= last_counter {
        return Err(format!("replayed or reordered frame counter {counter}"));
    }
    let msg_type = signed[8];
    let msg = Reliable::decode(msg_type, &signed[9..])
        .ok_or_else(|| format!("unknown reliable message type {msg_type:#02x}"))?;
    Ok((counter, msg))
}

fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
    stream.write_all(&(payload.len() as u32).to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

pub fn read_frame(stream: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad frame length {len}")));
    }
    buf.resize(len, 0);
    stream.read_exact(buf)
}

fn write_json<T: serde::Serialize>(stream: &mut TcpStream, value: &T) -> io::Result<()> {
    let text = serde_json::to_vec(value).expect("handshake frames are always serializable");
    write_frame(stream, &text)
}

fn read_json(stream: &mut TcpStream) -> Result<String, ConnectError> {
    let mut buf = Vec::new();
    read_frame(stream, &mut buf)?;
    String::from_utf8(buf).map_err(|_| ConnectError::Protocol("handshake frame is not UTF-8".into()))
}

fn remote_error(text: &str) -> Option<ConnectError> {
    let any: protocol::AnyFrame = serde_json::from_str(text).ok()?;
    if any.t != "ERROR" {
        return None;
    }
    let err: protocol::ProtoError = serde_json::from_str(text).unwrap_or(protocol::ProtoError {
        code: "PROTOCOL_ERROR".into(),
        message: String::new(),
    });
    Some(match err.code.as_str() {
        "PAIRING_DISABLED" => ConnectError::PairingDisabled,
        "BAD_PROOF" => ConnectError::BadPairingCode,
        "NOT_PAIRED" => ConnectError::NeedsPairing,
        _ => ConnectError::Remote { code: err.code, message: err.message },
    })
}

fn expect_frame(stream: &mut TcpStream, want: &str) -> Result<String, ConnectError> {
    let text = read_json(stream)?;
    if let Some(err) = remote_error(&text) {
        return Err(err);
    }
    let any: protocol::AnyFrame = serde_json::from_str(&text)
        .map_err(|e| ConnectError::Protocol(format!("malformed handshake frame: {e}")))?;
    if any.t != want {
        return Err(ConnectError::Protocol(format!("expected {want}, got {}", any.t)));
    }
    Ok(text)
}

/// Runs the full handshake. `pairing_code` is `Some` only when the user just typed one.
///
/// On success the device key is persisted, so the next connection skips pairing entirely.
pub fn connect(
    cfg: &Config,
    keys: &mut KeyStore,
    pairing_code: Option<&str>,
) -> Result<Session, ConnectError> {
    let host_port = format!("{}:{}", cfg.mac_host, cfg.tcp_port);
    let addr = host_port
        .to_socket_addrs()
        .map_err(|e| ConnectError::Protocol(format!("cannot resolve {host_port}: {e}")))?
        .next()
        .ok_or_else(|| ConnectError::Protocol(format!("cannot resolve {host_port}")))?;

    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_nodelay(true)?; // Nagle would batch keystrokes into 40 ms clumps.
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let mut client_nonce = [0u8; 32];
    crypto::random_bytes(&mut client_nonce);

    write_json(
        &mut stream,
        &protocol::Hello {
            t: "HELLO",
            protocol_version: protocol::PROTOCOL_VERSION,
            client_name: cfg.device_name.clone(),
            client_id: keys.client_id.clone(),
            client_nonce: crypto::hex_encode(&client_nonce),
            capabilities: protocol::capabilities(),
        },
    )?;

    let ack_text = expect_frame(&mut stream, "HELLO_ACK")?;
    let ack: protocol::HelloAck = serde_json::from_str(&ack_text)
        .map_err(|e| ConnectError::Protocol(format!("malformed HELLO_ACK: {e}")))?;
    if ack.protocol_version != protocol::PROTOCOL_VERSION {
        return Err(ConnectError::VersionMismatch(ack.protocol_version));
    }
    crate::log::debug(&format!(
        "receiver {:?} advertises capabilities {:?}",
        ack.server_name, ack.capabilities
    ));
    let server_nonce = crypto::hex_decode(&ack.server_nonce)
        .filter(|n| n.len() == 32)
        .ok_or_else(|| ConnectError::Protocol("HELLO_ACK carries no usable nonce".into()))?;

    // Filed under the Mac's own identity rather than the address it was reached at. An address is
    // not an identity: a new DHCP lease, or switching from 192.168.1.5 to the .local name, used to
    // make an already-paired Mac look like a stranger and ask for the pairing code again.
    let key_index =
        if ack.server_id.is_empty() { cfg.mac_host.clone() } else { ack.server_id.clone() };
    let stored_key = keys
        .device_key(&key_index)
        // Anything paired before the Mac had an identity is still filed under the address, and is
        // moved across the first time it authenticates.
        .or_else(|| keys.device_key(&cfg.mac_host));
    // `freshly_paired` decides whether the key is persisted after AUTH_OK. Storing it earlier is
    // what allows the two sides to diverge: if authentication never completes, one machine ends
    // up holding a key the other has never heard of, and every later attempt fails with no way
    // back except pairing again.
    let mut freshly_paired = false;
    let device_key = match (ack.known_client, stored_key, pairing_code) {
        // Both sides already know each other: straight to AUTH.
        (true, Some(key), None) => key,
        // An explicit pairing attempt always re-pairs, even if a stale key exists.
        (_, _, Some(code)) => {
            if !ack.pairing_mode {
                return Err(ConnectError::PairingDisabled);
            }
            freshly_paired = true;
            pair(&mut stream, code, &client_nonce, &server_nonce)?
        }
        // The Mac forgot us (or was reset): a fresh code is required.
        (false, _, None) => return Err(ConnectError::NeedsPairing),
        (true, None, None) => return Err(ConnectError::NeedsPairing),
    };

    let proof = crypto::auth_proof(&device_key, &client_nonce, &server_nonce);
    write_json(&mut stream, &protocol::Auth { t: "AUTH", proof: crypto::hex_encode(&proof) })?;

    let ok_text = match expect_frame(&mut stream, "AUTH_OK") {
        Ok(text) => text,
        // A rejected proof here is not a mistyped code - it means the stored key is stale.
        Err(ConnectError::BadPairingCode) => return Err(ConnectError::AuthRejected),
        Err(other) => return Err(other),
    };
    let ok: protocol::AuthOk = serde_json::from_str(&ok_text)
        .map_err(|e| ConnectError::Protocol(format!("malformed AUTH_OK: {e}")))?;
    let expected =
        crypto::auth_ack_proof(&device_key, &client_nonce, &server_nonce, ok.session_id);
    let got = crypto::hex_decode(&ok.server_proof).unwrap_or_default();
    if !crypto::ct_eq(&expected, &got) {
        // Mutual authentication: refuse to type into a host that cannot prove it holds our key.
        return Err(ConnectError::Protocol("the Mac failed to prove it holds our device key".into()));
    }

    // Also stored when nothing was freshly paired but the key was found under the old address-keyed
    // entry: that is the migration, and it costs one write, once.
    let needs_filing = freshly_paired || keys.device_key(&key_index).is_none();
    if needs_filing {
        // Both sides have now proven they hold the same key, so it is safe to keep.
        keys.set_device_key(&key_index, &device_key);
        if let Err(e) = keys.save() {
            crate::log::warn(&format!("could not persist the device key: {e}"));
        }
        if freshly_paired {
            crate::log::info("paired successfully; the code is not needed again");
        } else {
            crate::log::info(
                "this Mac now has a stable identity; the pairing will survive a change of address",
            );
        }
    }

    let (tcp_key, udp_key) = crypto::session_keys(&device_key, &client_nonce, &server_nonce);
    let udp_port = if ok.udp_port != 0 { ok.udp_port } else { cfg.udp_port };
    let mut udp_addr = addr;
    udp_addr.set_port(udp_port);

    stream.set_read_timeout(None)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    Ok(Session {
        stream,
        session_id: ok.session_id,
        tcp_key,
        udp_key,
        udp_addr,
        server_name: ack.server_name,
        send_counter: 0,
    })
}

/// Performs the pairing exchange and returns the unwrapped device key **without storing it**.
fn pair(
    stream: &mut TcpStream,
    code: &str,
    client_nonce: &[u8],
    server_nonce: &[u8],
) -> Result<Key, ConnectError> {
    let pairing_key = crypto::derive_pairing_key(code);
    let proof = crypto::pair_proof(&pairing_key, client_nonce, server_nonce);
    write_json(
        stream,
        &protocol::PairRequest { t: "PAIR_REQUEST", proof: crypto::hex_encode(&proof) },
    )?;
    let text = expect_frame(stream, "PAIR_RESPONSE")?;
    let resp: protocol::PairResponse = serde_json::from_str(&text)
        .map_err(|e| ConnectError::Protocol(format!("malformed PAIR_RESPONSE: {e}")))?;
    let wrapped = crypto::hex_key(&resp.wrapped_key)
        .ok_or_else(|| ConnectError::Protocol("PAIR_RESPONSE carries no wrapped key".into()))?;
    let expected_tag = crypto::wrap_tag(&pairing_key, client_nonce, server_nonce, &wrapped);
    let got_tag = crypto::hex_decode(&resp.tag).unwrap_or_default();
    if !crypto::ct_eq(&expected_tag, &got_tag) {
        return Err(ConnectError::BadPairingCode);
    }
    Ok(crypto::unwrap_device_key(&pairing_key, &wrapped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let key = [3u8; 32];
        let msg = Reliable::Key { hid_usage: 0x04, down: true, repeat: false };
        let frame = encode_frame(&key, 7, msg);
        let payload = &frame[4..];
        assert_eq!(u32::from_be_bytes(frame[0..4].try_into().unwrap()) as usize, payload.len());
        assert_eq!(decode_frame(&key, payload, 6).unwrap(), (7, msg));
    }

    #[test]
    fn tampered_frame_is_rejected() {
        let key = [3u8; 32];
        let mut frame = encode_frame(&key, 1, Reliable::ReleaseAll);
        let last = frame.len() - 1;
        frame[last] ^= 0xff;
        assert!(decode_frame(&key, &frame[4..], 0).is_err());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let frame = encode_frame(&[1u8; 32], 1, Reliable::ReleaseAll);
        assert!(decode_frame(&[2u8; 32], &frame[4..], 0).is_err());
    }

    #[test]
    fn replayed_counter_is_rejected() {
        let key = [3u8; 32];
        let frame = encode_frame(&key, 5, Reliable::ReleaseAll);
        assert!(decode_frame(&key, &frame[4..], 5).is_err());
        assert!(decode_frame(&key, &frame[4..], 9).is_err());
    }
}
