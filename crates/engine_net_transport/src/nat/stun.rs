//! STUN binding (RFC 8489, the subset games need): ask a server which address and port
//! our datagrams come from. Sans-I/O: the probe runs on the game socket through
//! [`crate::Endpoint::send_raw`] / [`crate::Endpoint::poll_foreign`], so the answer is the
//! mapping of the socket the game actually uses.
//!
//! Asking two servers tells the NAT's mapping behaviour (RFC 4787): the same external
//! port for both means endpoint-independent mapping (hole punching works); different
//! ports mean a symmetric NAT (it does not; a relay is needed).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use crate::crypto::random_bytes;

/// The magic cookie of RFC 5389/8489.
pub const MAGIC_COOKIE: u32 = 0x2112_a442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// Bytes of a binding request without attributes.
pub const REQUEST_BYTES: usize = 20;
/// Servers one probe asks at most.
pub const MAX_SERVERS: usize = 4;

/// Whether a datagram looks like STUN (first two bits zero, magic cookie).
pub fn is_stun(data: &[u8]) -> bool {
    data.len() >= 20 && data[0] & 0xc0 == 0 && data[4..8] == MAGIC_COOKIE.to_be_bytes()
}

/// Writes a binding request with `transaction` into `out`.
pub fn write_request(transaction: &[u8; 12], out: &mut [u8; REQUEST_BYTES]) {
    out[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    out[2..4].copy_from_slice(&0u16.to_be_bytes());
    out[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out[8..20].copy_from_slice(transaction);
}

fn xor_address(value: &[u8], transaction: &[u8; 12]) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]) ^ (MAGIC_COOKIE >> 16) as u16;
    let cookie = MAGIC_COOKIE.to_be_bytes();
    let ip = match family {
        1 => {
            let b = value.get(4..8)?;
            IpAddr::V4(Ipv4Addr::new(b[0] ^ cookie[0], b[1] ^ cookie[1], b[2] ^ cookie[2], b[3] ^ cookie[3]))
        }
        2 => {
            let b = value.get(4..20)?;
            let mut key = [0u8; 16];
            key[..4].copy_from_slice(&cookie);
            key[4..].copy_from_slice(transaction);
            let mut ip = [0u8; 16];
            for i in 0..16 {
                ip[i] = b[i] ^ key[i];
            }
            IpAddr::V6(Ipv6Addr::from(ip))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

fn plain_address(value: &[u8]) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]);
    let ip = match family {
        1 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(value.get(4..8)?).ok()?)),
        2 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(value.get(4..20)?).ok()?)),
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Parses a binding success response: its transaction id and the mapped address.
pub fn parse_response(data: &[u8]) -> Option<([u8; 12], SocketAddr)> {
    if !is_stun(data) || u16::from_be_bytes([data[0], data[1]]) != BINDING_SUCCESS {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([data[2], data[3]]));
    let body = data.get(20..20 + len)?;
    let transaction: [u8; 12] = data[8..20].try_into().ok()?;
    let mut plain = None;
    let mut at = 0;
    while at + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[at], body[at + 1]]);
        let alen = usize::from(u16::from_be_bytes([body[at + 2], body[at + 3]]));
        let value = body.get(at + 4..at + 4 + alen)?;
        match kind {
            ATTR_XOR_MAPPED_ADDRESS => return Some((transaction, xor_address(value, &transaction)?)),
            ATTR_MAPPED_ADDRESS => plain = plain_address(value),
            _ => {}
        }
        at += 4 + alen.div_ceil(4) * 4;
    }
    Some((transaction, plain?))
}

/// Answers a binding request from `from` (a minimal STUN server, as a relay or a game's
/// rendezvous service can run). Returns the response length.
pub fn answer_binding(request: &[u8], from: SocketAddr, out: &mut [u8]) -> Option<usize> {
    if !is_stun(request) || u16::from_be_bytes([request[0], request[1]]) != BINDING_REQUEST {
        return None;
    }
    let (family, ip_len) = if from.is_ipv4() { (1u8, 4) } else { (2u8, 16) };
    let total = 20 + 4 + 4 + ip_len;
    let out = out.get_mut(..total)?;
    out[0..2].copy_from_slice(&BINDING_SUCCESS.to_be_bytes());
    out[2..4].copy_from_slice(&((8 + ip_len) as u16).to_be_bytes());
    out[4..20].copy_from_slice(&request[4..20]);
    out[20..22].copy_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    out[22..24].copy_from_slice(&((4 + ip_len) as u16).to_be_bytes());
    out[24] = 0;
    out[25] = family;
    out[26..28].copy_from_slice(&(from.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
    let mut key = [0u8; 16];
    key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    key[4..].copy_from_slice(&request[8..20]);
    match from.ip() {
        IpAddr::V4(ip) => {
            for (i, b) in ip.octets().iter().enumerate() {
                out[28 + i] = b ^ key[i];
            }
        }
        IpAddr::V6(ip) => {
            for (i, b) in ip.octets().iter().enumerate() {
                out[28 + i] = b ^ key[i];
            }
        }
    }
    Some(total)
}

/// What a STUN probe found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StunResult {
    /// The address the first answering server saw.
    pub mapped: Option<SocketAddr>,
    /// Servers that answered.
    pub answers: usize,
    /// Two servers saw different external ports: a symmetric NAT (endpoint-dependent
    /// mapping). Hole punching will not work; use a relay.
    pub mapping_varies: bool,
}

#[derive(Clone, Copy)]
struct Query {
    server: SocketAddr,
    transaction: [u8; 12],
    attempts: u32,
    next_send: Instant,
    rto: Duration,
    answer: Option<SocketAddr>,
}

/// Asks up to [`MAX_SERVERS`] STUN servers for our mapped address. Retransmits with a
/// doubling timeout from 250 ms, four attempts per server (RFC 8489 uses 500 ms and seven;
/// a game wants an answer within seconds).
pub struct StunProbe {
    queries: [Option<Query>; MAX_SERVERS],
}

impl std::fmt::Debug for StunProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StunProbe").field("result", &self.result()).finish()
    }
}

const ATTEMPTS: u32 = 4;

impl StunProbe {
    /// A probe of `servers` (the first [`MAX_SERVERS`]).
    pub fn new(servers: &[SocketAddr], now: Instant) -> Self {
        let mut queries = [None; MAX_SERVERS];
        for (q, &server) in queries.iter_mut().zip(servers) {
            let mut transaction = [0u8; 12];
            random_bytes(&mut transaction);
            *q = Some(Query {
                server,
                transaction,
                attempts: 0,
                next_send: now,
                rto: Duration::from_millis(250),
                answer: None,
            });
        }
        Self { queries }
    }

    /// The next request due now: fills `out` and returns where to send it.
    pub fn poll_transmit(&mut self, now: Instant, out: &mut [u8; REQUEST_BYTES]) -> Option<SocketAddr> {
        for q in self.queries.iter_mut().flatten() {
            if q.answer.is_none() && q.attempts < ATTEMPTS && now >= q.next_send {
                q.attempts += 1;
                q.next_send = now + q.rto;
                q.rto *= 2;
                write_request(&q.transaction, out);
                return Some(q.server);
            }
        }
        None
    }

    /// Offers a received datagram; returns whether it was an answer to this probe.
    pub fn handle(&mut self, from: SocketAddr, data: &[u8]) -> bool {
        let Some((transaction, mapped)) = parse_response(data) else { return false };
        for q in self.queries.iter_mut().flatten() {
            if q.transaction == transaction && q.server == from {
                q.answer = Some(mapped);
                return true;
            }
        }
        false
    }

    /// Whether every server answered or gave up.
    pub fn is_done(&self, now: Instant) -> bool {
        self.queries
            .iter()
            .flatten()
            .all(|q| q.answer.is_some() || (q.attempts >= ATTEMPTS && now >= q.next_send))
    }

    /// The findings so far.
    pub fn result(&self) -> StunResult {
        let answers: Vec<SocketAddr> = self.queries.iter().flatten().filter_map(|q| q.answer).collect();
        StunResult {
            mapped: answers.first().copied(),
            answers: answers.len(),
            mapping_varies: answers.windows(2).any(|w| w[0] != w[1]),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn rfc_5769_ipv4_response_vector() {
        // RFC 5769 §2.2 sample IPv4 response (attributes: SOFTWARE, XOR-MAPPED-ADDRESS,
        // MESSAGE-INTEGRITY, FINGERPRINT); mapped address 192.0.2.1:32853.
        let hex = "0101003c2112a442b7e7a701bc34d686fa87dfae8022000b7465737420766563746f7220002000080001a147e112a643000800142b91f599fd9e90c38c7489f92af9ba53f06be7d780280004c07d4c96";
        let bytes: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let (_, addr) = parse_response(&bytes).unwrap();
        assert_eq!(addr, "192.0.2.1:32853".parse().unwrap());
    }

    #[test]
    fn answers_round_trip_v4_and_v6() {
        for from in ["198.51.100.7:40001", "[2001:db8::7]:40001"] {
            let from: SocketAddr = from.parse().unwrap();
            let t0 = Instant::now();
            let server: SocketAddr = "203.0.113.1:3478".parse().unwrap();
            let mut probe = StunProbe::new(&[server], t0);
            let mut req = [0u8; REQUEST_BYTES];
            assert_eq!(probe.poll_transmit(t0, &mut req), Some(server));
            assert!(probe.poll_transmit(t0, &mut req).is_none());
            let mut resp = [0u8; 64];
            let n = answer_binding(&req, from, &mut resp).unwrap();
            assert!(!probe.handle("203.0.113.2:3478".parse().unwrap(), &resp[..n]));
            assert!(probe.handle(server, &resp[..n]));
            assert_eq!(probe.result().mapped, Some(from));
            assert!(probe.is_done(t0));
        }
    }

    #[test]
    fn retransmits_then_gives_up() {
        let t0 = Instant::now();
        let mut probe = StunProbe::new(&["203.0.113.1:3478".parse().unwrap()], t0);
        let mut req = [0u8; REQUEST_BYTES];
        let mut sent = Vec::new();
        for ms in 0..5000 {
            let now = t0 + Duration::from_millis(ms);
            if probe.poll_transmit(now, &mut req).is_some() {
                sent.push(ms);
            }
        }
        assert_eq!(sent, vec![0, 250, 750, 1750]);
        assert!(probe.is_done(t0 + Duration::from_millis(3750)));
        assert_eq!(probe.result().mapped, None);
    }
}
