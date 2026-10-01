//! Acknowledgements, loss detection and round-trip estimates for one connection.
//!
//! Every payload packet carries the newest sequence received and a 32-bit mask of the 32
//! before it (Fiedler's reliable.io; an ack vector in GameNetworkingSockets' terms), plus
//! how long the receiver held the newest before acknowledging it. The sender keeps a
//! record of what each packet carried. A record is acknowledged once, or declared lost
//! with QUIC's rules (RFC 9002): three newer packets acknowledged, or acknowledged newer
//! packets and 9/8 of the round trip passed, or a probe timeout without any
//! acknowledgement.

use std::time::{Duration, Instant};

use crate::channel::{Entry, EntryList};

/// Sent-packet records kept per connection (a packet older than this is lost).
pub(crate) const SENT_RECORDS: usize = 256;
/// Unit of the acknowledgement delay on the wire.
const ACK_DELAY_UNIT_US: u64 = 8;

struct SentPacket {
    seq: u64,
    time: Instant,
    bytes: u32,
    in_flight: bool,
    entries: EntryList,
}

/// What became of a packet's entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Acked(Entry),
    Lost(Entry),
}

pub(crate) struct AckTracker {
    sent: Box<[SentPacket]>,
    /// No record below this sequence is in flight.
    oldest: u64,
    next: u64,
    largest_acked: Option<u64>,
    // Receive side.
    recv_largest: Option<u64>,
    recv_bits: u32,
    recv_time: Instant,
    pub(crate) ack_pending: bool,
    // Estimates, seconds.
    srtt: f64,
    rttvar: f64,
    min_rtt: f64,
    latest_rtt: f64,
    has_sample: bool,
    loss: f64,
    pub(crate) packets_lost: u64,
    pub(crate) packets_acked: u64,
    max_ack_delay: Duration,
}

impl AckTracker {
    pub(crate) fn new(now: Instant, max_ack_delay: Duration) -> Self {
        Self {
            sent: (0..SENT_RECORDS)
                .map(|_| SentPacket {
                    seq: 0,
                    time: now,
                    bytes: 0,
                    in_flight: false,
                    entries: EntryList::new(),
                })
                .collect(),
            oldest: 0,
            next: 0,
            largest_acked: None,
            recv_largest: None,
            recv_bits: 0,
            recv_time: now,
            ack_pending: false,
            srtt: 0.1,
            rttvar: 0.05,
            min_rtt: f64::MAX,
            latest_rtt: 0.1,
            has_sample: false,
            loss: 0.0,
            packets_lost: 0,
            packets_acked: 0,
            max_ack_delay,
        }
    }

    /// Starts over with the first packet numbered `first_seq` (send side).
    pub(crate) fn reset(&mut self, now: Instant, first_seq: u64) {
        for r in self.sent.iter_mut() {
            r.in_flight = false;
        }
        self.oldest = first_seq;
        self.next = first_seq;
        self.largest_acked = None;
        self.recv_largest = None;
        self.recv_bits = 0;
        self.recv_time = now;
        self.ack_pending = false;
        self.srtt = 0.1;
        self.rttvar = 0.05;
        self.min_rtt = f64::MAX;
        self.latest_rtt = 0.1;
        self.has_sample = false;
        self.loss = 0.0;
        self.packets_lost = 0;
        self.packets_acked = 0;
    }

    pub(crate) fn rtt(&self) -> Duration {
        Duration::from_secs_f64(self.srtt)
    }

    pub(crate) fn rtt_variance(&self) -> Duration {
        Duration::from_secs_f64(self.rttvar)
    }

    pub(crate) fn min_rtt(&self) -> Duration {
        if self.has_sample { Duration::from_secs_f64(self.min_rtt) } else { Duration::ZERO }
    }

    pub(crate) fn loss(&self) -> f32 {
        self.loss as f32
    }

    /// Records a packet sent with sequence `seq`.
    pub(crate) fn on_sent(
        &mut self,
        seq: u64,
        now: Instant,
        bytes: usize,
        entries: &EntryList,
        outcome: &mut impl FnMut(Outcome),
    ) {
        let slot = (seq % SENT_RECORDS as u64) as usize;
        if self.sent[slot].in_flight {
            // Unacknowledged for SENT_RECORDS packets.
            self.declare_lost(slot, outcome);
        }
        let r = &mut self.sent[slot];
        r.seq = seq;
        r.time = now;
        r.bytes = bytes as u32;
        r.in_flight = true;
        r.entries.items[..entries.len].copy_from_slice(entries.as_slice());
        r.entries.len = entries.len;
        self.next = seq + 1;
    }

    fn declare_lost(&mut self, slot: usize, outcome: &mut impl FnMut(Outcome)) {
        let r = &mut self.sent[slot];
        r.in_flight = false;
        for e in r.entries.as_slice() {
            outcome(Outcome::Lost(*e));
        }
        self.packets_lost += 1;
        self.loss += (1.0 - self.loss) * 0.05;
    }

    fn acked(&mut self, seq: u64, outcome: &mut impl FnMut(Outcome)) -> Option<Instant> {
        let slot = (seq % SENT_RECORDS as u64) as usize;
        let r = &mut self.sent[slot];
        if !r.in_flight || r.seq != seq {
            return None;
        }
        r.in_flight = false;
        for e in r.entries.as_slice() {
            outcome(Outcome::Acked(*e));
        }
        self.packets_acked += 1;
        self.loss -= self.loss * 0.05;
        Some(r.time)
    }

    /// Processes an acknowledgement header from the peer.
    pub(crate) fn on_ack(
        &mut self,
        largest_low: u16,
        bits: u32,
        delay_units: u16,
        now: Instant,
        outcome: &mut impl FnMut(Outcome),
    ) {
        if self.next == 0 {
            return;
        }
        let last = self.next - 1;
        let back = u64::from((last as u16).wrapping_sub(largest_low));
        let Some(largest) = last.checked_sub(back) else { return };
        if let Some(sent_at) = self.acked(largest, outcome) {
            let delay = Duration::from_micros(u64::from(delay_units) * ACK_DELAY_UNIT_US).min(self.max_ack_delay);
            self.rtt_sample(now.saturating_duration_since(sent_at), delay);
        }
        for i in 0..32 {
            if bits & (1 << i) != 0
                && let Some(seq) = largest.checked_sub(i + 1)
            {
                self.acked(seq, outcome);
            }
        }
        if self.largest_acked.is_none_or(|l| largest > l) {
            self.largest_acked = Some(largest);
        }
        self.detect_lost(now, outcome);
    }

    fn rtt_sample(&mut self, rtt: Duration, ack_delay: Duration) {
        let rtt = rtt.as_secs_f64();
        self.min_rtt = self.min_rtt.min(rtt);
        // Remove the receiver's holding time unless that would go below the minimum.
        let delay = ack_delay.as_secs_f64();
        let adjusted = if rtt - delay >= self.min_rtt { rtt - delay } else { rtt };
        self.latest_rtt = adjusted;
        if !self.has_sample {
            self.srtt = adjusted;
            self.rttvar = adjusted / 2.0;
            self.has_sample = true;
        } else {
            self.rttvar = 0.75 * self.rttvar + 0.25 * (self.srtt - adjusted).abs();
            self.srtt = 0.875 * self.srtt + 0.125 * adjusted;
        }
    }

    /// The probe timeout: how long an unacknowledged packet may wait.
    fn pto(&self) -> f64 {
        self.srtt + (4.0 * self.rttvar).max(0.001) + self.max_ack_delay.as_secs_f64()
    }

    /// Declares packets lost by the packet and time thresholds and the probe timeout.
    pub(crate) fn detect_lost(&mut self, now: Instant, outcome: &mut impl FnMut(Outcome)) {
        let time_threshold = self.srtt.max(self.latest_rtt) * 9.0 / 8.0;
        let pto = self.pto();
        let mut seq = self.oldest.max(self.next.saturating_sub(SENT_RECORDS as u64));
        let mut all_done = true;
        while seq < self.next {
            let slot = (seq % SENT_RECORDS as u64) as usize;
            let r = &self.sent[slot];
            if r.in_flight && r.seq == seq {
                let age = now.saturating_duration_since(r.time).as_secs_f64();
                let newer_acked = self.largest_acked.is_some_and(|l| l > seq);
                let lost = self.largest_acked.is_some_and(|l| l >= seq + 3)
                    || (newer_acked && age > time_threshold)
                    || age > pto;
                if lost {
                    self.declare_lost(slot, outcome);
                } else if all_done {
                    self.oldest = seq;
                    all_done = false;
                }
            }
            seq += 1;
        }
        if all_done {
            self.oldest = self.next;
        }
    }

    /// Records a payload packet received with sequence `seq`.
    pub(crate) fn on_received(&mut self, seq: u64, now: Instant) {
        match self.recv_largest {
            None => {
                self.recv_largest = Some(seq);
                self.recv_bits = 0;
                self.recv_time = now;
            }
            Some(largest) if seq > largest => {
                let shift = seq - largest;
                let bits = (u64::from(self.recv_bits) << 1 | 1) << (shift - 1).min(63);
                self.recv_bits = if shift > 32 { 0 } else { bits as u32 };
                self.recv_largest = Some(seq);
                self.recv_time = now;
            }
            Some(largest) => {
                let back = largest - seq;
                if (1..=32).contains(&back) {
                    self.recv_bits |= 1 << (back - 1);
                }
            }
        }
        self.ack_pending = true;
    }

    /// The acknowledgement header to send now: newest, mask, delay in 8 µs units.
    pub(crate) fn ack_header(&self, now: Instant) -> Option<(u16, u32, u16)> {
        let largest = self.recv_largest?;
        let delay = now.saturating_duration_since(self.recv_time).as_micros() as u64 / ACK_DELAY_UNIT_US;
        Some((largest as u16, self.recv_bits, delay.min(u64::from(u16::MAX)) as u16))
    }

    /// Bytes sent in packets still in flight.
    pub(crate) fn bytes_in_flight(&self) -> u64 {
        self.sent.iter().filter(|r| r.in_flight).map(|r| u64::from(r.bytes)).sum()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn entries(id: u32) -> EntryList {
        let mut e = EntryList::new();
        e.push(Entry { channel: 0, id });
        e
    }

    #[test]
    fn acks_and_losses_resolve_every_packet_once() {
        let t0 = Instant::now();
        let mut sender = AckTracker::new(t0, Duration::from_millis(25));
        let mut receiver = AckTracker::new(t0, Duration::from_millis(25));
        sender.reset(t0, 1_000_000);
        let mut acked = Vec::new();
        let mut lost = Vec::new();
        let mut record = |o: Outcome| match o {
            Outcome::Acked(e) => acked.push(e.id),
            Outcome::Lost(e) => lost.push(e.id),
        };
        // Send 100 packets 10 ms apart; drop every 7th; ack each arrival 30 ms later.
        for i in 0..100u32 {
            let now = t0 + Duration::from_millis(10 * u64::from(i));
            sender.on_sent(1_000_000 + u64::from(i), now, 100, &entries(i), &mut record);
            if i % 7 != 3 {
                receiver.on_received(1_000_000 + u64::from(i), now + Duration::from_millis(15));
            }
            let (l, b, d) = receiver.ack_header(now + Duration::from_millis(15)).unwrap();
            sender.on_ack(l, b, d, now + Duration::from_millis(30), &mut record);
        }
        sender.detect_lost(t0 + Duration::from_secs(5), &mut record);
        let mut all: Vec<u32> = acked.iter().chain(lost.iter()).copied().collect();
        all.sort_unstable();
        assert_eq!(all, (0..100).collect::<Vec<_>>());
        assert!(lost.iter().all(|i| i % 7 == 3), "{lost:?}");
        assert_eq!(lost.len(), 14);
        let rtt = sender.rtt().as_secs_f64();
        assert!((0.025..0.035).contains(&rtt), "rtt {rtt}");
    }
}
