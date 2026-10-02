//! Message layouts shared by server and client (ADR 0074).
//!
//! Channels ([`crate::NetSchema::endpoint_config`]): 0 reliable-ordered (welcome,
//! reliable events), 1 unreliable with delivery notification (snapshots), 2 unreliable
//! (inputs, unreliable events). Every message starts with a tag byte.
//!
//! - `WELCOME` (S→C, reliable): schema hash `u64`, client index `u16`, tick `u32`, tick
//!   rate `f32`.
//! - `EVENT` (both ways): type `u16`, tick `u32`, the event's bytes.
//! - `SNAPSHOT` (S→C): tick `u32`, then bits: input acknowledgement, input lead, default
//!   baseline, and entity records sorted by slot (see `server::encode_entity`).
//! - `INPUT` (C→S): newest tick `u32`, count `u8`, then bits per input, oldest first:
//!   view delay and payload, each with a "same as the previous" bit.

use engine_net_transport::ChannelId;

pub(crate) const CH_RELIABLE: ChannelId = ChannelId(0);
pub(crate) const CH_SNAPSHOT: ChannelId = ChannelId(1);
pub(crate) const CH_UNRELIABLE: ChannelId = ChannelId(2);

pub(crate) const TAG_WELCOME: u8 = 1;
pub(crate) const TAG_EVENT: u8 = 2;
pub(crate) const TAG_SNAPSHOT: u8 = 3;
pub(crate) const TAG_INPUT: u8 = 4;

/// Record kinds in a snapshot (2 bits).
pub(crate) const KIND_DELTA: u32 = 0;
pub(crate) const KIND_FULL: u32 = 1;
pub(crate) const KIND_REMOVE: u32 = 2;

/// Exponential-Golomb orders: slot gaps, deltas of integer words, full integer words.
pub(crate) const K_GAP: u32 = 1;
pub(crate) const K_DELTA: u32 = 2;
pub(crate) const K_FULL: u32 = 6;

/// Server frames kept for baselines: a baseline older than this many ticks is not used
/// (the entity is sent whole). The client keeps twice as many decoded snapshots.
pub(crate) const HISTORY: u32 = 32;
pub(crate) const CLIENT_FRAMES: u32 = 64;
/// Inputs the server buffers per client, by tick.
pub(crate) const INPUT_RING: u32 = 64;
/// Inputs at most in one input message (redundancy).
pub(crate) const MAX_REDUNDANT_INPUTS: u32 = 32;
/// Snapshots per client whose delivery is awaited.
pub(crate) const SENT_RING: usize = 256;

/// Bytes of an event message before the payload.
pub(crate) const EVENT_HEADER: usize = 7;

pub(crate) fn event_message(out: &mut Vec<u8>, kind: u16, tick: u32, payload: &[u8]) {
    out.clear();
    out.push(TAG_EVENT);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&tick.to_le_bytes());
    out.extend_from_slice(payload);
}

/// `(type, tick, payload)` of an event message.
pub(crate) fn parse_event(msg: &[u8]) -> Option<(u16, u32, &[u8])> {
    if msg.len() < EVENT_HEADER || msg[0] != TAG_EVENT {
        return None;
    }
    let kind = u16::from_le_bytes([msg[1], msg[2]]);
    let tick = u32::from_le_bytes([msg[3], msg[4], msg[5], msg[6]]);
    Some((kind, tick, &msg[EVENT_HEADER..]))
}

/// Received events of one update, by type, for the game to read.
#[derive(Debug, Default)]
pub(crate) struct Inbox {
    /// `(type, sender, tick, start, len)` into `bytes`.
    pub(crate) records: Vec<(u16, u16, u32, u32, u32)>,
    pub(crate) bytes: Vec<u8>,
}

impl Inbox {
    pub(crate) fn clear(&mut self) {
        self.records.clear();
        self.bytes.clear();
    }

    pub(crate) fn push(&mut self, kind: u16, sender: u16, tick: u32, payload: &[u8]) {
        let start = self.bytes.len() as u32;
        self.bytes.extend_from_slice(payload);
        self.records
            .push((kind, sender, tick, start, payload.len() as u32));
    }

    /// Calls `f(sender, tick, event)` for every event of type index `kind` with `size`
    /// bytes.
    pub(crate) fn read<E: bytemuck::Pod>(&self, kind: u16, mut f: impl FnMut(u16, u32, E)) {
        for &(k, sender, tick, start, len) in &self.records {
            if k == kind && len as usize == core::mem::size_of::<E>() {
                let bytes = &self.bytes[start as usize..(start + len) as usize];
                f(sender, tick, bytemuck::pod_read_unaligned(bytes));
            }
        }
    }
}
