//! Key derivation and packet authentication.
//!
//! Every realtime packet and every control frame carries an HMAC-SHA256 tag derived from the
//! paired device key, so an unpaired host cannot inject input even though the MVP does not
//! encrypt payloads (see docs/PROTOCOL.md "Threat model").

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

pub type Key = [u8; 32];

const PAIR_SALT: &[u8] = b"rib-pair-v1";
const PAIR_INFO: &[u8] = b"pairing";
const WRAP_INFO: &[u8] = b"wrap";
const SESSION_INFO_TCP: &[u8] = b"rib-session-v1|tcp";
const SESSION_INFO_UDP: &[u8] = b"rib-session-v1|udp";

pub fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

pub fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> Key {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, &mut out)
        .expect("32 bytes is a valid HKDF output length");
    out
}

/// Pairing codes are compared after case folding and stripping separators, so `abcd-efgh`,
/// `ABCD EFGH` and `abcdefgh` all pair.
pub fn normalize_pairing_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(|c| c.to_uppercase()).collect()
}

pub fn derive_pairing_key(code: &str) -> Key {
    hkdf32(normalize_pairing_code(code).as_bytes(), PAIR_SALT, PAIR_INFO)
}

pub fn wrap_mask(pairing_key: &Key) -> Key {
    hkdf32(pairing_key, PAIR_SALT, WRAP_INFO)
}

pub fn unwrap_device_key(pairing_key: &Key, wrapped: &Key) -> Key {
    let mask = wrap_mask(pairing_key);
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = wrapped[i] ^ mask[i];
    }
    out
}

/// TCP and UDP get separate keys from the same device key so a captured realtime packet can
/// never be replayed as a control frame.
pub fn session_keys(device_key: &Key, client_nonce: &[u8], server_nonce: &[u8]) -> (Key, Key) {
    let mut salt = Vec::with_capacity(client_nonce.len() + server_nonce.len());
    salt.extend_from_slice(client_nonce);
    salt.extend_from_slice(server_nonce);
    (
        hkdf32(device_key, &salt, SESSION_INFO_TCP),
        hkdf32(device_key, &salt, SESSION_INFO_UDP),
    )
}

pub fn pair_proof(pairing_key: &Key, client_nonce: &[u8], server_nonce: &[u8]) -> [u8; 32] {
    let mut data = Vec::with_capacity(4 + client_nonce.len() + server_nonce.len());
    data.extend_from_slice(b"pair");
    data.extend_from_slice(client_nonce);
    data.extend_from_slice(server_nonce);
    hmac(pairing_key, &data)
}

pub fn wrap_tag(
    pairing_key: &Key,
    client_nonce: &[u8],
    server_nonce: &[u8],
    wrapped: &[u8],
) -> [u8; 32] {
    let mut data = Vec::new();
    data.extend_from_slice(b"wrap");
    data.extend_from_slice(client_nonce);
    data.extend_from_slice(server_nonce);
    data.extend_from_slice(wrapped);
    hmac(pairing_key, &data)
}

pub fn auth_proof(device_key: &Key, client_nonce: &[u8], server_nonce: &[u8]) -> [u8; 32] {
    let mut data = Vec::new();
    data.extend_from_slice(b"auth");
    data.extend_from_slice(client_nonce);
    data.extend_from_slice(server_nonce);
    hmac(device_key, &data)
}

pub fn auth_ack_proof(
    device_key: &Key,
    client_nonce: &[u8],
    server_nonce: &[u8],
    session_id: u64,
) -> [u8; 32] {
    let mut data = Vec::new();
    data.extend_from_slice(b"auth-ack");
    data.extend_from_slice(client_nonce);
    data.extend_from_slice(server_nonce);
    data.extend_from_slice(&session_id.to_be_bytes());
    hmac(device_key, &data)
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.ct_eq(b).into()
}

pub fn random_bytes(out: &mut [u8]) {
    getrandom::getrandom(out).expect("OS entropy source unavailable");
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn random_key() -> Key {
    let mut k = [0u8; 32];
    random_bytes(&mut k);
    k
}

pub fn hex_encode(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        Some(match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        })
    };
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Some(out)
}

pub fn hex_key(s: &str) -> Option<Key> {
    hex_decode(s)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_normalisation_is_forgiving() {
        assert_eq!(normalize_pairing_code(" a4c9-k2mn "), "A4C9K2MN");
        assert_eq!(derive_pairing_key("a4c9-k2mn"), derive_pairing_key("A4C9 K2MN"));
    }

    #[test]
    fn device_key_survives_wrapping() {
        let pk = derive_pairing_key("A4C9K2MN");
        let device = random_key();
        let mask = wrap_mask(&pk);
        let mut wrapped = [0u8; 32];
        for i in 0..32 {
            wrapped[i] = device[i] ^ mask[i];
        }
        assert_ne!(wrapped, device);
        assert_eq!(unwrap_device_key(&pk, &wrapped), device);
    }

    #[test]
    fn tcp_and_udp_keys_differ() {
        let (tcp, udp) = session_keys(&[1u8; 32], b"cn", b"sn");
        assert_ne!(tcp, udp);
        let (tcp2, _) = session_keys(&[1u8; 32], b"cn", b"sn2");
        assert_ne!(tcp, tcp2, "nonces must diversify the session key");
    }

    /// Pins the key schedule to fixed vectors. The same vectors are asserted by the macOS
    /// receiver (`--crypto-selftest`) and printed by `scripts/test-sender.py --vectors`, so a
    /// change in any one implementation fails loudly instead of producing a silent auth failure.
    #[test]
    fn matches_reference_vectors() {
        let pk = derive_pairing_key("A4C9-K2MN");
        assert_eq!(
            hex_encode(&pk),
            "dfbd3be82b70e4ca211992bb8b397a876d6ef9e33ac61bfd690f235d6ff53886"
        );
        assert_eq!(
            hex_encode(&wrap_mask(&pk)),
            "a9a0f1a7ea6572f1388a07b725b20cb0923bf09bc19880a1b8f034afd8f13fcf"
        );
        let mut device_key = [0u8; 32];
        for (index, byte) in device_key.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let (tcp, udp) = session_keys(&device_key, b"client-nonce", b"server-nonce");
        assert_eq!(
            hex_encode(&tcp),
            "6400cb4f1e4490fd1cec12c08a9c7d56be1420fc3dbcea0940bb54716434f8da"
        );
        assert_eq!(
            hex_encode(&udp),
            "5461f982d118dead4af8ed61c969bb1c65e7e5db4c92e5f5dc2abc17224aa8ce"
        );
        assert_eq!(
            hex_encode(&hmac(&[1u8; 32], b"abc")),
            "73860612aa6aadf68985b6e9c4233357cedd5f24a221eba740b2aa8350276cfc"
        );
    }

    #[test]
    fn hex_round_trip() {
        let k = random_key();
        assert_eq!(hex_key(&hex_encode(&k)), Some(k));
        assert_eq!(hex_decode("zz"), None);
    }
}
