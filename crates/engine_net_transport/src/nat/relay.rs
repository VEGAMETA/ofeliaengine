//! Relays, the last rung of the ladder: when no mapping and no punched hole works
//! (symmetric or carrier-grade NAT on both sides), packets go through a server both
//! peers can reach.
//!
//! **The interface is [`DatagramSocket`]:** a relay path is a socket whose peers have
//! virtual addresses, and the [`crate::Endpoint`] runs on it unchanged (its packets stay
//! end-to-end encrypted; the relay sees ciphertext). Backends:
//! - [`RelaySocket`] with [`RelayServer`] below: a small TURN-like relay a game can host;
//! - Steam Datagram Relay, when the game ships with Steamworks: implement
//!   [`DatagramSocket`] over `ISteamNetworkingFakeUDPPort` (`SendMessageToFakeIP`,
//!   `ReceiveMessages`), which gives every peer a stable fake IPv4 address and port for
//!   exactly this purpose. The engine does not link Steamworks; the game supplies it;
//! - TURN (RFC 8656) servers, through an adapter of the same shape.
//!
//! The reference protocol (first byte 0xF2):
//! ```text
//! BIND      [F2 01] ticket[48] cookie[28]   client -> relay (cookie zero at first)
//! CHALLENGE [F2 02] cookie[28]              relay -> client (smaller than BIND)
//! BOUND     [F2 03] peer u32                relay -> client
//! SEND      [F2 04] to u32 payload          client -> relay
//! RECV      [F2 05] from u32 payload        relay -> client
//! ```
//! A ticket, issued by the game's backend with the relay's key, names a session, a peer
//! and an expiry. The cookie proves the binder receives at its address (return
//! routability), so a ticket holder cannot point the relay's traffic at a victim. The
//! relay forwards only between bound peers of one session: no open reflector.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use crate::crypto::{Cipher, Key, NONCE_BYTES, TAG_BYTES, random_bytes};
use crate::socket::DatagramSocket;
use crate::token::VERSION;

/// First byte of relay packets.
pub const RELAY_PREFIX: u8 = 0xf2;
const OP_BIND: u8 = 1;
const OP_CHALLENGE: u8 = 2;
const OP_BOUND: u8 = 3;
const OP_SEND: u8 = 4;
const OP_RECV: u8 = 5;
/// Bytes of a ticket.
pub const TICKET_BYTES: usize = NONCE_BYTES + 20 + TAG_BYTES;
const COOKIE_BYTES: usize = NONCE_BYTES + TAG_BYTES;
const BIND_BYTES: usize = 2 + TICKET_BYTES + COOKIE_BYTES;
/// Bytes a relayed packet adds.
pub const RELAY_OVERHEAD: usize = 6;
/// Bindings idle this long are dropped.
const BINDING_IDLE: Duration = Duration::from_secs(60);
/// A client re-binds this often (keeps its NAT mapping toward the relay alive too).
const REBIND: Duration = Duration::from_secs(15);

/// A permission to use a relay: session, peer id, expiry, sealed with the relay's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayTicket(pub [u8; TICKET_BYTES]);

impl RelayTicket {
    /// Issues a ticket (by the game's backend, which shares `relay_key` with the relay).
    pub fn issue(relay_key: &Key, session: u64, peer: u32, expire_unix: u64) -> Self {
        let mut t = [0u8; TICKET_BYTES];
        random_bytes(&mut t[..NONCE_BYTES]);
        let nonce: [u8; NONCE_BYTES] = t[..NONCE_BYTES].try_into().unwrap_or_default();
        t[12..20].copy_from_slice(&session.to_le_bytes());
        t[20..24].copy_from_slice(&peer.to_le_bytes());
        t[24..32].copy_from_slice(&expire_unix.to_le_bytes());
        let tag = Cipher::new(relay_key).seal(&nonce, &VERSION, &mut t[12..32]);
        t[32..].copy_from_slice(&tag);
        Self(t)
    }

    fn open(&self, cipher: &Cipher) -> Option<(u64, u32, u64)> {
        let mut t = self.0;
        let nonce: [u8; NONCE_BYTES] = t[..NONCE_BYTES].try_into().ok()?;
        let tag: [u8; TAG_BYTES] = t[32..].try_into().ok()?;
        cipher.open(&nonce, &VERSION, &mut t[12..32], &tag).ok()?;
        Some((
            u64::from_le_bytes(t[12..20].try_into().ok()?),
            u32::from_le_bytes(t[20..24].try_into().ok()?),
            u64::from_le_bytes(t[24..32].try_into().ok()?),
        ))
    }
}

/// The virtual address of relay peer `peer` (a unique-local IPv6 address, port 1).
pub fn peer_addr(peer: u32) -> SocketAddr {
    SocketAddr::new(
        IpAddr::V6(Ipv6Addr::new(
            0xfd00,
            0x6f66,
            0x6c79,
            0x7265,
            0x6c61,
            0x7900,
            (peer >> 16) as u16,
            peer as u16,
        )),
        1,
    )
}

fn peer_of(addr: SocketAddr) -> Option<u32> {
    let IpAddr::V6(v6) = addr.ip() else {
        return None;
    };
    let s = v6.segments();
    (s[..6] == [0xfd00, 0x6f66, 0x6c79, 0x7265, 0x6c61, 0x7900] && addr.port() == 1)
        .then(|| (u32::from(s[6]) << 16) | u32::from(s[7]))
}

/// The relay: a sans-I/O forwarding table. Run it on any UDP socket.
pub struct RelayServer {
    tickets: Cipher,
    cookies: Cipher,
    bindings: HashMap<(u64, u32), (SocketAddr, Instant)>,
    by_addr: HashMap<SocketAddr, (u64, u32)>,
    start: Instant,
}

impl std::fmt::Debug for RelayServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayServer")
            .field("bindings", &self.bindings.len())
            .finish()
    }
}

impl RelayServer {
    /// A relay accepting tickets sealed with `relay_key`.
    pub fn new(relay_key: &Key, now: Instant) -> Self {
        Self {
            tickets: Cipher::new(relay_key),
            cookies: Cipher::new(&Key::random()),
            bindings: HashMap::new(),
            by_addr: HashMap::new(),
            start: now,
        }
    }

    fn cookie_aad(&self, now: Instant, from: SocketAddr, epoch_back: u64) -> [u8; 27] {
        let epoch = now.saturating_duration_since(self.start).as_secs() / 30
            - epoch_back.min(now.saturating_duration_since(self.start).as_secs() / 30);
        let mut aad = [0u8; 27];
        aad[..8].copy_from_slice(&epoch.to_le_bytes());
        let (ip, port) = match from.ip() {
            IpAddr::V4(v4) => (v4.to_ipv6_mapped().octets(), from.port()),
            IpAddr::V6(v6) => (v6.octets(), from.port()),
        };
        aad[8..24].copy_from_slice(&ip);
        aad[24..26].copy_from_slice(&port.to_le_bytes());
        aad
    }

    fn cookie_valid(&self, now: Instant, from: SocketAddr, cookie: &[u8]) -> bool {
        let Ok(nonce) = <[u8; NONCE_BYTES]>::try_from(&cookie[..NONCE_BYTES]) else {
            return false;
        };
        let Ok(tag) = <[u8; TAG_BYTES]>::try_from(&cookie[NONCE_BYTES..]) else {
            return false;
        };
        // This epoch or the previous one (30-60 s validity).
        (0..2).any(|back| {
            self.cookies
                .open(&nonce, &self.cookie_aad(now, from, back), &mut [], &tag)
                .is_ok()
        })
    }

    /// Handles one datagram; returns a datagram to send, if any.
    pub fn handle(
        &mut self,
        now: Instant,
        unix_now: u64,
        from: SocketAddr,
        data: &[u8],
        out: &mut [u8],
    ) -> Option<(SocketAddr, usize)> {
        if data.len() < 2 || data[0] != RELAY_PREFIX {
            return None;
        }
        match data[1] {
            OP_BIND if data.len() == BIND_BYTES => {
                let ticket = RelayTicket(data[2..2 + TICKET_BYTES].try_into().ok()?);
                let (session, peer, expire) = ticket.open(&self.tickets)?;
                if expire <= unix_now {
                    return None;
                }
                let cookie = &data[2 + TICKET_BYTES..];
                if !self.cookie_valid(now, from, cookie) {
                    let mut nonce = [0u8; NONCE_BYTES];
                    random_bytes(&mut nonce);
                    let tag = self
                        .cookies
                        .seal(&nonce, &self.cookie_aad(now, from, 0), &mut []);
                    out[0] = RELAY_PREFIX;
                    out[1] = OP_CHALLENGE;
                    out[2..14].copy_from_slice(&nonce);
                    out[14..30].copy_from_slice(&tag);
                    return Some((from, 2 + COOKIE_BYTES));
                }
                if let Some((old, _)) = self.bindings.insert((session, peer), (from, now))
                    && old != from
                {
                    self.by_addr.remove(&old);
                }
                self.by_addr.insert(from, (session, peer));
                out[0] = RELAY_PREFIX;
                out[1] = OP_BOUND;
                out[2..6].copy_from_slice(&peer.to_le_bytes());
                Some((from, 6))
            }
            OP_SEND if data.len() >= 6 => {
                let &(session, peer) = self.by_addr.get(&from)?;
                if let Some(b) = self.bindings.get_mut(&(session, peer)) {
                    b.1 = now;
                }
                let to = u32::from_le_bytes(data[2..6].try_into().ok()?);
                let &(dest, _) = self.bindings.get(&(session, to))?;
                let n = data.len();
                let out = out.get_mut(..n)?;
                out[0] = RELAY_PREFIX;
                out[1] = OP_RECV;
                out[2..6].copy_from_slice(&peer.to_le_bytes());
                out[6..].copy_from_slice(&data[6..]);
                Some((dest, n))
            }
            _ => None,
        }
    }

    /// Drops bindings idle for a minute.
    pub fn expire(&mut self, now: Instant) {
        let by_addr = &mut self.by_addr;
        self.bindings.retain(|_, (addr, last)| {
            let keep = now.saturating_duration_since(*last) < BINDING_IDLE;
            if !keep {
                by_addr.remove(addr);
            }
            keep
        });
    }

    /// Live bindings.
    pub fn binding_count(&self) -> usize {
        self.bindings.len()
    }
}

/// A [`DatagramSocket`] through a [`RelayServer`]: peers of the session are reached at
/// [`peer_addr`]`(id)`; other addresses go direct on the same socket.
#[derive(Debug)]
pub struct RelaySocket<S: DatagramSocket> {
    inner: S,
    relay: SocketAddr,
    ticket: RelayTicket,
    cookie: [u8; COOKIE_BYTES],
    bound: bool,
    last_bind: Option<Instant>,
    scratch: Box<[u8]>,
}

impl<S: DatagramSocket> RelaySocket<S> {
    /// A socket that binds at `relay` with `ticket`.
    pub fn new(inner: S, relay: SocketAddr, ticket: RelayTicket) -> Self {
        Self {
            inner,
            relay,
            ticket,
            cookie: [0; COOKIE_BYTES],
            bound: false,
            last_bind: None,
            scratch: vec![0u8; 2048].into_boxed_slice(),
        }
    }

    /// Whether the relay confirmed the binding.
    pub fn is_bound(&self) -> bool {
        self.bound
    }

    fn maintain(&mut self, now: Instant) -> io::Result<()> {
        let due = match self.last_bind {
            None => true,
            Some(t) => {
                now.saturating_duration_since(t)
                    >= if self.bound {
                        REBIND
                    } else {
                        Duration::from_millis(250)
                    }
            }
        };
        if due {
            self.last_bind = Some(now);
            let mut b = [0u8; BIND_BYTES];
            b[0] = RELAY_PREFIX;
            b[1] = OP_BIND;
            b[2..2 + TICKET_BYTES].copy_from_slice(&self.ticket.0);
            b[2 + TICKET_BYTES..].copy_from_slice(&self.cookie);
            self.inner.send_to(now, self.relay, &b)?;
        }
        Ok(())
    }
}

impl<S: DatagramSocket> DatagramSocket for RelaySocket<S> {
    fn send_to(&mut self, now: Instant, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        self.maintain(now)?;
        let Some(peer) = peer_of(to) else {
            return self.inner.send_to(now, to, data);
        };
        let n = data.len() + RELAY_OVERHEAD;
        let Some(out) = self.scratch.get_mut(..n) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram too large for the relay",
            ));
        };
        out[0] = RELAY_PREFIX;
        out[1] = OP_SEND;
        out[2..6].copy_from_slice(&peer.to_le_bytes());
        out[6..].copy_from_slice(data);
        self.inner.send_to(now, self.relay, &self.scratch[..n])
    }

    fn recv_from(
        &mut self,
        now: Instant,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        self.maintain(now)?;
        loop {
            let Some((n, from)) = self.inner.recv_from(now, buf)? else {
                return Ok(None);
            };
            if from != self.relay || n < 2 || buf[0] != RELAY_PREFIX {
                return Ok(Some((n, from)));
            }
            match buf[1] {
                OP_RECV if n >= RELAY_OVERHEAD => {
                    let peer = u32::from_le_bytes(buf[2..6].try_into().unwrap_or_default());
                    buf.copy_within(RELAY_OVERHEAD..n, 0);
                    return Ok(Some((n - RELAY_OVERHEAD, peer_addr(peer))));
                }
                OP_CHALLENGE if n == 2 + COOKIE_BYTES => {
                    self.cookie.copy_from_slice(&buf[2..n]);
                    self.last_bind = None;
                    self.maintain(now)?;
                }
                OP_BOUND => self.bound = true,
                _ => {}
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::memory::{MemoryNetwork, NatBehavior};

    #[test]
    fn relays_between_symmetric_nats_and_only_within_a_session() {
        let net = MemoryNetwork::new(2);
        let relay_addr: SocketAddr = "203.0.113.50:3479".parse().unwrap();
        let key = Key::random();
        let t0 = Instant::now();
        let mut relay = RelayServer::new(&key, t0);
        let mut relay_socket = net.bind(relay_addr);
        let nat_a = net.add_nat("198.51.100.1".parse().unwrap(), NatBehavior::Symmetric);
        let nat_b = net.add_nat("198.51.100.2".parse().unwrap(), NatBehavior::Symmetric);
        let unix = crate::unix_now();
        let mut a = RelaySocket::new(
            net.bind_behind_nat("192.168.1.2:1".parse().unwrap(), nat_a),
            relay_addr,
            RelayTicket::issue(&key, 7, 1, unix + 60),
        );
        let mut b = RelaySocket::new(
            net.bind_behind_nat("192.168.2.2:1".parse().unwrap(), nat_b),
            relay_addr,
            RelayTicket::issue(&key, 7, 2, unix + 60),
        );
        let mut other = RelaySocket::new(
            net.bind("203.0.113.9:1".parse().unwrap()),
            relay_addr,
            RelayTicket::issue(&key, 8, 3, unix + 60),
        );
        let forged = RelayTicket::issue(&Key::random(), 7, 4, unix + 60);
        let mut mallory = RelaySocket::new(
            net.bind("203.0.113.10:1".parse().unwrap()),
            relay_addr,
            forged,
        );
        let mut buf = [0u8; 2048];
        let mut out = [0u8; 2048];
        let mut got_b = Vec::new();
        for ms in 0..2000u64 {
            let now = t0 + Duration::from_millis(ms);
            if ms == 1000 {
                a.send_to(now, peer_addr(2), b"through the relay").unwrap();
                other.send_to(now, peer_addr(2), b"other session").unwrap();
                mallory.send_to(now, peer_addr(2), b"forged").unwrap();
            }
            while let Some((n, from)) = relay_socket.recv_from(now, &mut buf).unwrap() {
                if let Some((to, m)) = relay.handle(now, unix, from, &buf[..n], &mut out) {
                    relay_socket.send_to(now, to, &out[..m]).unwrap();
                }
            }
            for s in [&mut a, &mut other, &mut mallory] {
                while s.recv_from(now, &mut buf).unwrap().is_some() {}
            }
            while let Some((n, from)) = b.recv_from(now, &mut buf).unwrap() {
                got_b.push((buf[..n].to_vec(), from));
            }
        }
        assert!(a.is_bound() && b.is_bound() && other.is_bound() && !mallory.is_bound());
        assert_eq!(relay.binding_count(), 3);
        assert_eq!(got_b, vec![(b"through the relay".to_vec(), peer_addr(1))]);
        relay.expire(t0 + Duration::from_secs(100));
        assert_eq!(relay.binding_count(), 0);
    }
}
