//! The client: decodes snapshots into the world, predicts its own entities with the
//! game's systems and corrects them by rollback, interpolates everything else.
//!
//! One call per rendered frame, [`Client::frame`]:
//!
//! 1. receive: snapshots are decoded against the baselines they name (a ring of decoded
//!    snapshots) and applied: new entities spawn, removed ones despawn, remote entities'
//!    states enter their interpolation buffers;
//! 2. reconcile: the server's state of each predicted entity is compared with the
//!    prediction stored for that tick; on a difference the predicted entities are reset
//!    to the server's state and the game's systems re-run up to the present with the
//!    stored inputs (no difference: nothing runs, ADR 0019);
//! 3. predict: the ticks that are due run the game's systems with this frame's input; the
//!    client keeps ahead of the server by its measured input lead (time dilation of a few
//!    percent rather than jumps);
//! 4. inputs go out with every unacknowledged one repeated;
//! 5. remote entities are written interpolated at "server time − delay", the delay
//!    adapting to the measured jitter.

use crate::bits::{BitReader, BitWriter};
use crate::schema::{NetSchema, WordKind};
use crate::wire::{self, CH_RELIABLE, CH_SNAPSHOT, CH_UNRELIABLE, CLIENT_FRAMES, Inbox};
use crate::{InputData, InputHeader, NetId, NetInput, Predicted, Replicated};
use engine_ecs::{Component, ComponentId, Entity, World, bytemuck};
use engine_net_transport::{ConnectionId, DisconnectReason, Event, Transport};
use std::collections::VecDeque;
use std::time::Instant;

/// Client settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClientSettings {
    /// Ticks per second until the server says otherwise.
    pub tick_rate: f64,
    /// Smallest interpolation delay, in ticks (the delay adapts above it).
    pub min_interp_delay: f64,
    /// Most ticks predicted in one frame (a longer hitch slows the client down).
    pub max_ticks_per_frame: u32,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            tick_rate: 64.0,
            min_interp_delay: 1.0,
            max_ticks_per_frame: 8,
        }
    }
}

/// Something the game reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientEvent {
    /// Joined: the server accepted and the schemas match.
    Connected,
    /// The connection ended (or never started).
    Disconnected(DisconnectReason),
    /// The server runs another schema (another build of the game): disconnected.
    SchemaMismatch,
    /// A networked entity appeared (attach what is not replicated: meshes, sounds).
    Spawned {
        /// The local entity.
        entity: Entity,
        /// Its network id.
        id: NetId,
        /// Whether this client predicts it (its own pawn).
        predicted: bool,
    },
    /// A networked entity is gone (or left this client's interest); the local entity is
    /// despawned at the start of the next frame.
    Despawned {
        /// The local entity.
        entity: Entity,
        /// Its network id.
        id: NetId,
    },
}

/// What one [`Client::frame`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameInfo {
    /// Ticks predicted with this frame's input.
    pub ticks: u32,
    /// Ticks re-simulated after a misprediction.
    pub resimulated: u32,
    /// The next tick to predict.
    pub tick: u32,
    /// The time remote entities were shown at, in server ticks.
    pub interp_time: f64,
    /// Snapshots received.
    pub snapshots: u32,
}

/// Counters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClientStats {
    /// Snapshots received and decoded.
    pub snapshots: u64,
    /// Snapshot bytes received.
    pub snapshot_bytes: u64,
    /// Snapshots that could not be decoded (a missing baseline or malformed): should be 0.
    pub decode_errors: u64,
    /// Snapshots too old to use.
    pub stale_snapshots: u64,
    /// Server states of predicted entities that differed from the prediction.
    pub mispredictions: u64,
    /// Ticks re-simulated.
    pub resimulated_ticks: u64,
    /// Predictions confirmed by the server unchanged.
    pub confirmed: u64,
    /// Hard clock corrections (lead too far off).
    pub clock_resyncs: u64,
    /// Smoothed input lead the server reports, ticks.
    pub input_lead: f32,
    /// Current interpolation delay, ticks.
    pub interp_delay: f64,
    /// Frames that showed an entity extrapolated past its newest state.
    pub extrapolated: u64,
    /// Frames that held an entity at its newest state (extrapolated as far as allowed).
    pub interp_starved: u64,
}

const PRED_RING: u32 = 128;
/// States buffered per interpolated entity: covers delays up to about six ticks.
const INTERP: usize = 8;
/// Ticks an entity is extrapolated past its newest state before it holds there.
const MAX_EXTRAPOLATION: f64 = 2.0;

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    slot: u32,
    generation: u16,
    owned: bool,
    removal: bool,
    mask: u64,
    offset: u32,
}

#[derive(Debug, Default)]
struct Frame {
    tick: u32,
    valid: bool,
    /// Sorted by slot.
    entries: Vec<Entry>,
    words: Vec<u32>,
}

impl Frame {
    fn find(&self, slot: u32) -> Option<&Entry> {
        self.entries
            .binary_search_by_key(&slot, |e| e.slot)
            .ok()
            .map(|i| &self.entries[i])
    }
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    entity: Entity,
    generation: u16,
    owned: bool,
    live: bool,
    mask: u64,
    last_tick: u32,
    interp_ticks: [u32; INTERP],
    interp_len: u8,
    applied_tick: u32,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            entity: Entity::NULL,
            generation: 0,
            owned: false,
            live: false,
            mask: 0,
            last_tick: 0,
            interp_ticks: [0; INTERP],
            interp_len: 0,
            applied_tick: 0,
        }
    }
}

#[derive(Debug, Default)]
struct PredFrame<I> {
    tick: u32,
    valid: bool,
    input: I,
    header: InputHeader,
    /// (slot, mask, offset into `words`).
    entries: Vec<(u32, u64, u32)>,
    words: Vec<u32>,
}

/// The client side of a networked game, over any [`Transport`].
pub struct Client<I: InputData, T: Transport> {
    transport: T,
    conn: Option<ConnectionId>,
    welcomed: bool,
    index: u16,
    schema: NetSchema,
    schema_hash: u64,
    settings: ClientSettings,
    comp_ids: Vec<ComponentId>,
    dt: f64,
    // Clocks.
    last_now: Option<Instant>,
    t0: Option<Instant>,
    synced: bool,
    tick: u32,
    accum: f64,
    scale: f64,
    lead: f32,
    lead_dev: f32,
    has_lead: bool,
    lead_cooldown: u32,
    input_ack: u32,
    offset: f64,
    jitter: f64,
    interp_time: f64,
    newest_snapshot: u32,
    // State.
    frames: Vec<Frame>,
    /// The largest decoded snapshot: (entries, words).
    frame_capacity: (usize, usize),
    slots: Vec<Slot>,
    interp_words: Vec<u32>,
    max_words: usize,
    pred: Vec<PredFrame<I>>,
    predicted: Vec<u32>,
    checks: Vec<(u32, u32)>,
    events: VecDeque<ClientEvent>,
    inbox: Inbox,
    despawn: Vec<Entity>,
    corrections: Vec<(Entity, u64, u32, u32)>,
    correction_words: Vec<u32>,
    // Scratch.
    bytes: Vec<u8>,
    message: Vec<u8>,
    writer: BitWriter,
    stats: ClientStats,
    info: FrameInfo,
}

impl<I: InputData, T: Transport> core::fmt::Debug for Client<I, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Client")
            .field("tick", &self.tick)
            .field("synced", &self.synced)
            .finish_non_exhaustive()
    }
}

impl<I: InputData, T: Transport> Client<I, T> {
    /// A client for `world` over `transport` (a client endpoint configured with
    /// [`NetSchema::endpoint_config`]'s channels, connecting with its token).
    pub fn new(
        transport: T,
        schema: NetSchema,
        settings: ClientSettings,
        world: &mut World,
    ) -> Self {
        let comp_ids = schema
            .components
            .iter()
            .map(|c| (c.register)(world))
            .collect();
        world.register::<Replicated>();
        world.register::<Predicted>();
        world.register::<NetInput<I>>();
        let max_words = schema.max_words();
        let max_size = schema.components.iter().map(|c| c.size).max().unwrap_or(0);
        Self {
            transport,
            conn: None,
            welcomed: false,
            index: 0,
            schema_hash: schema.hash(),
            settings,
            comp_ids,
            dt: 1.0 / settings.tick_rate,
            last_now: None,
            t0: None,
            synced: false,
            tick: 0,
            accum: 0.0,
            scale: 1.0,
            lead: 0.0,
            lead_dev: 0.0,
            has_lead: false,
            lead_cooldown: 0,
            input_ack: 0,
            offset: 0.0,
            jitter: 0.0,
            interp_time: 0.0,
            newest_snapshot: 0,
            frames: (0..CLIENT_FRAMES).map(|_| Frame::default()).collect(),
            frame_capacity: (0, 0),
            slots: Vec::new(),
            interp_words: Vec::new(),
            max_words,
            pred: (0..PRED_RING).map(|_| PredFrame::default()).collect(),
            predicted: Vec::new(),
            checks: Vec::new(),
            events: VecDeque::new(),
            inbox: Inbox::default(),
            despawn: Vec::new(),
            corrections: Vec::new(),
            correction_words: Vec::new(),
            bytes: vec![0; max_size],
            message: Vec::with_capacity(2048),
            writer: BitWriter::with_limit(2048),
            stats: ClientStats::default(),
            info: FrameInfo::default(),
            schema,
        }
    }

    /// The next connection or entity event.
    pub fn poll_event(&mut self) -> Option<ClientEvent> {
        self.events.pop_front()
    }

    /// Whether the clocks are synchronized (the first snapshot arrived).
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Whether the server accepted and the schemas match.
    pub fn is_connected(&self) -> bool {
        self.welcomed && self.conn.is_some()
    }

    /// This client's id on the server (once connected).
    pub fn client_id(&self) -> Option<crate::ClientId> {
        self.is_connected().then_some(crate::ClientId(self.index))
    }

    /// The next tick to predict.
    pub fn tick(&self) -> u32 {
        self.tick
    }

    /// Seconds per tick.
    pub fn dt(&self) -> f32 {
        self.dt as f32
    }

    /// The server time remote entities are shown at, in ticks.
    pub fn interp_time(&self) -> f64 {
        self.interp_time
    }

    /// Counters.
    pub fn stats(&self) -> ClientStats {
        self.stats
    }

    /// The transport.
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// The transport, mutably.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// The fraction of the current tick that has elapsed (for [`crate::SubTick`] of an
    /// action sampled now: it belongs to the next predicted tick).
    pub fn subtick(&self) -> crate::SubTick {
        crate::SubTick::from_fraction((self.accum / self.dt) as f32)
    }

    /// Sends a one-shot event to the server (non-gameplay: chat, readiness; gameplay
    /// actions belong in the input).
    pub fn send_event<E: bytemuck::Pod>(&mut self, reliable: bool, event: &E) -> bool {
        self.send_event_bytes(
            reliable,
            core::any::TypeId::of::<E>(),
            bytemuck::bytes_of(event),
        )
    }

    /// [`Self::send_event`] for an event given by its type and bytes (a session that knows
    /// only its input type, as the player's: ADR 0106). False if the type is not
    /// registered, the bytes are not its size, or there is no connection.
    pub fn send_event_bytes(
        &mut self,
        reliable: bool,
        event_type: core::any::TypeId,
        bytes: &[u8],
    ) -> bool {
        let (Some(kind), Some(conn)) = (self.schema.event_index(event_type), self.conn) else {
            return false;
        };
        if self.schema.events[kind].size != bytes.len() {
            return false;
        }
        wire::event_message(&mut self.message, kind as u16, self.tick, bytes);
        let ch = if reliable { CH_RELIABLE } else { CH_UNRELIABLE };
        self.transport.send(conn, ch, &self.message).is_ok()
    }

    /// Calls `f(type, tick, bytes)` for every event received in the last frame, whatever
    /// its type.
    pub fn read_event_bytes(&self, mut f: impl FnMut(core::any::TypeId, u32, &[u8])) {
        for &(kind, _, tick, start, len) in &self.inbox.records {
            if let Some(event) = self.schema.events.get(usize::from(kind)) {
                let bytes = &self.inbox.bytes[start as usize..(start + len) as usize];
                f(event.type_id, tick, bytes);
            }
        }
    }

    /// How far the prediction clock is into its next tick, 0..1: where to show the
    /// predicted entities between their last two ticks.
    pub fn alpha(&self) -> f32 {
        ((self.accum / self.dt) as f32).clamp(0.0, 1.0)
    }

    /// Calls `f(tick, event)` for every event of type `E` received in the last frame.
    pub fn read_events<E: bytemuck::Pod>(&self, mut f: impl FnMut(u32, E)) {
        if let Some(kind) = self.schema.event_index(core::any::TypeId::of::<E>()) {
            self.inbox.read::<E>(kind as u16, |_, tick, e| f(tick, e));
        }
    }

    /// For a predicted entity corrected in the last frame: its component `C` as predicted
    /// before the correction and after it, at the newest predicted tick. The difference
    /// is the error to smooth out visually (fields that are not replicated read as zero).
    pub fn correction<C: Component + bytemuck::Pod>(&self, entity: Entity) -> Option<(C, C)> {
        let si = self.schema.component_index(core::any::TypeId::of::<C>())?;
        let &(_, mask, before, after) = self.corrections.iter().find(|c| c.0 == entity)?;
        if mask & (1 << si) == 0 {
            return None;
        }
        let o = words_before(&self.schema, mask, si);
        let c = &self.schema.components[si];
        let mut a = C::zeroed();
        let mut b = C::zeroed();
        c.dequantize(
            &self.correction_words[before as usize + o..before as usize + o + c.words],
            bytemuck::bytes_of_mut(&mut a),
        );
        c.dequantize(
            &self.correction_words[after as usize + o..after as usize + o + c.words],
            bytemuck::bytes_of_mut(&mut b),
        );
        Some((a, b))
    }

    /// One rendered frame at `now`, with the input sampled for it: receive and apply
    /// snapshots, correct mispredictions (re-running `simulate`), predict the due ticks
    /// (running `simulate(world, dt)` with `input` in the predicted entities'
    /// [`NetInput`]), send inputs, interpolate remote entities.
    ///
    /// `simulate` must be the same systems the server runs for these entities, reading
    /// their state only from replicated components (and resetting anything derived from
    /// them, such as a physics character's position, from those components).
    pub fn frame(
        &mut self,
        world: &mut World,
        now: Instant,
        input: I,
        mut simulate: impl FnMut(&mut World, f32),
    ) -> FrameInfo {
        engine_profiler::zone!("net::client::frame");
        self.info = FrameInfo::default();
        for e in self.despawn.drain(..) {
            world.despawn(e);
        }
        self.inbox.clear();
        self.corrections.clear();
        self.correction_words.clear();
        let elapsed = self
            .last_now
            .map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        self.last_now = Some(now);
        let t0 = *self.t0.get_or_insert(now);
        let local_ticks = now.saturating_duration_since(t0).as_secs_f64() / self.dt;

        self.receive(world, now, local_ticks);
        self.reconcile(world, &mut simulate);

        if self.synced {
            // Prediction clock: real time, dilated to hold the input lead.
            self.accum += elapsed * self.scale;
            let mut due = 0;
            while self.accum >= self.dt && due < self.settings.max_ticks_per_frame {
                self.accum -= self.dt;
                due += 1;
            }
            if due == self.settings.max_ticks_per_frame {
                self.accum = self.accum.min(self.dt);
            }
            // Interpolation clock: follows "server time − delay" smoothly.
            self.advance_interp(elapsed, local_ticks);
            for _ in 0..due {
                self.predict_tick(world, input, &mut simulate);
            }
            if due > 0 {
                self.send_inputs();
            }
            self.interpolate(world);
        }
        self.transport.flush(now);
        self.info.tick = self.tick;
        self.info.interp_time = self.interp_time;
        self.info
    }

    fn receive(&mut self, world: &mut World, now: Instant, local_ticks: f64) {
        self.transport.update(now);
        while let Some(event) = self.transport.poll_event() {
            match event {
                Event::Connected(conn) => self.conn = Some(conn),
                Event::Disconnected(_, reason) => {
                    self.conn = None;
                    self.welcomed = false;
                    self.events.push_back(ClientEvent::Disconnected(reason));
                }
                _ => {}
            }
        }
        let Some(conn) = self.conn else { return };
        while let Some(msg) = self.transport.receive(conn, CH_RELIABLE) {
            match msg.first() {
                Some(&wire::TAG_WELCOME) if msg.len() >= 19 => {
                    let mut h = [0u8; 8];
                    h.copy_from_slice(&msg[1..9]);
                    if u64::from_le_bytes(h) != self.schema_hash {
                        self.events.push_back(ClientEvent::SchemaMismatch);
                        self.transport.disconnect(now, conn);
                        self.conn = None;
                        return;
                    }
                    self.index = u16::from_le_bytes([msg[9], msg[10]]);
                    let hz = f32::from_le_bytes([msg[15], msg[16], msg[17], msg[18]]);
                    if hz > 0.0 {
                        self.dt = 1.0 / f64::from(hz);
                    }
                    self.welcomed = true;
                    self.events.push_back(ClientEvent::Connected);
                }
                Some(&wire::TAG_EVENT) => {
                    if let Some((kind, tick, payload)) = wire::parse_event(msg) {
                        self.inbox.push(kind, 0, tick, payload);
                    }
                }
                _ => {}
            }
        }
        while let Some(msg) = self.transport.receive(conn, CH_UNRELIABLE) {
            if let Some((kind, tick, payload)) = wire::parse_event(msg) {
                self.inbox.push(kind, 0, tick, payload);
            }
        }
        // Snapshots: copied out of the transport, decoded, applied.
        loop {
            let Some(msg) = self.transport.receive(conn, CH_SNAPSHOT) else {
                break;
            };
            self.message.clear();
            self.message.extend_from_slice(msg);
            let msg = core::mem::take(&mut self.message);
            self.snapshot(world, &msg, local_ticks);
            self.message = msg;
        }
    }

    fn snapshot(&mut self, world: &mut World, msg: &[u8], local_ticks: f64) {
        if msg.len() < 5 || msg[0] != wire::TAG_SNAPSHOT {
            self.stats.decode_errors += 1;
            return;
        }
        let tick = u32::from_le_bytes([msg[1], msg[2], msg[3], msg[4]]);
        if self.newest_snapshot >= CLIENT_FRAMES && tick <= self.newest_snapshot - CLIENT_FRAMES / 2
        {
            self.stats.stale_snapshots += 1;
            return;
        }
        let mut r = BitReader::new(&msg[5..]);
        let Some(lead) = decode_header(&mut r) else {
            self.stats.decode_errors += 1;
            return;
        };
        let Some(default_base) = decode_default(&mut r, tick) else {
            self.stats.decode_errors += 1;
            return;
        };
        if self.decode(&mut r, tick, default_base).is_none() {
            self.frames[(tick % CLIENT_FRAMES) as usize].valid = false;
            self.stats.decode_errors += 1;
            return;
        }
        self.stats.snapshots += 1;
        self.stats.snapshot_bytes += msg.len() as u64;
        self.info.snapshots += 1;
        self.observe_clock(tick, lead, local_ticks);
        self.apply(world, tick);
    }

    fn decode(
        &mut self,
        r: &mut BitReader<'_>,
        tick: u32,
        default_base: Option<u32>,
    ) -> Option<()> {
        let index = (tick % CLIENT_FRAMES) as usize;
        // Take the frame out so baselines (other frames) can be read while it is written.
        let mut frame = core::mem::take(&mut self.frames[index]);
        frame.tick = tick;
        frame.valid = false;
        frame.entries.clear();
        frame.words.clear();
        let result = self.decode_into(r, tick, default_base, &mut frame);
        frame.valid = result.is_some();
        // A new largest snapshot: grow every frame of the ring to it at once, so the
        // ring does not keep allocating as each of its frames meets the peak in turn.
        let (entries, words) = (frame.entries.capacity(), frame.words.capacity());
        self.frames[index] = frame;
        if entries > self.frame_capacity.0 || words > self.frame_capacity.1 {
            self.frame_capacity = (
                entries.max(self.frame_capacity.0),
                words.max(self.frame_capacity.1),
            );
            let (e, w) = self.frame_capacity;
            for f in &mut self.frames {
                f.entries.reserve(e.saturating_sub(f.entries.len()));
                f.words.reserve(w.saturating_sub(f.words.len()));
            }
        }
        result
    }

    fn decode_into(
        &self,
        r: &mut BitReader<'_>,
        tick: u32,
        default_base: Option<u32>,
        frame: &mut Frame,
    ) -> Option<()> {
        let n = self.schema.components.len() as u32;
        let mut prev: Option<u32> = None;
        while r.bit()? {
            let gap = r.golomb(wire::K_GAP)?;
            let slot = match prev {
                Some(p) => p.checked_add(gap)?.checked_add(1)?,
                None => gap,
            };
            prev = Some(slot);
            let kind = r.read(2)?;
            let offset = frame.words.len() as u32;
            match kind {
                wire::KIND_REMOVE => {
                    let generation = r.read(16)? as u16;
                    frame.entries.push(Entry {
                        slot,
                        generation,
                        removal: true,
                        ..Entry::default()
                    });
                }
                wire::KIND_FULL => {
                    let generation = r.read(16)? as u16;
                    let owned = r.bit()?;
                    let mut mask = u64::from(r.read(n.min(32))?);
                    if n > 32 {
                        mask |= u64::from(r.read(n - 32)?) << 32;
                    }
                    let mut m = mask;
                    while m != 0 {
                        let i = m.trailing_zeros() as usize;
                        m &= m - 1;
                        for &kind in &self.schema.components[i].kinds {
                            frame.words.push(match kind {
                                WordKind::Int => r.signed(wire::K_FULL)? as u32,
                                WordKind::Raw => r.read(32)?,
                                WordKind::Bit => r.read(1)?,
                            });
                        }
                    }
                    frame.entries.push(Entry {
                        slot,
                        generation,
                        owned,
                        removal: false,
                        mask,
                        offset,
                    });
                }
                wire::KIND_DELTA => {
                    let base_tick = if r.bit()? {
                        default_base?
                    } else {
                        tick.checked_sub(r.golomb(2)?)?
                    };
                    let base = &self.frames[(base_tick % CLIENT_FRAMES) as usize];
                    if !base.valid || base.tick != base_tick {
                        return None;
                    }
                    let be = *base.find(slot)?;
                    if be.removal {
                        return None;
                    }
                    // The older state for predicted baselines.
                    let older = if r.bit()? {
                        let span = r.golomb(0)?.checked_add(1)?;
                        let older_tick = base_tick.checked_sub(span)?;
                        let of = &self.frames[(older_tick % CLIENT_FRAMES) as usize];
                        if !of.valid || of.tick != older_tick {
                            return None;
                        }
                        let oe = *of.find(slot)?;
                        if oe.removal || oe.mask != be.mask {
                            return None;
                        }
                        Some((of, oe.offset as usize, span))
                    } else {
                        None
                    };
                    let mut m = be.mask;
                    let mut bo = be.offset as usize;
                    let mut oo = older.map_or(0, |o| o.1);
                    while m != 0 {
                        let i = m.trailing_zeros() as usize;
                        m &= m - 1;
                        let c = &self.schema.components[i];
                        let bw = &base.words[bo..bo + c.words];
                        bo += c.words;
                        let pw = older.map(|(of, _, span)| {
                            (
                                &of.words[oo..oo + c.words],
                                crate::server::Older {
                                    words: &[],
                                    ahead: i64::from(tick - base_tick),
                                    span: i64::from(span),
                                },
                            )
                        });
                        oo += c.words;
                        if !r.bit()? {
                            frame.words.extend_from_slice(bw);
                            continue;
                        }
                        let predicted = pw.is_some() && c.any_linear && r.bit()?;
                        for (j, (&b, &kind)) in bw.iter().zip(&c.kinds).enumerate() {
                            let reference = match pw {
                                Some((pw, ref old)) if predicted && c.linear[j] => {
                                    crate::server::extrapolate(b, pw[j], old)
                                }
                                _ => b,
                            };
                            let v = if r.bit()? {
                                match kind {
                                    WordKind::Int => {
                                        reference.wrapping_add(r.signed(wire::K_DELTA)? as u32)
                                    }
                                    WordKind::Raw => r.read(32)?,
                                    WordKind::Bit => (reference ^ 1) & 1,
                                }
                            } else {
                                reference
                            };
                            frame.words.push(v);
                        }
                    }
                    frame.entries.push(Entry {
                        slot,
                        generation: be.generation,
                        owned: be.owned,
                        removal: false,
                        mask: be.mask,
                        offset,
                    });
                }
                _ => return None,
            }
        }
        Some(())
    }

    fn observe_clock(&mut self, tick: u32, lead: Option<i32>, local_ticks: f64) {
        let rtt_ticks = self
            .conn
            .and_then(|c| self.transport.stats(c))
            .map_or(0.0, |s| s.rtt.as_secs_f64() / self.dt);
        let o = f64::from(tick) - local_ticks;
        if !self.synced {
            self.synced = true;
            self.offset = o;
            self.newest_snapshot = tick;
            let delay = self.settings.min_interp_delay;
            self.interp_time = f64::from(tick) - delay;
            self.tick = tick + rtt_ticks.ceil() as u32 + 2;
            self.accum = 0.0;
            self.stats.interp_delay = delay;
        } else {
            // Upper envelope of (server tick − local tick): the least delayed snapshots
            // tell the server's clock best; how far the others fall behind is jitter.
            if o > self.offset {
                self.offset += (o - self.offset) * 0.5;
            } else {
                self.offset += (o - self.offset) * 0.01;
            }
            self.jitter = self.jitter * 0.95 + (self.offset - o).max(0.0) * 0.05;
            self.newest_snapshot = self.newest_snapshot.max(tick);
        }
        if let Some(lead) = lead {
            self.input_ack = self
                .input_ack
                .max((i64::from(tick) + i64::from(lead)).max(0) as u32);
            let lead = lead as f32;
            if self.lead_cooldown > 0 {
                self.lead_cooldown -= 1;
                return;
            }
            if self.has_lead {
                self.lead_dev = self.lead_dev * 0.9 + (lead - self.lead).abs() * 0.1;
                self.lead = self.lead * 0.9 + lead * 0.1;
            } else {
                self.lead = lead;
                self.has_lead = true;
            }
            self.stats.input_lead = self.lead;
            let loss = self
                .conn
                .and_then(|c| self.transport.stats(c))
                .map_or(0.0, |s| s.packet_loss);
            let target = 1.0 + 2.0 * self.lead_dev + if loss > 0.02 { 1.0 } else { 0.0 };
            let error = target - self.lead;
            if !(-12.0..=2.5).contains(&error) {
                // Inputs arriving too late (just joined, a hitch, a route change), or far
                // too early: jump, then wait for samples that reflect the jump.
                self.accum += f64::from(error) * self.dt;
                self.lead = target;
                self.lead_cooldown = rtt_ticks.ceil() as u32 + 4;
                self.stats.clock_resyncs += 1;
                self.scale = 1.0;
            } else {
                self.scale = 1.0 + f64::from((error * 0.02).clamp(-0.05, 0.05));
            }
        }
    }

    fn advance_interp(&mut self, elapsed: f64, local_ticks: f64) {
        let interval = 1.0; // snapshots every tick by default
        // One snapshot interval, twice the measured jitter, and under loss one more
        // interval so a single lost snapshot does not starve the buffer (Source's
        // `cl_interp_ratio 2`).
        let loss = self
            .conn
            .and_then(|c| self.transport.stats(c))
            .map_or(0.0, |s| s.packet_loss);
        let lossy = if loss > 0.01 { interval } else { 0.0 };
        let delay =
            (interval + 2.0 * self.jitter + lossy + 0.25).max(self.settings.min_interp_delay);
        self.stats.interp_delay = delay;
        let target = local_ticks + self.offset - delay;
        let error = target - self.interp_time;
        if error.abs() > 4.0 {
            self.interp_time = target;
        } else {
            self.interp_time += elapsed / self.dt * (1.0 + (error * 0.1).clamp(-0.1, 0.1));
        }
    }

    fn ensure_slot(&mut self, slot: u32) {
        let s = slot as usize;
        if self.slots.len() <= s {
            self.slots.resize(s + 1, Slot::default());
            self.interp_words
                .resize(self.slots.len() * INTERP * self.max_words, 0);
        }
    }

    fn apply(&mut self, world: &mut World, tick: u32) {
        let index = (tick % CLIENT_FRAMES) as usize;
        let frame = core::mem::take(&mut self.frames[index]);
        for e in &frame.entries {
            self.ensure_slot(e.slot);
            let s = self.slots[e.slot as usize];
            if tick <= s.last_tick {
                continue; // older than what this slot already shows
            }
            self.slots[e.slot as usize].last_tick = tick;
            let words = &frame.words[e.offset as usize..];
            if e.removal {
                if s.live && s.generation == e.generation {
                    self.despawn_local(e.slot);
                }
                continue;
            }
            if !s.live || s.generation != e.generation || s.owned != e.owned {
                if s.live {
                    self.despawn_local(e.slot);
                }
                self.spawn_local(world, e, words, tick);
            } else if s.mask != e.mask {
                self.change_components(world, e, words);
                self.reset_interp(e.slot, tick, words);
            } else if !e.owned {
                self.push_interp(e.slot, tick, words);
            }
            if e.owned {
                self.checks.push((e.slot, tick));
            }
        }
        self.frames[index] = frame;
    }

    fn spawn_local(&mut self, world: &mut World, e: &Entry, words: &[u32], tick: u32) {
        let rep = Replicated {
            id: e.slot + 1,
            generation: e.generation,
            owner: if e.owned { self.index + 1 } else { 0 },
        };
        let entity = world.spawn((rep,));
        let mut m = e.mask;
        let mut o = 0;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let c = &self.schema.components[i];
            let bytes = &mut self.bytes[..c.size];
            bytes.fill(0);
            c.dequantize(&words[o..o + c.words], bytes);
            o += c.words;
            // SAFETY: `bytes` holds a valid value of the component (plain data: any bytes
            // are valid, `NetSchema::replicate` requires `Pod`), of its registered size;
            // `insert_raw` copies it and the buffer is not used as that value again.
            unsafe { world.insert_raw(entity, self.comp_ids[i], bytes.as_ptr()) };
        }
        if e.owned {
            world.insert(entity, Predicted);
            world.insert(entity, NetInput::<I>::default());
            if !self.predicted.contains(&e.slot) {
                self.predicted.push(e.slot);
            }
        }
        let s = &mut self.slots[e.slot as usize];
        s.entity = entity;
        s.generation = e.generation;
        s.owned = e.owned;
        s.live = true;
        s.mask = e.mask;
        s.interp_len = 0;
        s.applied_tick = 0;
        if !e.owned {
            self.reset_interp(e.slot, tick, words);
        }
        self.events.push_back(ClientEvent::Spawned {
            entity,
            id: NetId {
                slot: e.slot,
                generation: e.generation,
            },
            predicted: e.owned,
        });
    }

    fn change_components(&mut self, world: &mut World, e: &Entry, words: &[u32]) {
        let s = self.slots[e.slot as usize];
        let mut o = 0;
        for i in 0..self.schema.components.len() {
            let bit = 1u64 << i;
            let c = &self.schema.components[i];
            if e.mask & bit != 0 {
                if s.mask & bit == 0 {
                    let bytes = &mut self.bytes[..c.size];
                    bytes.fill(0);
                    c.dequantize(&words[o..o + c.words], bytes);
                    // SAFETY: as in `spawn_local`.
                    unsafe { world.insert_raw(s.entity, self.comp_ids[i], bytes.as_ptr()) };
                }
                o += c.words;
            } else if s.mask & bit != 0 {
                world.remove_raw(s.entity, self.comp_ids[i]);
            }
        }
        self.slots[e.slot as usize].mask = e.mask;
    }

    fn despawn_local(&mut self, slot: u32) {
        let s = &mut self.slots[slot as usize];
        if !s.live {
            return;
        }
        s.live = false;
        self.despawn.push(s.entity);
        self.events.push_back(ClientEvent::Despawned {
            entity: s.entity,
            id: NetId {
                slot,
                generation: s.generation,
            },
        });
        self.predicted.retain(|&p| p != slot);
    }

    fn reset_interp(&mut self, slot: u32, tick: u32, words: &[u32]) {
        let s = &mut self.slots[slot as usize];
        s.interp_len = 0;
        s.applied_tick = 0;
        self.push_interp(slot, tick, words);
    }

    fn push_interp(&mut self, slot: u32, tick: u32, words: &[u32]) {
        let s = &mut self.slots[slot as usize];
        let len = s.interp_len as usize;
        let i = if len < INTERP {
            s.interp_len += 1;
            len
        } else {
            // Replace the oldest.
            let (i, &oldest) = s
                .interp_ticks
                .iter()
                .enumerate()
                .min_by_key(|(_, t)| **t)
                .unwrap_or((0, &0));
            if tick < oldest {
                return;
            }
            i
        };
        s.interp_ticks[i] = tick;
        let n = words_of(&self.schema, s.mask);
        let base = (slot as usize * INTERP + i) * self.max_words;
        self.interp_words[base..base + n].copy_from_slice(&words[..n]);
    }

    fn interpolate(&mut self, world: &mut World) {
        engine_profiler::zone!("net::client::interpolate");
        let r = self.interp_time;
        for slot in 0..self.slots.len() {
            let s = self.slots[slot];
            if !s.live || s.owned || s.interp_len == 0 {
                continue;
            }
            let ticks = &s.interp_ticks[..s.interp_len as usize];
            let (mut a, mut b): (Option<usize>, Option<usize>) = (None, None);
            for (i, &t) in ticks.iter().enumerate() {
                if f64::from(t) <= r {
                    if a.is_none_or(|j| ticks[j] < t) {
                        a = Some(i);
                    }
                } else if b.is_none_or(|j| ticks[j] > t) {
                    b = Some(i);
                }
            }
            let (from, to, alpha) = match (a, b) {
                (Some(a), Some(b)) => {
                    let (ta, tb) = (f64::from(ticks[a]), f64::from(ticks[b]));
                    (a, b, ((r - ta) / (tb - ta)) as f32)
                }
                (Some(a), None) => {
                    // Past the newest state (late or lost snapshots): extrapolate from
                    // the two newest for up to `MAX_EXTRAPOLATION` ticks, then hold.
                    let ta = f64::from(ticks[a]);
                    let previous = ticks
                        .iter()
                        .enumerate()
                        .filter(|&(_, &t)| t < ticks[a])
                        .max_by_key(|&(_, &t)| t)
                        .map(|(i, _)| i);
                    match previous {
                        Some(p) if r - ta <= MAX_EXTRAPOLATION => {
                            self.stats.extrapolated += 1;
                            let tp = f64::from(ticks[p]);
                            (p, a, ((r - tp) / (ta - tp)) as f32)
                        }
                        _ => {
                            if s.applied_tick == ticks[a] {
                                continue; // already showing the newest state
                            }
                            self.stats.interp_starved += 1;
                            (a, a, 1.0)
                        }
                    }
                }
                (None, Some(b)) => (b, b, 1.0),
                (None, None) => continue,
            };
            let n = words_of(&self.schema, s.mask);
            let fa = (slot * INTERP + from) * self.max_words;
            let fb = (slot * INTERP + to) * self.max_words;
            let mut m = s.mask;
            let mut o = 0;
            while m != 0 {
                let i = m.trailing_zeros() as usize;
                m &= m - 1;
                let c = &self.schema.components[i];
                if let Some(ptr) = world.get_raw_mut(s.entity, self.comp_ids[i]) {
                    // SAFETY: the pointer is to a live, plain-data component of `c.size`
                    // bytes; `&mut World` makes the access exclusive for this call.
                    let bytes = unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), c.size) };
                    c.interpolate(
                        &self.interp_words[fa + o..fa + o + c.words],
                        &self.interp_words[fb + o..fb + o + c.words],
                        alpha,
                        bytes,
                    );
                }
                o += c.words;
            }
            debug_assert!(o <= n);
            if alpha >= 1.0 && from == to {
                self.slots[slot].applied_tick = ticks[from];
            } else {
                self.slots[slot].applied_tick = 0;
            }
        }
    }

    /// Quantizes the predicted entities into the prediction ring at `tick` and rounds
    /// their components to the network precision (as the server does with its own).
    fn store_prediction(&mut self, world: &mut World, tick: u32) {
        let p = &mut self.pred[(tick % PRED_RING) as usize];
        p.entries.clear();
        p.words.clear();
        for &slot in &self.predicted {
            let s = self.slots[slot as usize];
            if !s.live {
                continue;
            }
            let offset = p.words.len();
            p.entries.push((slot, s.mask, offset as u32));
            let mut m = s.mask;
            while m != 0 {
                let i = m.trailing_zeros() as usize;
                m &= m - 1;
                let c = &self.schema.components[i];
                let start = p.words.len();
                p.words.resize(start + c.words, 0);
                if let Some(ptr) = world.get_raw_mut(s.entity, self.comp_ids[i]) {
                    // SAFETY: as in `interpolate`.
                    let bytes = unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), c.size) };
                    c.quantize(bytes, &mut p.words[start..]);
                    c.dequantize(&p.words[start..], bytes);
                }
            }
        }
    }

    fn write_inputs(&self, world: &mut World, input: I, header: InputHeader) {
        for &slot in &self.predicted {
            let s = self.slots[slot as usize];
            if let Some(n) = world.get_mut::<NetInput<I>>(s.entity) {
                n.input = input;
                n.header = header;
            }
        }
    }

    fn predict_tick(
        &mut self,
        world: &mut World,
        input: I,
        simulate: &mut impl FnMut(&mut World, f32),
    ) {
        let tick = self.tick;
        let delay = ((f64::from(tick) - self.interp_time) * 256.0).clamp(0.0, 65535.0) as u16;
        let header = InputHeader {
            tick,
            view_delay: delay,
            fresh: true,
        };
        self.write_inputs(world, input, header);
        simulate(world, self.dt as f32);
        let p = &mut self.pred[(tick % PRED_RING) as usize];
        p.tick = tick;
        p.valid = true;
        p.input = input;
        p.header = header;
        self.store_prediction(world, tick);
        self.tick += 1;
        self.info.ticks += 1;
    }

    fn reconcile(&mut self, world: &mut World, simulate: &mut impl FnMut(&mut World, f32)) {
        if self.checks.is_empty() {
            return;
        }
        engine_profiler::zone!("net::client::reconcile");
        // The newest server state per predicted slot.
        self.checks
            .sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        self.checks.dedup_by_key(|c| c.0);
        let mut start: Option<u32> = None;
        for k in 0..self.checks.len() {
            let (slot, tick) = self.checks[k];
            let server = &self.frames[(tick % CLIENT_FRAMES) as usize];
            let Some(se) = server
                .find(slot)
                .copied()
                .filter(|_| server.tick == tick && server.valid)
            else {
                continue;
            };
            let sw = &server.words
                [se.offset as usize..se.offset as usize + words_of(&self.schema, se.mask)];
            if tick >= self.tick {
                // The server is ahead of the prediction (just joined, or the clock is
                // behind): take its state as is.
                load(
                    &self.schema,
                    &self.comp_ids,
                    &self.slots,
                    world,
                    slot,
                    se.mask,
                    sw,
                );
                continue;
            }
            let p = &self.pred[(tick % PRED_RING) as usize];
            let matches = p.valid
                && p.tick == tick
                && p.entries.iter().any(|&(s, mask, o)| {
                    s == slot
                        && mask == se.mask
                        && p.words[o as usize..o as usize + sw.len()] == *sw
                });
            if matches {
                self.stats.confirmed += 1;
            } else {
                self.stats.mispredictions += 1;
                start = Some(start.map_or(tick, |s: u32| s.min(tick)));
            }
        }
        self.checks.clear();
        let Some(start) = start else { return };
        let last = self.tick.saturating_sub(1);
        // The prediction of the newest tick before the correction.
        let before = self.pred_snapshot(last);
        // Back to the server's state (or the stored prediction for entities the snapshot
        // did not carry) at `start`.
        let server = &self.frames[(start % CLIENT_FRAMES) as usize];
        for k in 0..self.predicted.len() {
            let slot = self.predicted[k];
            let from_server = server
                .find(slot)
                .copied()
                .filter(|e| server.valid && server.tick == start && !e.removal);
            if let Some(e) = from_server {
                let n = words_of(&self.schema, e.mask);
                let words = &server.words[e.offset as usize..e.offset as usize + n];
                load(
                    &self.schema,
                    &self.comp_ids,
                    &self.slots,
                    world,
                    slot,
                    e.mask,
                    words,
                );
            } else {
                let p = &self.pred[(start % PRED_RING) as usize];
                if let Some(&(_, mask, o)) = p
                    .entries
                    .iter()
                    .find(|e| e.0 == slot)
                    .filter(|_| p.tick == start)
                {
                    let n = words_of(&self.schema, mask);
                    load(
                        &self.schema,
                        &self.comp_ids,
                        &self.slots,
                        world,
                        slot,
                        mask,
                        &p.words[o as usize..o as usize + n],
                    );
                }
            }
        }
        // Re-run the ticks since, with the inputs they had.
        for t in start + 1..self.tick {
            let p = &self.pred[(t % PRED_RING) as usize];
            if !p.valid || p.tick != t {
                break;
            }
            let (input, header) = (p.input, p.header);
            self.write_inputs(world, input, header);
            simulate(world, self.dt as f32);
            self.store_prediction(world, t);
            self.info.resimulated += 1;
            self.stats.resimulated_ticks += 1;
        }
        if start < last
            && let Some(before) = before
        {
            self.record_corrections(last, before);
        }
    }

    /// Copies the prediction of `tick` into the correction buffer; returns its start.
    fn pred_snapshot(&mut self, tick: u32) -> Option<u32> {
        let p = &self.pred[(tick % PRED_RING) as usize];
        if !p.valid || p.tick != tick {
            return None;
        }
        let start = self.correction_words.len() as u32;
        self.correction_words.extend_from_slice(&p.words);
        Some(start)
    }

    fn record_corrections(&mut self, tick: u32, before: u32) {
        let Some(after) = self.pred_snapshot(tick) else {
            return;
        };
        let p = &self.pred[(tick % PRED_RING) as usize];
        for &(slot, mask, o) in &p.entries {
            let entity = self.slots[slot as usize].entity;
            self.corrections.push((entity, mask, before + o, after + o));
        }
    }

    fn send_inputs(&mut self) {
        let Some(conn) = self.conn else { return };
        let newest = self.tick - 1;
        let mut oldest = newest.saturating_sub(wire::MAX_REDUNDANT_INPUTS - 1);
        oldest = oldest.max(self.input_ack + 1);
        while oldest < newest {
            let p = &self.pred[(oldest % PRED_RING) as usize];
            if p.valid && p.tick == oldest {
                break;
            }
            oldest += 1;
        }
        if oldest > newest {
            oldest = newest;
        }
        let count = newest - oldest + 1;
        let size = core::mem::size_of::<I>();
        let w = &mut self.writer;
        w.reset(1500);
        let mut prev: Option<(u16, I)> = None;
        for t in oldest..=newest {
            let p = &self.pred[(t % PRED_RING) as usize];
            let (view, input) = if p.valid && p.tick == t {
                (p.header.view_delay, p.input)
            } else {
                (0, I::default())
            };
            let same_view = prev.is_some_and(|(v, _)| v == view);
            w.bit(same_view);
            if !same_view {
                w.golomb(u32::from(view), 6);
            }
            let same = prev.is_some_and(|(_, i)| i == input);
            w.bit(same);
            if !same {
                for &b in bytemuck::bytes_of(&input) {
                    w.write(u32::from(b), 8);
                }
            }
            prev = Some((view, input));
        }
        let _ = size;
        let bits = w.finish();
        self.message.clear();
        self.message.push(wire::TAG_INPUT);
        self.message.extend_from_slice(&newest.to_le_bytes());
        self.message.push(count as u8);
        self.message.extend_from_slice(bits);
        let _ = self.transport.send(conn, CH_UNRELIABLE, &self.message);
    }
}

fn load(
    schema: &NetSchema,
    comp_ids: &[ComponentId],
    slots: &[Slot],
    world: &mut World,
    slot: u32,
    mask: u64,
    words: &[u32],
) {
    let s = slots[slot as usize];
    if !s.live {
        return;
    }
    let mut m = mask;
    let mut o = 0;
    while m != 0 {
        let i = m.trailing_zeros() as usize;
        m &= m - 1;
        let c = &schema.components[i];
        if let Some(ptr) = world.get_raw_mut(s.entity, comp_ids[i]) {
            // SAFETY: as in `Client::interpolate`.
            let bytes = unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), c.size) };
            c.dequantize(&words[o..o + c.words], bytes);
        }
        o += c.words;
    }
}

fn decode_header(r: &mut BitReader<'_>) -> Option<Option<i32>> {
    Some(if r.bit()? { Some(r.signed(2)?) } else { None })
}

fn decode_default(r: &mut BitReader<'_>, tick: u32) -> Option<Option<u32>> {
    Some(if r.bit()? {
        Some(tick.checked_sub(r.golomb(2)?)?)
    } else {
        None
    })
}

fn words_of(schema: &NetSchema, mask: u64) -> usize {
    let mut m = mask;
    let mut n = 0;
    while m != 0 {
        let i = m.trailing_zeros() as usize;
        n += schema.components[i].words;
        m &= m - 1;
    }
    n
}

fn words_before(schema: &NetSchema, mask: u64, index: usize) -> usize {
    words_of(schema, mask & ((1u64 << index) - 1))
}
