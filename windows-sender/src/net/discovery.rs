//! Finding the Mac on the local network instead of typing its address.
//!
//! A DHCP lease changes and a hand-typed address stops working, with nothing to see but a
//! connection timeout - so the sender asks instead. It broadcasts a probe on the local subnet and
//! every receiver that hears it answers with its name, the `.local` name it can also be reached by,
//! and the ports it is listening on.
//!
//! The reply is deliberately smaller than the probe. A short question with a long answer is a
//! reflection amplifier: spoof the source address and every responder on the network helps flood
//! someone else. Padding the probe to 256 bytes makes answering it useless for that.
//!
//! Discovery grants nothing on its own. It reveals a name and two port numbers to anyone already
//! on the network, who could have found the ports with a scan anyway, and pairing still needs the
//! code shown on the Mac.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Discovery has its own port so a stray probe can never be mistaken for movement data.
pub const DEFAULT_DISCOVERY_PORT: u16 = 47823;

/// `RIBD`, big endian.
pub const MAGIC: u32 = 0x5249_4244;
pub const VERSION: u8 = 1;

/// Probes are padded to this so the answer is never larger than the question.
pub const PROBE_BYTES: usize = 256;
const MAX_REPLY_BYTES: usize = 1024;

/// How long to listen after asking. Long enough for a sleepy Wi-Fi link to answer, short enough
/// that the button does not feel stuck.
const LISTEN_FOR: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, Deserialize)]
pub struct Reply {
    /// What the Mac calls itself, for the user to recognise.
    pub name: String,
    /// Its multicast DNS name, which survives a new lease. Empty if it has none.
    #[serde(default)]
    pub host: String,
    pub tcp: u16,
    pub udp: u16,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct Found {
    pub address: SocketAddr,
    pub reply: Reply,
}

impl Found {
    /// What to put in the address field. The `.local` name is preferred over the address that
    /// answered: it is the one that keeps working after the lease changes, which is the entire
    /// reason this feature exists. Windows resolves those names natively.
    pub fn preferred_host(&self) -> String {
        if self.reply.host.is_empty() {
            self.address.ip().to_string()
        } else {
            self.reply.host.clone()
        }
    }

    pub fn label(&self) -> String {
        format!("{} ({})", self.reply.name, self.address.ip())
    }
}

pub fn probe_packet() -> Vec<u8> {
    let mut packet = Vec::with_capacity(PROBE_BYTES);
    packet.extend_from_slice(&MAGIC.to_be_bytes());
    packet.push(VERSION);
    packet.resize(PROBE_BYTES, 0);
    packet
}

/// Broadcasts a probe and collects whatever answers within [`LISTEN_FOR`].
///
/// Answers are keyed by address, so a Mac that hears the probe on two interfaces is listed once
/// per address it answers from - which is honest: those are genuinely different routes to it.
pub fn find(port: u16) -> Result<Vec<Found>, String> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).map_err(|e| format!("cannot open a socket: {e}"))?;
    socket.set_broadcast(true).map_err(|e| format!("cannot enable broadcast: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| format!("cannot set a read timeout: {e}"))?;

    // Every subnet this machine is on, not just 255.255.255.255. The limited broadcast goes out
    // through the default route, and a VPN holding that route makes it fail outright with "no
    // route to host" - measured, not guessed. A subnet-directed broadcast reaches the network the
    // Mac is actually on, whatever else the routing table is doing.
    let packet = probe_packet();
    let mut sent = 0;
    let mut last_error = String::new();
    for target in broadcast_targets() {
        match socket.send_to(&packet, (target, port)) {
            Ok(_) => sent += 1,
            Err(e) => last_error = format!("{target}: {e}"),
        }
    }
    if sent == 0 {
        return Err(format!(
            "the probe could not be sent on any network{}",
            if last_error.is_empty() { String::new() } else { format!(" ({last_error})") }
        ));
    }

    let deadline = Instant::now() + LISTEN_FOR;
    let mut found: Vec<Found> = Vec::new();
    let mut buffer = [0u8; MAX_REPLY_BYTES];
    while Instant::now() < deadline {
        let (read, from) = match socket.recv_from(&mut buffer) {
            Ok(result) => result,
            // A timeout is the normal way to notice that nobody else is going to answer.
            Err(_) => continue,
        };
        let Some(reply) = parse_reply(&buffer[..read]) else {
            continue;
        };
        if found.iter().any(|existing| existing.address == from) {
            continue;
        }
        found.push(Found { address: from, reply });
    }
    Ok(found)
}

/// Every address worth broadcasting a probe to: the directed broadcast of each subnet this machine
/// has an address on, plus the limited broadcast as a last resort for setups the enumeration missed.
fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut targets = local_broadcasts();
    targets.push(Ipv4Addr::BROADCAST);
    targets.dedup();
    targets
}

/// The directed broadcast address of every live IPv4 subnet this machine is attached to.
#[cfg(windows)]
fn local_broadcasts() -> Vec<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    const IF_OPER_STATUS_UP: i32 = 1;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    let mut buffer = vec![0u8; size as usize];
    // ERROR_BUFFER_OVERFLOW (111) is the documented way to be told the real size.
    let status = unsafe {
        GetAdaptersAddresses(AF_INET as u32, flags, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut size)
    };
    let status = if status == 111 {
        buffer = vec![0u8; size as usize];
        unsafe {
            GetAdaptersAddresses(AF_INET as u32, flags, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut size)
        }
    } else {
        status
    };
    if status != 0 {
        crate::log::debug(&format!("GetAdaptersAddresses failed ({status}); falling back to the limited broadcast"));
        return Vec::new();
    }

    let mut broadcasts = Vec::new();
    let mut adapter = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !adapter.is_null() {
        let current = unsafe { &*adapter };
        adapter = current.Next;
        if current.OperStatus != IF_OPER_STATUS_UP {
            continue;
        }
        let mut unicast = current.FirstUnicastAddress;
        while !unicast.is_null() {
            let entry = unsafe { &*unicast };
            unicast = entry.Next;
            let sockaddr = entry.Address.lpSockaddr;
            if sockaddr.is_null() || unsafe { (*sockaddr).sa_family } != AF_INET {
                continue;
            }
            let address = unsafe { &*(sockaddr as *const SOCKADDR_IN) };
            let octets = unsafe { address.sin_addr.S_un.S_addr }.to_ne_bytes();
            let ip = Ipv4Addr::from(octets);
            // A loopback or a self-assigned address has no Mac on the other end of it.
            if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
                continue;
            }
            let prefix = entry.OnLinkPrefixLength.min(32);
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
            let broadcast = Ipv4Addr::from(u32::from(ip) | !mask);
            if !broadcasts.contains(&broadcast) {
                broadcasts.push(broadcast);
            }
        }
    }
    broadcasts
}

#[cfg(not(windows))]
fn local_broadcasts() -> Vec<Ipv4Addr> {
    Vec::new()
}

pub fn parse_reply(bytes: &[u8]) -> Option<Reply> {
    if bytes.len() < 5 || u32::from_be_bytes(bytes[0..4].try_into().ok()?) != MAGIC {
        return None;
    }
    if bytes[4] != VERSION {
        return None;
    }
    let reply: Reply = serde_json::from_slice(&bytes[5..]).ok()?;
    if reply.tcp == 0 || reply.udp == 0 || reply.name.is_empty() {
        return None;
    }
    Some(reply)
}

// ---------------------------------------------------------------------------
// Search state for the settings window
// ---------------------------------------------------------------------------

/// A search runs on its own thread - it takes over a second, and the window has to keep drawing -
/// so what it found is left here for the next refresh to pick up.
struct Search {
    running: bool,
    /// Hosts to offer, most useful first. Taken once, by the window that owns the field.
    fresh: Option<Vec<String>>,
    message: String,
    message_at_us: u64,
}

static SEARCH: std::sync::Mutex<Search> = std::sync::Mutex::new(Search {
    running: false,
    fresh: None,
    message: String::new(),
    message_at_us: 0,
});

/// How long the result of a search stays on screen before the window goes back to its usual hint.
const MESSAGE_LIFETIME_US: u64 = 8_000_000;

pub fn is_searching() -> bool {
    SEARCH.lock().unwrap().running
}

/// The hosts found by the last search, if the window has not taken them yet.
pub fn take_found() -> Option<Vec<String>> {
    SEARCH.lock().unwrap().fresh.take()
}

/// What to tell the user about the last search, while it is still worth telling.
pub fn message() -> Option<String> {
    let search = SEARCH.lock().unwrap();
    if search.message.is_empty() {
        return None;
    }
    let age = crate::state::state().now_us().saturating_sub(search.message_at_us);
    (age < MESSAGE_LIFETIME_US).then(|| search.message.clone())
}

fn report(message: String, hosts: Option<Vec<String>>) {
    let mut search = SEARCH.lock().unwrap();
    search.running = false;
    search.message_at_us = crate::state::state().now_us();
    search.message = message;
    search.fresh = hosts;
    drop(search);
    crate::ui::refresh();
}

pub fn start_search(port: u16) {
    {
        let mut search = SEARCH.lock().unwrap();
        if search.running {
            return;
        }
        search.running = true;
    }
    crate::ui::refresh();
    std::thread::Builder::new()
        .name("rib-discovery".into())
        .spawn(move || match find(port) {
            Err(e) => {
                crate::log::warn(&format!("discovery failed: {e}"));
                report(format!("Could not search: {e}"), None);
            }
            Ok(found) if found.is_empty() => {
                crate::log::info("discovery: nobody answered");
                report(
                    "No Macs answered. Check that the app is running on the Mac and that both \
                     machines are on the same network."
                        .into(),
                    None,
                );
            }
            Ok(found) => {
                for entry in &found {
                    crate::log::info(&format!(
                        "discovery: {} at {} (version {})",
                        entry.reply.name, entry.address.ip(), entry.reply.version
                    ));
                }
                let message = format!(
                    "Found {}: {}",
                    if found.len() == 1 { "one Mac".to_string() } else { format!("{} Macs", found.len()) },
                    found.iter().map(|entry| entry.label()).collect::<Vec<_>>().join(", ")
                );
                // Both spellings are offered: the .local name survives a new DHCP lease, and the
                // address works even where multicast DNS does not.
                let mut hosts = Vec::new();
                for entry in &found {
                    let preferred = entry.preferred_host();
                    if !hosts.contains(&preferred) {
                        hosts.push(preferred);
                    }
                    let address = entry.address.ip().to_string();
                    if !hosts.contains(&address) {
                        hosts.push(address);
                    }
                }
                report(message, Some(hosts));
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_bytes(json: &str) -> Vec<u8> {
        let mut bytes = MAGIC.to_be_bytes().to_vec();
        bytes.push(VERSION);
        bytes.extend_from_slice(json.as_bytes());
        bytes
    }

    #[test]
    fn a_well_formed_reply_parses() {
        let reply = parse_reply(&reply_bytes(
            r#"{"name":"Mac","host":"mac.local","tcp":47821,"udp":47822,"version":"0.3.1"}"#,
        ))
        .expect("should parse");
        assert_eq!(reply.host, "mac.local");
        assert_eq!(reply.tcp, 47821);
    }

    /// Captured off the wire from the Swift responder rather than written by hand: the two
    /// implementations have to agree about the bytes, and only a real reply proves that they do.
    /// Note the name carries a non-breaking space and an em dash, which is exactly the sort of
    /// thing a hand-written fixture would have quietly omitted.
    #[test]
    fn the_reply_a_real_receiver_sends_parses() {
        const CAPTURED: &str = "52494244017b22686f7374223a224d6163426f6f6b2d50726f2d2d4d616b657472\
                                79756b2e6c6f63616c222c2276657273696f6e223a22302e332e31222c226e616d65\
                                223a224d6163426f6f6b2050726fc2a0e28094204d616b65747279756b222c227463\
                                70223a34373832312c22756470223a34373832327d";
        let bytes: Vec<u8> = CAPTURED
            .split_whitespace()
            .collect::<String>()
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        assert!(bytes.len() <= PROBE_BYTES, "a reply must never be larger than the probe");

        let reply = parse_reply(&bytes).expect("the receiver's own reply should parse");
        assert_eq!(reply.host, "MacBook-Pro--Maketryuk.local");
        assert_eq!(reply.tcp, 47821);
        assert_eq!(reply.udp, 47822);
        assert_eq!(reply.version, "0.3.1");
        assert!(reply.name.contains("MacBook Pro"));
    }

    #[test]
    fn anything_that_is_not_ours_is_ignored() {
        assert!(parse_reply(b"").is_none());
        assert!(parse_reply(b"hello there").is_none(), "a stray datagram on the port");
        let mut wrong_version = reply_bytes(r#"{"name":"Mac","tcp":1,"udp":2}"#);
        wrong_version[4] = VERSION + 1;
        assert!(parse_reply(&wrong_version).is_none());
        assert!(
            parse_reply(&reply_bytes(r#"{"name":"","tcp":47821,"udp":47822}"#)).is_none(),
            "a reply with no name is not usable in a list"
        );
        assert!(
            parse_reply(&reply_bytes(r#"{"name":"Mac","tcp":0,"udp":47822}"#)).is_none(),
            "port zero is not a port"
        );
    }

    #[test]
    fn the_probe_is_never_smaller_than_a_reply_could_be() {
        assert_eq!(probe_packet().len(), PROBE_BYTES);
        assert!(
            PROBE_BYTES >= 256,
            "the probe has to be big enough that answering it is no use to a reflector"
        );
    }

    #[test]
    fn the_local_name_is_preferred_over_the_address_that_answered() {
        let reply = Reply {
            name: "Mac".into(),
            host: "mac.local".into(),
            tcp: 47821,
            udp: 47822,
            version: String::new(),
        };
        let found = Found { address: "192.168.1.5:47823".parse().unwrap(), reply };
        assert_eq!(found.preferred_host(), "mac.local");

        let mut nameless = found.clone();
        nameless.reply.host = String::new();
        assert_eq!(nameless.preferred_host(), "192.168.1.5");
    }
}
