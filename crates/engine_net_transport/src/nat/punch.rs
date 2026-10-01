//! UDP hole punching: both peers send authenticated probes to every candidate address of
//! the other at the same time, so each NAT sees outgoing traffic to the peer before the
//! peer's packets arrive and lets them in (Ford, Srisuresh, Kegel, "Peer-to-Peer
//! Communication Across Network Address Translators", USENIX 2005; RFC 5128).
//!
//! The candidates (LAN address, STUN-mapped address, port-mapped address) travel through
//! a rendezvous the game provides ([`Rendezvous`]): a lobby service, the platform's invite
//! data, or the game's own server. Probes are sealed with a key both peers derive from
//! the connect token ([`crate::ConnectToken::punch_key`]), so a third party can neither
//! fake a successful punch nor learn anything from the probes. Sans-I/O: probes go out
//! through [`crate::Endpoint::send_raw`] on the game socket, whose NAT mapping is the
//! one being opened.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::crypto::{Cipher, Key, TAG_BYTES, sequence_nonce};
use crate::token::{AddressList, VERSION};

/// First byte of a punch packet (outside the transport's prefixes, not STUN).
pub const PUNCH_PREFIX: u8 = 0xf1;
const PROBE: u8 = 1;
const ACK: u8 = 2;
/// Bytes of a punch packet.
pub const PUNCH_BYTES: usize = 1 + 8 + 8 + 1 + TAG_BYTES;

/// Where the game exchanges candidates with the other peer. Implement it over whatever
/// channel the game has; the transport never talks to a third party by itself.
pub trait Rendezvous {
    /// Publishes our candidates for `session` (the other peer reads them).
    fn publish(&mut self, session: u64, candidates: &AddressList);
    /// The other peer's candidates for `session`, once known.
    fn peer_candidates(&mut self, session: u64) -> Option<AddressList>;
}

/// An in-process [`Rendezvous`] for tests and LAN tools: both sides share one board.
#[derive(Clone, Debug, Default)]
pub struct LocalRendezvous {
    board: std::sync::Arc<std::sync::Mutex<Vec<(u64, bool, AddressList)>>>,
    host: bool,
}

impl LocalRendezvous {
    /// A board; `for_peer` gives the other side's handle.
    pub fn new_host() -> Self {
        Self {
            board: Default::default(),
            host: true,
        }
    }

    /// The other side's handle on the same board.
    pub fn for_peer(&self) -> Self {
        Self {
            board: self.board.clone(),
            host: !self.host,
        }
    }
}

impl Rendezvous for LocalRendezvous {
    fn publish(&mut self, session: u64, candidates: &AddressList) {
        let mut b = self
            .board
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        b.retain(|(s, h, _)| !(*s == session && *h == self.host));
        b.push((session, self.host, *candidates));
    }

    fn peer_candidates(&mut self, session: u64) -> Option<AddressList> {
        let b = self
            .board
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        b.iter()
            .find(|(s, h, _)| *s == session && *h != self.host)
            .map(|(_, _, c)| *c)
    }
}

/// How a punch ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PunchState {
    /// Still probing.
    Probing,
    /// Our probes reach the peer at this address and its probes reach us.
    Open(SocketAddr),
    /// Nothing got through before the deadline.
    Failed,
}

/// One hole punch toward one peer.
pub struct Puncher {
    cipher: Cipher,
    session: u64,
    initiator: bool,
    remotes: AddressList,
    interval: Duration,
    next_round: Instant,
    next_index: usize,
    deadline: Instant,
    seq: u64,
    ack_to: Option<SocketAddr>,
    heard_from: Option<SocketAddr>,
    state: PunchState,
}

impl std::fmt::Debug for Puncher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Puncher")
            .field("session", &self.session)
            .field("state", &self.state)
            .finish()
    }
}

impl Puncher {
    /// A punch for `session` toward `remotes`, sealed with `key`. One side is the
    /// `initiator` (the client), the other not (the host). Probes go to every candidate
    /// every 50 ms until `timeout`.
    pub fn new(
        key: &Key,
        session: u64,
        initiator: bool,
        remotes: AddressList,
        now: Instant,
        timeout: Duration,
    ) -> Self {
        Self {
            cipher: Cipher::new(key),
            session,
            initiator,
            remotes,
            interval: Duration::from_millis(50),
            next_round: now,
            next_index: 0,
            deadline: now + timeout,
            seq: 0,
            ack_to: None,
            heard_from: None,
            state: PunchState::Probing,
        }
    }

    /// The outcome so far.
    pub fn state(&self) -> PunchState {
        self.state
    }

    /// The session id.
    pub fn session(&self) -> u64 {
        self.session
    }

    fn seal(&mut self, kind: u8, out: &mut [u8; PUNCH_BYTES]) {
        let seq = self.seq;
        self.seq += 1;
        // The two sides share the key: the top bit keeps their nonces apart.
        let nonce_seq = seq | (u64::from(self.initiator) << 63);
        out[0] = PUNCH_PREFIX;
        out[1..9].copy_from_slice(&self.session.to_le_bytes());
        out[9..17].copy_from_slice(&nonce_seq.to_le_bytes());
        out[17] = kind;
        let aad: [u8; 17] = {
            let mut a = [0u8; 17];
            a[..8].copy_from_slice(&VERSION);
            a[8..17].copy_from_slice(&out[0..9]);
            a
        };
        let tag = self
            .cipher
            .seal(&sequence_nonce(nonce_seq), &aad, &mut out[17..18]);
        out[18..].copy_from_slice(&tag);
    }

    /// The next packet due now: fills `out`, returns where to send it.
    pub fn poll_transmit(
        &mut self,
        now: Instant,
        out: &mut [u8; PUNCH_BYTES],
    ) -> Option<SocketAddr> {
        if let Some(to) = self.ack_to.take() {
            self.seal(ACK, out);
            return Some(to);
        }
        if self.state == PunchState::Probing && now >= self.deadline {
            self.state = PunchState::Failed;
        }
        // Keep probing a little after opening so the peer's side opens too.
        let probing = self.state == PunchState::Probing
            || (matches!(self.state, PunchState::Open(_)) && now < self.deadline);
        if !probing || now < self.next_round {
            return None;
        }
        let remotes = self.remotes.as_slice();
        let to = remotes[self.next_index];
        self.next_index += 1;
        if self.next_index == remotes.len() {
            self.next_index = 0;
            self.next_round = now + self.interval;
        }
        if let PunchState::Open(addr) = self.state
            && to != addr
        {
            return None;
        }
        self.seal(PROBE, out);
        Some(to)
    }

    /// Offers a received datagram; returns whether it was this punch's.
    pub fn handle(&mut self, now: Instant, from: SocketAddr, data: &[u8]) -> bool {
        if data.len() != PUNCH_BYTES
            || data[0] != PUNCH_PREFIX
            || data[1..9] != self.session.to_le_bytes()
        {
            return false;
        }
        let nonce_seq = u64::from_le_bytes(data[9..17].try_into().unwrap_or_default());
        // Only the other side's packets (and not our own reflected back).
        if (nonce_seq >> 63 == 1) == self.initiator {
            return false;
        }
        let mut aad = [0u8; 17];
        aad[..8].copy_from_slice(&VERSION);
        aad[8..17].copy_from_slice(&data[0..9]);
        let mut kind = [data[17]];
        let tag: [u8; TAG_BYTES] = data[18..].try_into().unwrap_or_default();
        if self
            .cipher
            .open(&sequence_nonce(nonce_seq), &aad, &mut kind, &tag)
            .is_err()
        {
            return false;
        }
        match kind[0] {
            PROBE => {
                self.heard_from = Some(from);
                self.ack_to = Some(from);
            }
            ACK => {
                if self.state != PunchState::Failed {
                    self.state = PunchState::Open(from);
                    // A short tail of probes for the peer's sake.
                    self.deadline = self.deadline.min(now + Duration::from_millis(500));
                }
            }
            _ => {}
        }
        true
    }
}

impl crate::ConnectToken {
    /// The hole-punching key of this token: both the client (holding the token) and the
    /// host (which issued it) can compute it, nobody else. Derived from the
    /// client-to-server key as AES-256-GCM keystream under a nonce packets never use.
    pub fn punch_key(&self) -> Key {
        derive_punch_key(&self.client_to_server_key)
    }
}

fn derive_punch_key(key: &Key) -> Key {
    let mut out = [0u8; 32];
    // Packet sequences stay below 2^62 (connections) or in [2^63, 2^63 + 2^62 + n)
    // (server handshakes): u64::MAX is never a packet nonce under this key.
    let _ = Cipher::new(key).seal(&sequence_nonce(u64::MAX), b"punch", &mut out);
    Key(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::DatagramSocket;
    use crate::memory::{MemoryNetwork, NatBehavior};

    fn punch(behavior: NatBehavior) -> (PunchState, PunchState) {
        let net = MemoryNetwork::new(5);
        let nat_a = net.add_nat("198.51.100.1".parse().unwrap(), behavior);
        let nat_b = net.add_nat("198.51.100.2".parse().unwrap(), NatBehavior::PortRestricted);
        let mut a = net.bind_behind_nat("192.168.1.2:7000".parse().unwrap(), nat_a);
        let mut b = net.bind_behind_nat("192.168.2.2:7000".parse().unwrap(), nat_b);
        let mut stun = net.bind("203.0.113.1:3478".parse().unwrap());
        let t0 = Instant::now();
        // Each side learns its mapping from a STUN server (as the probe would).
        let mut buf = [0u8; 64];
        a.send_to(t0, "203.0.113.1:3478".parse().unwrap(), b"?")
            .unwrap();
        b.send_to(t0, "203.0.113.1:3478".parse().unwrap(), b"?")
            .unwrap();
        let (_, a_pub) = stun.recv_from(t0, &mut buf).unwrap().unwrap();
        let (_, b_pub) = stun.recv_from(t0, &mut buf).unwrap().unwrap();
        let key = Key::random();
        let mut pa = Puncher::new(
            &key,
            9,
            true,
            AddressList::new(&["192.168.2.2:7000".parse().unwrap(), b_pub]).unwrap(),
            t0,
            Duration::from_secs(3),
        );
        let mut pb = Puncher::new(
            &key,
            9,
            false,
            AddressList::new(&["192.168.1.2:7000".parse().unwrap(), a_pub]).unwrap(),
            t0,
            Duration::from_secs(3),
        );
        let mut out = [0u8; PUNCH_BYTES];
        for ms in 0..4000 {
            let now = t0 + Duration::from_millis(ms);
            while let Some(to) = pa.poll_transmit(now, &mut out) {
                a.send_to(now, to, &out).unwrap();
            }
            while let Some(to) = pb.poll_transmit(now, &mut out) {
                b.send_to(now, to, &out).unwrap();
            }
            while let Some((n, from)) = a.recv_from(now, &mut buf).unwrap() {
                assert!(pa.handle(now, from, &buf[..n]));
            }
            while let Some((n, from)) = b.recv_from(now, &mut buf).unwrap() {
                assert!(pb.handle(now, from, &buf[..n]));
            }
        }
        (pa.state(), pb.state())
    }

    #[test]
    fn punches_through_cone_nats_but_not_symmetric_ones() {
        let (a, b) = punch(NatBehavior::PortRestricted);
        assert!(
            matches!(a, PunchState::Open(addr) if addr.ip().to_string() == "198.51.100.2"),
            "{a:?}"
        );
        assert!(
            matches!(b, PunchState::Open(addr) if addr.ip().to_string() == "198.51.100.1"),
            "{b:?}"
        );
        let (a, b) = punch(NatBehavior::Symmetric);
        assert_eq!(a, PunchState::Failed);
        assert_eq!(b, PunchState::Failed);
    }

    #[test]
    fn forged_probes_are_ignored() {
        let t0 = Instant::now();
        let remotes = AddressList::new(&["10.0.0.1:1".parse().unwrap()]).unwrap();
        let mut honest = Puncher::new(
            &Key::random(),
            3,
            false,
            remotes,
            t0,
            Duration::from_secs(1),
        );
        let mut forger = Puncher::new(&Key::random(), 3, true, remotes, t0, Duration::from_secs(1));
        let mut out = [0u8; PUNCH_BYTES];
        forger.poll_transmit(t0, &mut out).unwrap();
        assert!(!honest.handle(t0, "10.0.0.9:1".parse().unwrap(), &out));
        assert_eq!(honest.state(), PunchState::Probing);
    }
}
