//! Packet framing: the prefix byte, the sequence number, encryption, replay protection.
//!
//! ```text
//! connection request (plaintext, 1061 bytes):
//!   prefix 0x04 | version [8] | protocol id u64 | expire u64 | token nonce [12] | sealed private token [1024]
//! every other packet:
//!   prefix (seq bytes << 4 | kind) | sequence (1-8 bytes LE) | ciphertext | tag [16]
//!   additional data: version [8] | protocol id u64 | prefix
//! ```
//! Kinds 4-9 with at most 8 sequence bytes: a first byte of 0x00-0x03 (STUN) or anything
//! else is a foreign datagram (NAT traversal shares the socket).

use crate::crypto::{Cipher, NONCE_BYTES, TAG_BYTES, sequence_nonce};
use crate::token::{PRIVATE_TOKEN_BYTES, VERSION};

/// Packet kinds (low nibble of the prefix).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    Request = 4,
    Denied = 5,
    Challenge = 6,
    Response = 7,
    Payload = 8,
    Disconnect = 9,
}

impl Kind {
    fn from_nibble(n: u8) -> Option<Self> {
        Some(match n {
            4 => Self::Request,
            5 => Self::Denied,
            6 => Self::Challenge,
            7 => Self::Response,
            8 => Self::Payload,
            9 => Self::Disconnect,
            _ => return None,
        })
    }
}

/// Bytes of a connection request.
pub(crate) const REQUEST_BYTES: usize = 1 + 8 + 8 + 8 + NONCE_BYTES + PRIVATE_TOKEN_BYTES;
/// Most bytes a header takes (prefix + 8 sequence bytes).
pub(crate) const MAX_HEADER_BYTES: usize = 9;
/// Bytes every encrypted packet adds besides its header.
pub(crate) const OVERHEAD_BYTES: usize = TAG_BYTES;

/// Whether a datagram's first byte belongs to this protocol.
pub fn is_transport_packet(first: u8) -> bool {
    let kind = first & 0x0f;
    let seq_bytes = first >> 4;
    match kind {
        4 => seq_bytes == 0,
        5..=9 => (1..=8).contains(&seq_bytes),
        _ => false,
    }
}

/// Sequence bytes needed for `seq` (at least one).
fn sequence_bytes(seq: u64) -> usize {
    (8 - (seq.leading_zeros() as usize / 8)).max(1)
}

/// Header bytes of an encrypted packet with sequence `seq`.
pub(crate) fn header_len(seq: u64) -> usize {
    1 + sequence_bytes(seq)
}

fn aad(protocol_id: u64, prefix: u8) -> [u8; 17] {
    let mut a = [0u8; 17];
    a[..8].copy_from_slice(&VERSION);
    a[8..16].copy_from_slice(&protocol_id.to_le_bytes());
    a[16] = prefix;
    a
}

/// Finishes an encrypted packet whose plaintext is already at
/// `buf[header_len(seq)..header_len(seq) + len]`; returns the packet's length.
pub(crate) fn seal(buf: &mut [u8], kind: Kind, seq: u64, protocol_id: u64, cipher: &Cipher, len: usize) -> usize {
    let n = sequence_bytes(seq);
    let prefix = ((n as u8) << 4) | kind as u8;
    buf[0] = prefix;
    buf[1..1 + n].copy_from_slice(&seq.to_le_bytes()[..n]);
    let h = 1 + n;
    let tag = cipher.seal(&sequence_nonce(seq), &aad(protocol_id, prefix), &mut buf[h..h + len]);
    buf[h + len..h + len + TAG_BYTES].copy_from_slice(&tag);
    h + len + TAG_BYTES
}

/// The kind and sequence of an encrypted packet, and where its ciphertext starts.
pub(crate) fn parse_header(packet: &[u8]) -> Option<(Kind, u64, usize)> {
    let prefix = *packet.first()?;
    if !is_transport_packet(prefix) {
        return None;
    }
    let kind = Kind::from_nibble(prefix & 0x0f)?;
    if kind == Kind::Request {
        return Some((kind, 0, 0));
    }
    let n = usize::from(prefix >> 4);
    if packet.len() < 1 + n + TAG_BYTES {
        return None;
    }
    let mut seq = [0u8; 8];
    seq[..n].copy_from_slice(&packet[1..1 + n]);
    Some((kind, u64::from_le_bytes(seq), 1 + n))
}

/// Decrypts an encrypted packet in place; returns the plaintext range.
pub(crate) fn open(
    packet: &mut [u8],
    seq: u64,
    header: usize,
    protocol_id: u64,
    cipher: &Cipher,
) -> Option<std::ops::Range<usize>> {
    let prefix = packet[0];
    let len = cipher
        .open_appended(&sequence_nonce(seq), &aad(protocol_id, prefix), &mut packet[header..])
        .ok()?;
    Some(header..header + len)
}

/// Remembers the last 256 sequence numbers received (netcode's replay buffer): a packet
/// older than that window, or seen before, is dropped before decryption.
pub(crate) struct ReplayWindow {
    most_recent: u64,
    seen: [u64; 256],
}

impl ReplayWindow {
    pub(crate) const fn new() -> Self {
        Self {
            most_recent: 0,
            seen: [u64::MAX; 256],
        }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn is_replay(&self, seq: u64) -> bool {
        if seq.saturating_add(256) <= self.most_recent {
            return true;
        }
        let slot = self.seen[(seq % 256) as usize];
        slot != u64::MAX && slot >= seq
    }

    /// Records `seq` (after the packet authenticated).
    pub(crate) fn record(&mut self, seq: u64) {
        if seq > self.most_recent {
            self.most_recent = seq;
        }
        self.seen[(seq % 256) as usize] = seq;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::crypto::Key;

    #[test]
    fn packets_round_trip_and_reject_tampering() {
        let cipher = Cipher::new(&Key::random());
        for seq in [0u64, 1, 255, 256, 1 << 40, u64::MAX >> 2] {
            let mut buf = [0u8; 128];
            let h = header_len(seq);
            buf[h..h + 5].copy_from_slice(b"hello");
            let n = seal(&mut buf, Kind::Payload, seq, 7, &cipher, 5);
            let (kind, s, header) = parse_header(&buf[..n]).unwrap();
            assert_eq!((kind, s, header), (Kind::Payload, seq, h));
            let mut copy = buf;
            let range = open(&mut copy[..n], s, header, 7, &cipher).unwrap();
            assert_eq!(&copy[range], b"hello");
            // A different protocol id, a flipped bit anywhere, or a changed kind fail.
            let mut copy = buf;
            assert!(open(&mut copy[..n], s, header, 8, &cipher).is_none());
            for i in 0..n {
                let mut copy = buf;
                copy[i] ^= 0x01;
                if let Some((_, s2, h2)) = parse_header(&copy[..n]) {
                    assert!(open(&mut copy[..n], s2, h2, 7, &cipher).is_none(), "byte {i}");
                }
            }
        }
    }

    #[test]
    fn replays_are_detected() {
        let mut w = ReplayWindow::new();
        assert!(!w.is_replay(0));
        w.record(0);
        assert!(w.is_replay(0));
        w.record(10);
        assert!(!w.is_replay(5));
        w.record(5);
        assert!(w.is_replay(5));
        w.record(1000);
        assert!(w.is_replay(700));
        assert!(!w.is_replay(800));
        assert!(w.is_replay(10));
    }

    #[test]
    fn foreign_first_bytes() {
        // STUN messages start with 0x00 or 0x01.
        for b in 0..4u8 {
            assert!(!is_transport_packet(b));
        }
        assert!(is_transport_packet(0x04));
        assert!(!is_transport_packet(0x14));
        assert!(is_transport_packet(0x18));
        assert!(!is_transport_packet(0x98));
        assert!(!is_transport_packet(0xf1));
    }
}
