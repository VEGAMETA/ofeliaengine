//! Per-connection channel buffers. Everything is allocated when the connection's slot is
//! first created and reused afterwards: no allocation per message or packet.
//!
//! - Reliable-ordered channels are a byte stream (as in QUIC and GameNetworkingSockets):
//!   each message is `varint length + bytes` in the stream; packets carry stream ranges
//!   ("chunks"), lost chunks are sent again, the receiver writes ranges into a window at
//!   their offsets and hands out messages once their bytes are contiguous. Large messages
//!   need no separate fragmentation; small ones share chunks.
//! - Unreliable channels queue the messages of one flush; a message larger than
//!   [`FRAGMENT_BYTES`] travels in fragments and is delivered only if every fragment
//!   arrives. Unreliable-sequenced channels drop anything older than the newest delivered.

use crate::bytes::{Reader, Writer, varint_len};

/// Bytes of one fragment of a large unreliable message.
pub const FRAGMENT_BYTES: usize = 1024;
/// Chunks of a reliable stream in flight at once (per channel).
const CHUNKS: usize = 1024;
/// Notification slots per channel: unresolved notified messages before the oldest is
/// reported lost.
const NOTIFY_SLOTS: usize = 1024;
/// Unreliable messages of one flush per channel.
const MAX_QUEUED_MESSAGES: usize = 1024;
/// Unreliable messages being reassembled at once per channel.
const REASSEMBLY_SLOTS: usize = 2;

/// How a channel delivers messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    /// May be lost, duplicated never, order not kept. Snapshots with their own ticks.
    Unreliable,
    /// May be lost; anything older than the newest delivered message is dropped.
    UnreliableSequenced,
    /// Every message arrives, once, in order. Events, chat, RPCs.
    ReliableOrdered,
}

/// One channel's configuration. Client and server must use the same list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelConfig {
    /// Delivery guarantees.
    pub kind: ChannelKind,
    /// The largest message, bytes. Reliable: at most `buffer_bytes - 5`.
    pub max_message_size: usize,
    /// Reliable: the stream window in bytes (a power of two), on both sides: how much may
    /// be unacknowledged (sender) or unread (receiver). Unreliable: bytes of messages
    /// queued per flush, and bytes of received messages waiting to be read.
    pub buffer_bytes: usize,
    /// Unreliable channels: report [`crate::Event::Delivered`] / [`crate::Event::Lost`]
    /// for every message (Tribes-style delivery notification for replication).
    pub notify: bool,
}

impl ChannelConfig {
    /// A reliable-ordered channel with a 64 KiB window.
    pub const fn reliable() -> Self {
        Self {
            kind: ChannelKind::ReliableOrdered,
            max_message_size: 32 * 1024,
            buffer_bytes: 64 * 1024,
            notify: false,
        }
    }

    /// An unreliable channel: messages up to 16 KiB, 64 KiB per flush.
    pub const fn unreliable() -> Self {
        Self {
            kind: ChannelKind::Unreliable,
            max_message_size: 16 * 1024,
            buffer_bytes: 64 * 1024,
            notify: false,
        }
    }

    /// An unreliable-sequenced channel: messages up to 16 KiB, 64 KiB per flush.
    pub const fn unreliable_sequenced() -> Self {
        Self {
            kind: ChannelKind::UnreliableSequenced,
            ..Self::unreliable()
        }
    }

    /// The same channel with delivery notifications on.
    pub const fn with_notify(mut self) -> Self {
        self.notify = true;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        match self.kind {
            ChannelKind::ReliableOrdered => {
                if !self.buffer_bytes.is_power_of_two() || self.buffer_bytes < 4096 {
                    return Err("a reliable channel's buffer_bytes must be a power of two >= 4096");
                }
                if self.buffer_bytes > 1 << 30 {
                    return Err("a reliable channel's buffer_bytes must be at most 1 GiB");
                }
                if self.max_message_size + 5 > self.buffer_bytes {
                    return Err(
                        "a reliable channel's max_message_size must be <= buffer_bytes - 5",
                    );
                }
                if self.notify {
                    return Err("delivery notification is for unreliable channels");
                }
            }
            ChannelKind::Unreliable | ChannelKind::UnreliableSequenced => {
                if self.max_message_size > 0xffff * FRAGMENT_BYTES {
                    return Err("an unreliable message is at most 65535 fragments");
                }
                if self.buffer_bytes < self.max_message_size + 4 {
                    return Err("an unreliable channel's buffer_bytes must hold one message");
                }
            }
        }
        Ok(())
    }
}

/// Why a message was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// No such connection (never existed, or disconnected).
    #[error("no such connection")]
    NotConnected,
    /// No such channel.
    #[error("no such channel")]
    NoChannel,
    /// The message exceeds the channel's `max_message_size`.
    #[error("message larger than the channel's max_message_size")]
    TooLarge,
    /// The channel's buffer is full: unacknowledged reliable data, or unreliable messages
    /// of this flush. Try again after the next flush.
    #[error("channel buffer full")]
    QueueFull,
}

/// A message id within one channel of one connection (wraps after 2^32 messages).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(pub u32);

/// Something a packet carried that needs to know whether the packet arrived.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) channel: u8,
    /// Reliable: chunk id. Unreliable: message id.
    pub(crate) id: u32,
}

/// A delivery outcome of a notified unreliable message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    Delivered(MessageId),
    Lost(MessageId),
}

// Record header byte: bits 0-2 channel, bit 3 FRAG (unreliable: fragment fields follow;
// reliable: unused), bits 4-7 zero.
pub(crate) const RECORD_FRAG: u8 = 0x08;
/// The most channels a connection may have.
pub const MAX_CHANNELS: usize = 8;

/// Copies `data` into a power-of-two ring at stream position `pos`.
fn ring_write(ring: &mut [u8], pos: u64, data: &[u8]) {
    let mask = ring.len() - 1;
    let start = (pos as usize) & mask;
    let first = data.len().min(ring.len() - start);
    ring[start..start + first].copy_from_slice(&data[..first]);
    ring[..data.len() - first].copy_from_slice(&data[first..]);
}

/// Copies from a power-of-two ring at stream position `pos` into `out`.
fn ring_read(ring: &[u8], pos: u64, out: &mut [u8]) {
    let mask = ring.len() - 1;
    let start = (pos as usize) & mask;
    let first = out.len().min(ring.len() - start);
    out[..first].copy_from_slice(&ring[start..start + first]);
    let rest = out.len() - first;
    out[first..].copy_from_slice(&ring[..rest]);
}

// ---------------------------------------------------------------------------------------
// Reliable stream: sender
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChunkState {
    Free,
    InFlight,
    Lost,
    Acked,
}

#[derive(Clone, Copy, Debug)]
struct Chunk {
    id: u32,
    offset: u64,
    len: u32,
    state: ChunkState,
}

pub(crate) struct ReliableSend {
    ring: Box<[u8]>,
    max_message: usize,
    /// End of the queued stream.
    written: u64,
    /// Everything below is acknowledged and its ring space free.
    acked_until: u64,
    /// Everything below has been sent at least once.
    sent_until: u64,
    chunks: Box<[Chunk]>,
    /// Next chunk id.
    head: u32,
    /// Oldest live chunk id.
    tail: u32,
    lost: u32,
    messages: u32,
}

/// Record header bytes for a reliable chunk of `len` bytes: header, offset, length.
fn chunk_header(len: usize) -> usize {
    1 + 4 + varint_len(len as u32)
}

impl ReliableSend {
    pub(crate) fn new(config: &ChannelConfig) -> Self {
        Self {
            ring: vec![0u8; config.buffer_bytes].into_boxed_slice(),
            max_message: config.max_message_size,
            written: 0,
            acked_until: 0,
            sent_until: 0,
            chunks: vec![
                Chunk {
                    id: 0,
                    offset: 0,
                    len: 0,
                    state: ChunkState::Free
                };
                CHUNKS
            ]
            .into_boxed_slice(),
            head: 0,
            tail: 0,
            lost: 0,
            messages: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.messages = 0;
        self.written = 0;
        self.acked_until = 0;
        self.sent_until = 0;
        self.head = 0;
        self.tail = 0;
        self.lost = 0;
        for c in self.chunks.iter_mut() {
            c.state = ChunkState::Free;
        }
    }

    pub(crate) fn queue(&mut self, message: &[u8]) -> Result<MessageId, SendError> {
        if message.len() > self.max_message {
            return Err(SendError::TooLarge);
        }
        let mut header = [0u8; 5];
        let mut w = Writer::new(&mut header);
        w.varint(message.len() as u32).ok_or(SendError::TooLarge)?;
        let header_len = w.pos();
        let need = (header_len + message.len()) as u64;
        if self.written + need - self.acked_until > self.ring.len() as u64 {
            return Err(SendError::QueueFull);
        }
        ring_write(&mut self.ring, self.written, &header[..header_len]);
        ring_write(&mut self.ring, self.written + header_len as u64, message);
        self.written += need;
        let id = MessageId(self.messages);
        self.messages = self.messages.wrapping_add(1);
        Ok(id)
    }

    /// Bytes queued but not yet acknowledged.
    pub(crate) fn unacked(&self) -> u64 {
        self.written - self.acked_until
    }

    /// Whether anything waits to be sent (new bytes or lost chunks).
    pub(crate) fn has_pending(&self) -> bool {
        self.lost > 0
            || (self.sent_until < self.written && self.head.wrapping_sub(self.tail) < CHUNKS as u32)
    }

    /// Writes lost chunks again, then new bytes, while they fit. `entries` gets one entry
    /// per chunk; returns `false` once the packet or the entry list is full.
    pub(crate) fn write(
        &mut self,
        channel: u8,
        w: &mut Writer<'_>,
        entries: &mut EntryList,
    ) -> bool {
        // Resends first, oldest first.
        if self.lost > 0 {
            let mut id = self.tail;
            while id != self.head && self.lost > 0 {
                let idx = id as usize & (CHUNKS - 1);
                let chunk = self.chunks[idx];
                if chunk.state == ChunkState::Lost {
                    let need = chunk_header(chunk.len as usize) + chunk.len as usize;
                    if need > w.remaining() || entries.is_full() {
                        return false;
                    }
                    self.write_chunk(channel, w, chunk.offset, chunk.len as usize);
                    self.chunks[idx].state = ChunkState::InFlight;
                    self.lost -= 1;
                    entries.push(Entry { channel, id });
                }
                id = id.wrapping_add(1);
            }
        }
        // New data.
        while self.sent_until < self.written {
            if self.head.wrapping_sub(self.tail) >= CHUNKS as u32 || entries.is_full() {
                return false;
            }
            let room = w.remaining();
            if room < chunk_header(1) + 1 {
                return false;
            }
            let pending = (self.written - self.sent_until).min(u32::MAX as u64) as usize;
            let mut len = pending.min(room - chunk_header(pending.min(room)));
            // The length varint may be one byte shorter than assumed; never longer.
            while chunk_header(len) + len > room {
                len -= 1;
            }
            if len == 0 {
                return false;
            }
            let offset = self.sent_until;
            self.write_chunk(channel, w, offset, len);
            let id = self.head;
            self.chunks[id as usize & (CHUNKS - 1)] = Chunk {
                id,
                offset,
                len: len as u32,
                state: ChunkState::InFlight,
            };
            self.head = id.wrapping_add(1);
            self.sent_until += len as u64;
            entries.push(Entry { channel, id });
        }
        true
    }

    fn write_chunk(&self, channel: u8, w: &mut Writer<'_>, offset: u64, len: usize) {
        // Space was checked by the caller.
        let ok = (|| {
            w.u8(channel)?;
            w.u32(offset as u32)?;
            w.varint(len as u32)?;
            let start = w.pos();
            w.zeros(len)?;
            Some(start)
        })();
        if let Some(start) = ok {
            ring_read(&self.ring, offset, w.slice_mut(start, len));
        }
    }

    pub(crate) fn on_acked(&mut self, id: u32) {
        let idx = id as usize & (CHUNKS - 1);
        let chunk = &mut self.chunks[idx];
        if chunk.id != id {
            return;
        }
        match chunk.state {
            ChunkState::InFlight => chunk.state = ChunkState::Acked,
            ChunkState::Lost => {
                chunk.state = ChunkState::Acked;
                self.lost -= 1;
            }
            ChunkState::Free | ChunkState::Acked => return,
        }
        // Free acknowledged chunks from the front.
        while self.tail != self.head {
            let idx = self.tail as usize & (CHUNKS - 1);
            let chunk = &mut self.chunks[idx];
            if chunk.state != ChunkState::Acked {
                break;
            }
            self.acked_until = chunk.offset + u64::from(chunk.len);
            chunk.state = ChunkState::Free;
            self.tail = self.tail.wrapping_add(1);
        }
    }

    pub(crate) fn on_lost(&mut self, id: u32) {
        let chunk = &mut self.chunks[id as usize & (CHUNKS - 1)];
        if chunk.id == id && chunk.state == ChunkState::InFlight {
            chunk.state = ChunkState::Lost;
            self.lost += 1;
        }
    }
}

// ---------------------------------------------------------------------------------------
// Reliable stream: receiver
// ---------------------------------------------------------------------------------------

pub(crate) struct ReliableRecv {
    ring: Box<[u8]>,
    /// One bit per ring byte: received at a position at or above `contiguous`.
    bits: Box<[u64]>,
    max_message: usize,
    /// Next byte for the application.
    read: u64,
    /// Everything below has been received.
    contiguous: u64,
    /// Bytes of the message handed out last, consumed on the next `receive`.
    pending: u64,
    scratch: Box<[u8]>,
    /// The peer sent a message longer than allowed.
    pub(crate) broken: bool,
}

impl ReliableRecv {
    pub(crate) fn new(config: &ChannelConfig) -> Self {
        Self {
            ring: vec![0u8; config.buffer_bytes].into_boxed_slice(),
            bits: vec![0u64; config.buffer_bytes / 64].into_boxed_slice(),
            max_message: config.max_message_size,
            read: 0,
            contiguous: 0,
            pending: 0,
            scratch: vec![0u8; config.max_message_size].into_boxed_slice(),
            broken: false,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.bits.fill(0);
        self.read = 0;
        self.contiguous = 0;
        self.pending = 0;
        self.broken = false;
    }

    /// The stream range a chunk at wire offset `wire` covers.
    fn range(&self, wire: u32, len: usize) -> (i128, i128) {
        let delta = i128::from(wire.wrapping_sub(self.read as u32) as i32);
        let start = i128::from(self.read) + delta;
        (start, start + len as i128)
    }

    /// Whether a chunk can be accepted (it is old, or fits in the window).
    pub(crate) fn fits(&self, wire: u32, len: usize) -> bool {
        let (_, end) = self.range(wire, len);
        end <= i128::from(self.read) + self.ring.len() as i128
    }

    /// Stores a chunk (call `fits` first).
    pub(crate) fn accept(&mut self, wire: u32, data: &[u8]) {
        let (start, end) = self.range(wire, data.len());
        let from = start.max(i128::from(self.contiguous));
        if end <= from {
            return;
        }
        let from = from as u64;
        let skip = (from as i128 - start) as usize;
        ring_write(&mut self.ring, from, &data[skip..]);
        self.set_bits(from, end as u64);
        // Advance over received bytes, clearing their bits for the next lap of the ring.
        let mask = self.ring.len() as u64 - 1;
        loop {
            let pos = self.contiguous & mask;
            let word = (pos / 64) as usize;
            let bit = pos % 64;
            let run = (!(self.bits[word] >> bit)).trailing_zeros() as u64;
            let run = run.min(64 - bit);
            if run == 0 {
                break;
            }
            let clear = if run == 64 {
                u64::MAX
            } else {
                ((1u64 << run) - 1) << bit
            };
            self.bits[word] &= !clear;
            self.contiguous += run;
        }
    }

    fn set_bits(&mut self, from: u64, to: u64) {
        let mask = self.ring.len() as u64 - 1;
        let mut pos = from;
        while pos < to {
            let p = pos & mask;
            let word = (p / 64) as usize;
            let bit = p % 64;
            let n = (to - pos).min(64 - bit);
            let set = if n == 64 {
                u64::MAX
            } else {
                ((1u64 << n) - 1) << bit
            };
            self.bits[word] |= set;
            pos += n;
        }
    }

    /// The next complete message, if any; valid until the next call.
    pub(crate) fn receive(&mut self) -> Option<&[u8]> {
        self.read += self.pending;
        self.pending = 0;
        let available = self.contiguous - self.read;
        if available == 0 {
            return None;
        }
        let mut header = [0u8; 5];
        let header_len = (available as usize).min(5);
        ring_read(&self.ring, self.read, &mut header[..header_len]);
        let mut r = Reader::new(&header[..header_len]);
        let Some(len) = r.varint() else {
            if header_len == 5 {
                self.broken = true;
            }
            return None;
        };
        let len = len as usize;
        if len > self.max_message {
            self.broken = true;
            return None;
        }
        let hl = r.pos() as u64;
        if available < hl + len as u64 {
            return None;
        }
        self.pending = hl + len as u64;
        let mask = self.ring.len() - 1;
        let start = ((self.read + hl) as usize) & mask;
        if start + len <= self.ring.len() {
            Some(&self.ring[start..start + len])
        } else {
            ring_read(&self.ring, self.read + hl, &mut self.scratch[..len]);
            Some(&self.scratch[..len])
        }
    }
}

// ---------------------------------------------------------------------------------------
// Unreliable: sender
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Queued {
    offset: u32,
    len: u32,
    id: u32,
    seq: u16,
    /// Fragments written so far (unfragmented: 0 or 1).
    next_fragment: u16,
}

#[derive(Clone, Copy, Debug, Default)]
struct NotifySlot {
    id: u32,
    remaining: u16,
    live: bool,
}

pub(crate) struct UnreliableSend {
    data: Box<[u8]>,
    used: usize,
    queue: Vec<Queued>,
    /// First queued message not completely written in this flush.
    cursor: usize,
    sequenced: bool,
    max_message: usize,
    next_seq: u16,
    next_id: u32,
    notify: Option<Box<[NotifySlot]>>,
}

fn fragment_count(len: usize) -> usize {
    if len <= FRAGMENT_BYTES {
        1
    } else {
        len.div_ceil(FRAGMENT_BYTES)
    }
}

impl UnreliableSend {
    pub(crate) fn new(config: &ChannelConfig) -> Self {
        Self {
            data: vec![0u8; config.buffer_bytes].into_boxed_slice(),
            used: 0,
            queue: Vec::with_capacity(MAX_QUEUED_MESSAGES),
            cursor: 0,
            sequenced: config.kind == ChannelKind::UnreliableSequenced,
            max_message: config.max_message_size,
            next_seq: 0,
            next_id: 0,
            notify: config
                .notify
                .then(|| vec![NotifySlot::default(); NOTIFY_SLOTS].into_boxed_slice()),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.used = 0;
        self.queue.clear();
        self.cursor = 0;
        self.next_seq = 0;
        self.next_id = 0;
        if let Some(n) = &mut self.notify {
            n.fill(NotifySlot::default());
        }
    }

    pub(crate) fn queue(
        &mut self,
        message: &[u8],
        notices: &mut impl FnMut(Notice),
    ) -> Result<MessageId, SendError> {
        if message.len() > self.max_message {
            return Err(SendError::TooLarge);
        }
        if self.used + message.len() > self.data.len() || self.queue.len() == MAX_QUEUED_MESSAGES {
            return Err(SendError::QueueFull);
        }
        let id = self.next_id;
        self.next_id = id.wrapping_add(1);
        self.data[self.used..self.used + message.len()].copy_from_slice(message);
        let fragmented = message.len() > FRAGMENT_BYTES;
        let seq = self.next_seq;
        if self.sequenced || fragmented {
            self.next_seq = seq.wrapping_add(1);
        }
        self.queue.push(Queued {
            offset: self.used as u32,
            len: message.len() as u32,
            id,
            seq,
            next_fragment: 0,
        });
        self.used += message.len();
        if let Some(slots) = &mut self.notify {
            let slot = &mut slots[id as usize & (NOTIFY_SLOTS - 1)];
            if slot.live {
                // Unresolved after NOTIFY_SLOTS newer messages: report it lost.
                notices(Notice::Lost(MessageId(slot.id)));
            }
            *slot = NotifySlot {
                id,
                remaining: fragment_count(message.len()) as u16,
                live: true,
            };
        }
        Ok(MessageId(id))
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.cursor < self.queue.len()
    }

    /// Writes queued messages (or their fragments) while they fit. Returns `false` once
    /// the packet or entry list is full.
    pub(crate) fn write(
        &mut self,
        channel: u8,
        w: &mut Writer<'_>,
        entries: &mut EntryList,
    ) -> bool {
        let notify = self.notify.is_some();
        while self.cursor < self.queue.len() {
            let m = self.queue[self.cursor];
            let len = m.len as usize;
            if notify && entries.is_full() {
                return false;
            }
            if len <= FRAGMENT_BYTES {
                let need = 1 + if self.sequenced { 2 } else { 0 } + varint_len(m.len) + len;
                if need > w.remaining() {
                    return false;
                }
                let ok = (|| {
                    w.u8(channel)?;
                    if self.sequenced {
                        w.u16(m.seq)?;
                    }
                    w.varint(m.len)?;
                    w.bytes(&self.data[m.offset as usize..m.offset as usize + len])
                })();
                debug_assert!(ok.is_some());
                if notify {
                    entries.push(Entry { channel, id: m.id });
                }
                self.cursor += 1;
            } else {
                let count = fragment_count(len);
                loop {
                    let index = self.queue[self.cursor].next_fragment as usize;
                    if index >= count {
                        break;
                    }
                    let start = index * FRAGMENT_BYTES;
                    let flen = (len - start).min(FRAGMENT_BYTES);
                    let need = 1
                        + 2
                        + varint_len(index as u32)
                        + varint_len(count as u32)
                        + varint_len(flen as u32)
                        + flen;
                    if need > w.remaining() || (notify && entries.is_full()) {
                        return false;
                    }
                    let from = m.offset as usize + start;
                    let ok = (|| {
                        w.u8(channel | RECORD_FRAG)?;
                        w.u16(m.seq)?;
                        w.varint(index as u32)?;
                        w.varint(count as u32)?;
                        w.varint(flen as u32)?;
                        w.bytes(&self.data[from..from + flen])
                    })();
                    debug_assert!(ok.is_some());
                    if notify {
                        entries.push(Entry { channel, id: m.id });
                    }
                    self.queue[self.cursor].next_fragment += 1;
                    if index + 1 == count {
                        break;
                    }
                }
                self.cursor += 1;
            }
        }
        true
    }

    /// Ends a flush: messages not written are dropped (reported lost when notified).
    pub(crate) fn end_flush(&mut self, notices: &mut impl FnMut(Notice)) -> usize {
        let dropped = self.queue.len() - self.cursor;
        if self.notify.is_some() {
            // Fragments of some may have gone out: those messages are lost either way.
            for i in self.cursor..self.queue.len() {
                let id = self.queue[i].id;
                self.resolve(id, false, notices);
            }
        }
        self.queue.clear();
        self.cursor = 0;
        self.used = 0;
        dropped
    }

    fn resolve(&mut self, id: u32, delivered: bool, notices: &mut impl FnMut(Notice)) {
        let Some(slots) = &mut self.notify else {
            return;
        };
        let slot = &mut slots[id as usize & (NOTIFY_SLOTS - 1)];
        if !slot.live || slot.id != id {
            return;
        }
        if delivered {
            slot.remaining = slot.remaining.saturating_sub(1);
            if slot.remaining > 0 {
                return;
            }
            notices(Notice::Delivered(MessageId(id)));
        } else {
            notices(Notice::Lost(MessageId(id)));
        }
        slot.live = false;
    }

    pub(crate) fn on_acked(&mut self, id: u32, notices: &mut impl FnMut(Notice)) {
        self.resolve(id, true, notices);
    }

    pub(crate) fn on_lost(&mut self, id: u32, notices: &mut impl FnMut(Notice)) {
        self.resolve(id, false, notices);
    }
}

// ---------------------------------------------------------------------------------------
// Unreliable: receiver
// ---------------------------------------------------------------------------------------

/// A FIFO of variable-size messages kept contiguous (a bip buffer): `[u32 len][bytes]`.
pub(crate) struct DeliveryRing {
    buf: Box<[u8]>,
    head: usize,
    tail: usize,
    /// End of the data before the wrap, while wrapped.
    wrap: usize,
    wrapped: bool,
    count: usize,
    pending: usize,
}

impl DeliveryRing {
    pub(crate) fn new(bytes: usize) -> Self {
        Self {
            buf: vec![0u8; bytes].into_boxed_slice(),
            head: 0,
            tail: 0,
            wrap: 0,
            wrapped: false,
            count: 0,
            pending: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.wrap = 0;
        self.wrapped = false;
        self.count = 0;
        self.pending = 0;
    }

    /// Reserves `len` bytes for a message and returns them, or `None` when full.
    pub(crate) fn push_with(&mut self, len: usize) -> Option<&mut [u8]> {
        let n = len + 4;
        let at = if !self.wrapped {
            if self.head + n <= self.buf.len() {
                self.head
            } else if n < self.tail {
                self.wrap = self.head;
                self.wrapped = true;
                0
            } else {
                return None;
            }
        } else if self.head + n < self.tail {
            self.head
        } else {
            return None;
        };
        self.buf[at..at + 4].copy_from_slice(&(len as u32).to_le_bytes());
        self.head = at + n;
        self.count += 1;
        Some(&mut self.buf[at + 4..at + n])
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> bool {
        match self.push_with(data.len()) {
            Some(slot) => {
                slot.copy_from_slice(data);
                true
            }
            None => false,
        }
    }

    fn pop(&mut self, n: usize) {
        self.tail += n;
        self.count -= 1;
        if self.wrapped && self.tail == self.wrap {
            self.tail = 0;
            self.wrapped = false;
        }
        if !self.wrapped && self.tail == self.head {
            self.tail = 0;
            self.head = 0;
        }
    }

    /// The next message, valid until the next call (which consumes it).
    pub(crate) fn receive(&mut self) -> Option<&[u8]> {
        if self.pending > 0 {
            let n = self.pending;
            self.pending = 0;
            self.pop(n);
        }
        if self.count == 0 {
            return None;
        }
        if self.wrapped && self.tail == self.wrap {
            self.tail = 0;
            self.wrapped = false;
        }
        let at = self.tail;
        let mut len = [0u8; 4];
        len.copy_from_slice(&self.buf[at..at + 4]);
        let len = u32::from_le_bytes(len) as usize;
        self.pending = len + 4;
        Some(&self.buf[at + 4..at + 4 + len])
    }
}

struct Reassembly {
    active: bool,
    seq: u16,
    count: u16,
    received: u16,
    total: usize,
    bits: Box<[u64]>,
    buf: Box<[u8]>,
    age: u64,
}

pub(crate) struct UnreliableRecv {
    pub(crate) delivery: DeliveryRing,
    sequenced: bool,
    max_message: usize,
    newest: Option<u16>,
    slots: [Reassembly; REASSEMBLY_SLOTS],
    clock: u64,
    /// Messages dropped: delivery buffer full, too old, malformed fragments.
    pub(crate) dropped: u64,
}

/// `a` is newer than `b` in a wrapping 16-bit sequence.
pub(crate) fn seq_newer(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}

impl UnreliableRecv {
    pub(crate) fn new(config: &ChannelConfig) -> Self {
        let max_frags = fragment_count(config.max_message_size.max(1));
        let slot = || Reassembly {
            active: false,
            seq: 0,
            count: 0,
            received: 0,
            total: 0,
            bits: vec![0u64; max_frags.div_ceil(64)].into_boxed_slice(),
            buf: vec![
                0u8;
                if config.max_message_size > FRAGMENT_BYTES {
                    config.max_message_size
                } else {
                    0
                }
            ]
            .into_boxed_slice(),
            age: 0,
        };
        Self {
            delivery: DeliveryRing::new(config.buffer_bytes),
            sequenced: config.kind == ChannelKind::UnreliableSequenced,
            max_message: config.max_message_size,
            newest: None,
            slots: [slot(), slot()],
            clock: 0,
            dropped: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.delivery.reset();
        self.newest = None;
        for s in &mut self.slots {
            s.active = false;
        }
        self.dropped = 0;
    }

    pub(crate) fn is_sequenced(&self) -> bool {
        self.sequenced
    }

    fn accept_seq(&mut self, seq: u16) -> bool {
        if !self.sequenced {
            return true;
        }
        match self.newest {
            Some(n) if !seq_newer(seq, n) => false,
            _ => {
                self.newest = Some(seq);
                true
            }
        }
    }

    pub(crate) fn whole(&mut self, seq: u16, data: &[u8]) {
        if data.len() > self.max_message || !self.accept_seq(seq) || !self.delivery.push(data) {
            self.dropped += 1;
        }
    }

    /// Checks a fragment's fields without storing it.
    pub(crate) fn fragment_valid(&self, index: usize, count: usize, len: usize) -> bool {
        if count < 2 || index >= count || count > fragment_count(self.max_message) {
            return false;
        }
        if index + 1 < count {
            len == FRAGMENT_BYTES
        } else {
            (1..=FRAGMENT_BYTES).contains(&len)
                && (count - 1) * FRAGMENT_BYTES + len <= self.max_message
        }
    }

    pub(crate) fn fragment(&mut self, seq: u16, index: usize, count: usize, data: &[u8]) {
        self.clock += 1;
        if self.sequenced && self.newest.is_some_and(|n| !seq_newer(seq, n)) {
            return;
        }
        let slot = match self.slots.iter().position(|s| s.active && s.seq == seq) {
            Some(i) => i,
            None => {
                let i = self
                    .slots
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| if s.active { s.age } else { 0 })
                    .map_or(0, |(i, _)| i);
                let s = &mut self.slots[i];
                if s.active {
                    self.dropped += 1;
                }
                s.active = true;
                s.seq = seq;
                s.count = count as u16;
                s.received = 0;
                s.total = 0;
                s.bits.fill(0);
                i
            }
        };
        let s = &mut self.slots[slot];
        s.age = self.clock;
        if usize::from(s.count) != count {
            return;
        }
        let (word, bit) = (index / 64, index % 64);
        if s.bits[word] & (1 << bit) != 0 {
            return;
        }
        s.bits[word] |= 1 << bit;
        s.received += 1;
        let start = index * FRAGMENT_BYTES;
        s.buf[start..start + data.len()].copy_from_slice(data);
        if index + 1 == count {
            s.total = start + data.len();
        }
        if s.received == s.count {
            s.active = false;
            let total = s.total;
            if !self.accept_seq(seq) {
                self.dropped += 1;
                return;
            }
            let s = &self.slots[slot];
            match self.delivery.push_with(total) {
                Some(out) => out.copy_from_slice(&s.buf[..total]),
                None => self.dropped += 1,
            }
        }
    }
}

/// Entries of one sent packet.
pub(crate) struct EntryList {
    pub(crate) items: [Entry; ENTRIES_PER_PACKET],
    pub(crate) len: usize,
}

/// Reliable chunks and notified messages one packet may carry.
pub(crate) const ENTRIES_PER_PACKET: usize = 32;

impl EntryList {
    pub(crate) const fn new() -> Self {
        Self {
            items: [Entry { channel: 0, id: 0 }; ENTRIES_PER_PACKET],
            len: 0,
        }
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len == ENTRIES_PER_PACKET
    }

    pub(crate) fn push(&mut self, e: Entry) {
        if self.len < ENTRIES_PER_PACKET {
            self.items[self.len] = e;
            self.len += 1;
        }
    }

    pub(crate) fn as_slice(&self) -> &[Entry] {
        &self.items[..self.len]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn delivery_ring_keeps_messages_contiguous_and_in_order() {
        let mut ring = DeliveryRing::new(64);
        let mut next = 0u8;
        let mut expect = 0u8;
        for round in 0..200 {
            let len = 1 + (round * 7) % 20;
            while ring.push(&vec![next; len]) {
                next = next.wrapping_add(1);
            }
            for _ in 0..(round % 3 + 1) {
                if let Some(m) = ring.receive() {
                    assert!(m.iter().all(|&b| b == expect));
                    expect = expect.wrapping_add(1);
                }
            }
        }
    }

    #[test]
    fn reliable_stream_reassembles_out_of_order_chunks() {
        let config = ChannelConfig {
            buffer_bytes: 4096,
            max_message_size: 4000,
            ..ChannelConfig::reliable()
        };
        let mut send = ReliableSend::new(&config);
        let mut recv = ReliableRecv::new(&config);
        let messages: Vec<Vec<u8>> = (0..5).map(|i| vec![i as u8; 300 + i * 100]).collect();
        for m in &messages {
            send.queue(m).unwrap();
        }
        // Small packets: many chunks.
        let mut packets = Vec::new();
        loop {
            let mut buf = [0u8; 200];
            let mut w = Writer::new(&mut buf);
            let mut entries = EntryList::new();
            send.write(0, &mut w, &mut entries);
            if w.pos() == 0 {
                break;
            }
            let n = w.pos();
            packets.push(buf[..n].to_vec());
            for e in entries.as_slice() {
                send.on_acked(e.id);
            }
        }
        packets.reverse();
        for p in &packets {
            let mut r = Reader::new(p);
            while !r.is_empty() {
                assert_eq!(r.u8(), Some(0));
                let off = r.u32().unwrap();
                let len = r.varint().unwrap() as usize;
                let data = r.bytes(len).unwrap();
                assert!(recv.fits(off, len));
                recv.accept(off, data);
            }
        }
        for m in &messages {
            assert_eq!(recv.receive().unwrap(), &m[..]);
        }
        assert!(recv.receive().is_none());
        assert_eq!(send.unacked(), 0);
    }
}
