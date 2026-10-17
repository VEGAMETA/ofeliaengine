//! The authoritative server: the world's networked entities become per-client snapshots.
//!
//! Per tick: [`Server::update`] (receive; write every client's input for this tick into
//! its pawns' [`NetInput`]), the game's systems, then [`Server::send`]:
//!
//! 1. new [`Replicated`] entities get slots, despawned ones free theirs;
//! 2. every networked entity's components are quantized once into the tick's frame (a
//!    ring of [`HISTORY`] frames shared by all clients) and the simulation's own values
//!    are rounded to what clients will see, so client and server simulate from the same
//!    numbers;
//! 3. per client: the entities that changed since their acknowledged state (or whose last
//!    send was lost) accumulate priority; the highest fill the client's byte budget, each
//!    a delta against the newest state of it the client acknowledged;
//! 4. delivery notifications move acknowledged states forward; losses mark entities for
//!    sending again (with whatever is newest then).

use crate::bits::BitWriter;
use crate::lagcomp::{HitCapsule, LagCompensation};
use crate::schema::{NetSchema, WordKind};
use crate::wire::{self, CH_RELIABLE, CH_SNAPSHOT, CH_UNRELIABLE, HISTORY, INPUT_RING, Inbox};
use crate::{InputData, InputHeader, NetInput, NetRelevance, Replicated};
use engine_core::jobs::ThreadPool;
use engine_core::math::Vec3;
use engine_ecs::{ComponentId, Entity, QueryState, Transform, World, bytemuck};
use engine_net_transport::{ConnectionId, DisconnectReason, Event, MessageId, Transport};
use std::collections::VecDeque;
use std::time::Instant;

/// A connected client (its connection slot on the server; reused after it leaves).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId(pub u16);

/// Something the game reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerEvent {
    /// A client joined: spawn its pawn and [`Server::set_owner`] it.
    ClientConnected(ClientId),
    /// A client left; its pawns are no longer owned (despawn them if the game wants).
    ClientDisconnected(ClientId, DisconnectReason),
}

/// Who an event goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Every client.
    All,
    /// One client.
    Client(ClientId),
    /// Every client but one (usually the one whose action caused it and who predicted it).
    AllExcept(ClientId),
}

/// Server settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ServerSettings {
    /// Ticks per second.
    pub tick_rate: f64,
    /// Largest snapshot per client per send, bytes (1,100 fits one packet).
    pub max_snapshot_bytes: usize,
    /// Send a snapshot every this many ticks (1: every tick).
    pub snapshot_interval: u32,
    /// Priority multiplier of a client's own entities (its pawn's state matters most to
    /// its prediction).
    pub owner_priority: f32,
    /// Lag compensation history, seconds (0: none).
    pub max_rewind: f64,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            tick_rate: 64.0,
            max_snapshot_bytes: 1100,
            snapshot_interval: 1,
            owner_priority: 100.0,
            max_rewind: 1.0,
        }
    }
}

/// Counters for one client.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ServerStats {
    /// Snapshots sent.
    pub snapshots: u64,
    /// Snapshot bytes sent (messages, before the transport's headers).
    pub snapshot_bytes: u64,
    /// Bytes of the last snapshot.
    pub last_snapshot_bytes: usize,
    /// Entity records sent (full, delta, removal).
    pub records: u64,
    /// Of which whole states.
    pub full_records: u64,
    /// Records that needed sending but did not fit the budget in the tick they first did.
    pub deferred: u64,
    /// Snapshots delivered and lost.
    pub delivered: u64,
    /// Snapshots reported lost.
    pub lost: u64,
    /// Ticks for which the client's input had not arrived (the previous one was used).
    pub inputs_missing: u64,
    /// Inputs that arrived too late to be used.
    pub inputs_late: u64,
    /// How many ticks ahead of the server the client's newest input was, at the last send.
    pub input_lead: i32,
}

const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
struct Slot {
    entity: Entity,
    generation: u16,
    alive: bool,
    owner: u16,
    changed_at: u32,
    has_relevance: bool,
    relevance: NetRelevance,
    position: Vec3,
}

#[derive(Debug, Default)]
pub(crate) struct Frame {
    pub(crate) tick: u32,
    pub(crate) valid: bool,
    pub(crate) offset: Vec<u32>,
    pub(crate) mask: Vec<u64>,
    pub(crate) generation: Vec<u16>,
    pub(crate) words: Vec<u32>,
}

impl Frame {
    fn state(&self, slot: u32) -> Option<(u64, u16, u32)> {
        let i = slot as usize;
        let o = *self.offset.get(i)?;
        (o != NONE).then(|| (self.mask[i], self.generation[i], o))
    }
}

/// The client's view of one slot, from the server's side.
#[derive(Debug, Clone, Copy, Default)]
struct Ghost {
    /// The client may have this slot (an alive record was sent and no removal confirmed).
    has: bool,
    /// Generation of the alive records sent.
    sent_gen: u16,
    /// Newest tick whose state of this entity the client acknowledged (0: none).
    acked: u32,
    /// The acknowledged state before `acked` (0: none), for predicted baselines.
    acked_prev: u32,
    /// Tick of the last alive record sent (0: none).
    alive_tick: u32,
    /// Tick of the last removal sent (0: none).
    removal_tick: u32,
    lost_alive: bool,
    lost_removal: bool,
    priority: f32,
    /// Bits of the last delta record sent (an estimate for the next one).
    last_bits: u16,
}

#[derive(Debug, Clone, Copy)]
struct Record {
    slot: u32,
    removal: bool,
    generation: u16,
}

#[derive(Debug, Default)]
struct Sent {
    active: bool,
    msg: u32,
    tick: u32,
    records: Vec<Record>,
}

struct ClientState<I> {
    conn: ConnectionId,
    id: ClientId,
    ghosts: Vec<Ghost>,
    sent: Vec<Sent>,
    inputs: Vec<(u32, InputHeader, I)>,
    newest_input: u32,
    last_input: (I, InputHeader),
    pawns: Vec<Entity>,
    acked_snapshot: u32,
    view: Option<Vec3>,
    avg_record_bits: f32,
    /// Records of the snapshot being written (swapped into its `Sent` entry).
    scratch: Vec<Record>,
    // Packing scratch, per client so clients pack in parallel.
    budget: usize,
    candidates: Vec<(f32, u32)>,
    kept: Vec<u32>,
    measure: BitWriter,
    writer: BitWriter,
    out: Vec<u8>,
    stats: ServerStats,
}

#[derive(Debug, Clone)]
struct ArchInfo {
    rep: usize,
    /// (schema index, column, word offset within the entity's state).
    comps: Vec<(usize, usize, usize)>,
    mask: u64,
    words: usize,
    relevance: Option<usize>,
    transform: Option<usize>,
}

/// The authoritative side of a networked game, over any [`Transport`] (an `Endpoint` on a
/// UDP socket, the in-memory network in tests, a relay).
///
/// `I` is the game's input type: plain data, the same on server and clients.
pub struct Server<I: InputData, T: Transport> {
    transport: T,
    schema: NetSchema,
    schema_hash: u64,
    settings: ServerSettings,
    comp_ids: Vec<ComponentId>,
    replicated_id: ComponentId,
    relevance_id: ComponentId,
    transform_id: ComponentId,
    tick: u32,
    slots: Vec<Slot>,
    free: Vec<u32>,
    entity_slot: Vec<u32>,
    frames: Vec<Frame>,
    clients: Vec<Option<Box<ClientState<I>>>>,
    events: VecDeque<ServerEvent>,
    inbox: Inbox,
    arch_info: Vec<Option<ArchInfo>>,
    epoch: u64,
    /// The world's change tick when the last frame was built.
    built_at: u64,
    lag: LagCompensation,
    hit_query: QueryState<(Entity, &'static HitCapsule, &'static Transform)>,
    // Scratch, kept between ticks so steady state does not allocate.
    removed: Vec<Entity>,
    pending: Vec<Entity>,
    chunk_slots: Vec<u32>,
    message: Vec<u8>,
    input_bytes: Vec<u8>,
}

impl<I: InputData, T: Transport> core::fmt::Debug for Server<I, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Server")
            .field("tick", &self.tick)
            .field("slots", &self.slots.len())
            .finish_non_exhaustive()
    }
}

impl<I: InputData, T: Transport> Server<I, T> {
    /// A server for `world` over `transport` (configured with
    /// [`NetSchema::endpoint_config`]'s channels).
    pub fn new(
        transport: T,
        schema: NetSchema,
        settings: ServerSettings,
        world: &mut World,
    ) -> Self {
        let comp_ids = schema
            .components
            .iter()
            .map(|c| (c.register)(world))
            .collect();
        let replicated_id = world.register::<Replicated>();
        world.track_removals::<Replicated>();
        let lag_ticks = (settings.max_rewind * settings.tick_rate).ceil() as u32 + 2;
        let schema_hash = schema.hash();
        Self {
            transport,
            schema_hash,
            settings,
            comp_ids,
            replicated_id,
            relevance_id: world.register::<NetRelevance>(),
            transform_id: world.register::<Transform>(),
            tick: 1,
            slots: Vec::new(),
            free: Vec::new(),
            entity_slot: Vec::new(),
            frames: (0..HISTORY).map(|_| Frame::default()).collect(),
            clients: Vec::new(),
            events: VecDeque::new(),
            inbox: Inbox::default(),
            arch_info: Vec::new(),
            epoch: world.resync_epoch(),
            built_at: 0,
            lag: LagCompensation::new(lag_ticks),
            hit_query: QueryState::new(world),
            removed: Vec::new(),
            pending: Vec::new(),
            chunk_slots: Vec::new(),
            message: Vec::with_capacity(settings.max_snapshot_bytes + 64),
            input_bytes: vec![0; core::mem::size_of::<I>()],
            schema,
        }
    }

    /// The tick being simulated (between [`Self::update`] and [`Self::send`]).
    pub fn tick(&self) -> u32 {
        self.tick
    }

    /// Seconds per tick.
    pub fn dt(&self) -> f32 {
        (1.0 / self.settings.tick_rate) as f32
    }

    /// The settings.
    pub fn settings(&self) -> &ServerSettings {
        &self.settings
    }

    /// The transport (statistics, NAT traversal, disconnecting).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// The transport, mutably.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// The lag compensation history (hit shapes of past ticks).
    pub fn lag(&self) -> &LagCompensation {
        &self.lag
    }

    /// The connected clients.
    pub fn clients(&self) -> impl Iterator<Item = ClientId> + '_ {
        self.clients.iter().flatten().map(|c| c.id)
    }

    /// A client's counters.
    pub fn stats(&self, client: ClientId) -> Option<ServerStats> {
        self.client(client).map(|c| c.stats)
    }

    /// The next connection event.
    pub fn poll_event(&mut self) -> Option<ServerEvent> {
        self.events.pop_front()
    }

    fn client(&self, id: ClientId) -> Option<&ClientState<I>> {
        self.clients.get(usize::from(id.0))?.as_deref()
    }

    /// Receives (connections, inputs, client events, delivery notifications) and writes
    /// every client's input for this tick into its pawns' [`NetInput`]. Call at the start
    /// of each tick, before the game's systems.
    pub fn update(&mut self, world: &mut World, now: Instant) {
        engine_profiler::zone!("net::server::update");
        self.inbox.clear();
        self.transport.update(now);
        while let Some(event) = self.transport.poll_event() {
            match event {
                Event::Connected(conn) => self.connect(conn),
                Event::Disconnected(conn, reason) => self.disconnect(world, conn, reason),
                Event::Delivered(conn, ch, msg) if ch == CH_SNAPSHOT => {
                    self.notified(conn, msg, true);
                }
                Event::Lost(conn, ch, msg) if ch == CH_SNAPSHOT => self.notified(conn, msg, false),
                _ => {}
            }
        }
        for index in 0..self.clients.len() {
            let Some(conn) = self.clients[index].as_ref().map(|c| c.conn) else {
                continue;
            };
            while let Some(msg) = self.transport.receive(conn, CH_UNRELIABLE) {
                match msg.first() {
                    Some(&wire::TAG_INPUT) => {
                        if let Some(c) = self.clients[index].as_deref_mut() {
                            read_inputs(c, msg, self.tick, &mut self.input_bytes);
                        }
                    }
                    Some(&wire::TAG_EVENT) => {
                        if let Some((kind, tick, payload)) = wire::parse_event(msg) {
                            self.inbox.push(kind, index as u16, tick, payload);
                        }
                    }
                    _ => {}
                }
            }
            while let Some(msg) = self.transport.receive(conn, CH_RELIABLE) {
                if let Some((kind, tick, payload)) = wire::parse_event(msg) {
                    self.inbox.push(kind, index as u16, tick, payload);
                }
            }
        }
        self.apply_inputs(world);
    }

    fn connect(&mut self, conn: ConnectionId) {
        let index = usize::from(conn.index);
        if self.clients.len() <= index {
            self.clients.resize_with(index + 1, || None);
        }
        let id = ClientId(conn.index);
        self.clients[index] = Some(Box::new(ClientState {
            conn,
            id,
            ghosts: Vec::new(),
            sent: (0..wire::SENT_RING).map(|_| Sent::default()).collect(),
            inputs: vec![(u32::MAX, InputHeader::default(), I::default()); INPUT_RING as usize],
            newest_input: 0,
            last_input: (I::default(), InputHeader::default()),
            pawns: Vec::new(),
            acked_snapshot: 0,
            view: None,
            avg_record_bits: 64.0,
            scratch: Vec::new(),
            budget: 0,
            candidates: Vec::new(),
            kept: Vec::new(),
            measure: BitWriter::with_limit(1 << 16),
            writer: BitWriter::with_limit(self.settings.max_snapshot_bytes),
            out: Vec::with_capacity(self.settings.max_snapshot_bytes + 16),
            stats: ServerStats::default(),
        }));
        let mut welcome = [0u8; 19];
        welcome[0] = wire::TAG_WELCOME;
        welcome[1..9].copy_from_slice(&self.schema_hash.to_le_bytes());
        welcome[9..11].copy_from_slice(&conn.index.to_le_bytes());
        welcome[11..15].copy_from_slice(&self.tick.to_le_bytes());
        welcome[15..19].copy_from_slice(&(self.settings.tick_rate as f32).to_le_bytes());
        let _ = self.transport.send(conn, CH_RELIABLE, &welcome);
        self.events.push_back(ServerEvent::ClientConnected(id));
    }

    fn disconnect(&mut self, world: &mut World, conn: ConnectionId, reason: DisconnectReason) {
        let index = usize::from(conn.index);
        let Some(c) = self.clients.get_mut(index).and_then(Option::take) else {
            return;
        };
        if c.conn != conn {
            self.clients[index] = Some(c);
            return;
        }
        for &pawn in &c.pawns {
            self.set_owner(world, pawn, None);
        }
        self.events
            .push_back(ServerEvent::ClientDisconnected(c.id, reason));
    }

    fn notified(&mut self, conn: ConnectionId, msg: MessageId, delivered: bool) {
        let Some(c) = self
            .clients
            .get_mut(usize::from(conn.index))
            .and_then(|c| c.as_deref_mut())
            .filter(|c| c.conn == conn)
        else {
            return;
        };
        let sent = &mut c.sent[msg.0 as usize % wire::SENT_RING];
        if !sent.active || sent.msg != msg.0 {
            return;
        }
        sent.active = false;
        let tick = sent.tick;
        if delivered {
            c.stats.delivered += 1;
            c.acked_snapshot = c.acked_snapshot.max(tick);
        } else {
            c.stats.lost += 1;
        }
        for r in &sent.records {
            let Some(g) = c.ghosts.get_mut(r.slot as usize) else {
                continue;
            };
            match (r.removal, delivered) {
                (false, true) => {
                    if r.generation == g.sent_gen && tick > g.acked {
                        g.acked_prev = g.acked;
                        g.acked = tick;
                    }
                    if tick == g.alive_tick {
                        g.lost_alive = false;
                    }
                }
                (false, false) => {
                    if tick == g.alive_tick {
                        g.lost_alive = true;
                    }
                }
                (true, true) => {
                    if tick > g.alive_tick {
                        *g = Ghost::default();
                    }
                }
                (true, false) => {
                    if tick == g.removal_tick {
                        g.lost_removal = true;
                    }
                }
            }
        }
    }

    fn apply_inputs(&mut self, world: &mut World) {
        let tick = self.tick;
        for c in self.clients.iter_mut().flatten() {
            let entry = c.inputs[(tick % INPUT_RING) as usize];
            let (input, header) = if entry.0 == tick {
                let mut h = entry.1;
                h.fresh = true;
                (entry.2, h)
            } else {
                c.stats.inputs_missing += u64::from(!c.pawns.is_empty() && c.newest_input > 0);
                let mut h = c.last_input.1;
                h.tick = tick;
                h.fresh = false;
                (c.last_input.0, h)
            };
            c.last_input = (input, header);
            for &pawn in &c.pawns {
                if let Some(n) = world.get_mut::<NetInput<I>>(pawn) {
                    n.input = input;
                    n.header = header;
                }
            }
        }
    }

    /// Gives `entity` (which has [`Replicated`]) to `owner`: that client predicts it and
    /// its input arrives in the entity's [`NetInput`]. `None`: the server owns it again.
    /// Returns false if the entity is not replicated.
    pub fn set_owner(
        &mut self,
        world: &mut World,
        entity: Entity,
        owner: Option<ClientId>,
    ) -> bool {
        let Some(rep) = world.get::<Replicated>(entity).copied() else {
            return false;
        };
        for c in self.clients.iter_mut().flatten() {
            c.pawns.retain(|&p| p != entity);
        }
        let code = owner.map_or(0, |c| c.0 + 1);
        let mut rep = rep;
        rep.owner = code;
        if let Some(slot) = rep.id.checked_sub(1)
            && let Some(s) = self.slots.get_mut(slot as usize)
            && s.entity == entity
            && s.owner != code
        {
            // A new generation: every client replaces the entity, the new owner as
            // predicted, the old one as interpolated.
            s.generation = s.generation.wrapping_add(1);
            s.owner = code;
            s.changed_at = self.tick;
            rep.generation = s.generation;
        }
        if let Some(r) = world.get_mut_untracked::<Replicated>(entity) {
            *r = rep;
        }
        match owner {
            Some(id) => {
                if let Some(c) = self
                    .clients
                    .get_mut(usize::from(id.0))
                    .and_then(|c| c.as_deref_mut())
                {
                    c.pawns.push(entity);
                }
                if !world.has::<NetInput<I>>(entity) {
                    world.insert(entity, NetInput::<I>::default());
                }
            }
            None => {
                world.remove::<NetInput<I>>(entity);
            }
        }
        true
    }

    /// Feeds the listen server host's (or a bot's) input to `entity`, which the server
    /// owns: the host plays in the server's world directly, without a network.
    pub fn set_local_input(&mut self, world: &mut World, entity: Entity, input: I) {
        let header = InputHeader {
            tick: self.tick,
            view_delay: 0,
            fresh: true,
        };
        if let Some(n) = world.get_mut::<NetInput<I>>(entity) {
            n.input = input;
            n.header = header;
        } else {
            world.insert(entity, NetInput { input, header });
        }
    }

    /// Where a client's view is (its camera), for [`NetRelevance`].
    pub fn set_view(&mut self, client: ClientId, position: Option<Vec3>) {
        if let Some(c) = self
            .clients
            .get_mut(usize::from(client.0))
            .and_then(|c| c.as_deref_mut())
        {
            c.view = position;
        }
    }

    /// Sends a one-shot event of a type registered with [`NetSchema::event`]. Reliable
    /// events arrive once and in order; unreliable ones arrive at most once (dropped
    /// silently when lost). Returns false if the type is not registered.
    pub fn send_event<E: bytemuck::Pod>(
        &mut self,
        target: Target,
        reliable: bool,
        event: &E,
    ) -> bool {
        self.send_event_bytes(
            target,
            reliable,
            core::any::TypeId::of::<E>(),
            bytemuck::bytes_of(event),
        )
    }

    /// [`Self::send_event`] for an event given by its type and bytes (a session that knows
    /// only its input type, as the player's: ADR 0106). False if the type is not
    /// registered or the bytes are not its size.
    pub fn send_event_bytes(
        &mut self,
        target: Target,
        reliable: bool,
        event_type: core::any::TypeId,
        bytes: &[u8],
    ) -> bool {
        let Some(kind) = self.schema.event_index(event_type) else {
            return false;
        };
        if self.schema.events[kind].size != bytes.len() {
            return false;
        }
        wire::event_message(&mut self.message, kind as u16, self.tick, bytes);
        let channel = if reliable { CH_RELIABLE } else { CH_UNRELIABLE };
        for c in self.clients.iter().flatten() {
            let to = match target {
                Target::All => true,
                Target::Client(id) => id == c.id,
                Target::AllExcept(id) => id != c.id,
            };
            if to {
                let _ = self.transport.send(c.conn, channel, &self.message);
            }
        }
        true
    }

    /// Calls `f(client, type, tick, bytes)` for every event received in the last
    /// [`Self::update`], whatever its type.
    pub fn read_event_bytes(&self, mut f: impl FnMut(ClientId, core::any::TypeId, u32, &[u8])) {
        for &(kind, sender, tick, start, len) in &self.inbox.records {
            if let Some(event) = self.schema.events.get(usize::from(kind)) {
                let bytes = &self.inbox.bytes[start as usize..(start + len) as usize];
                f(ClientId(sender), event.type_id, tick, bytes);
            }
        }
    }

    /// Calls `f(client, tick, event)` for every event of type `E` received in the last
    /// [`Self::update`].
    pub fn read_events<E: bytemuck::Pod>(&self, mut f: impl FnMut(ClientId, u32, E)) {
        if let Some(kind) = self.schema.event_index(core::any::TypeId::of::<E>()) {
            self.inbox
                .read::<E>(kind as u16, |sender, tick, e| f(ClientId(sender), tick, e));
        }
    }

    /// Ends the tick: assigns network ids, quantizes the networked state, records hit
    /// shapes, sends each client its snapshot, flushes the transport and advances the
    /// tick. Call after the game's systems.
    pub fn send(&mut self, world: &mut World, now: Instant) {
        self.send_with(world, now, None);
    }

    /// [`Self::send`], packing the clients' snapshots in parallel on `pool` (worth it
    /// from a few clients with many entities: each client's packing scans every
    /// networked entity).
    pub fn send_with(&mut self, world: &mut World, now: Instant, pool: Option<&ThreadPool>) {
        engine_profiler::zone!("net::server::send");
        self.sync_entities(world);
        self.build_frame(world);
        if self.settings.max_rewind > 0.0 {
            self.record_hit_shapes(world);
        }
        if self
            .tick
            .is_multiple_of(self.settings.snapshot_interval.max(1))
        {
            self.write_snapshots(pool);
        }
        self.transport.flush(now);
        self.tick += 1;
    }

    fn sync_entities(&mut self, world: &mut World) {
        let mut removed = core::mem::take(&mut self.removed);
        removed.clear();
        world.drain_removed::<Replicated>(|e| removed.push(e));
        for &e in &removed {
            self.free_entity(e);
        }
        self.removed = removed;
        if world.resync_epoch() != self.epoch {
            self.epoch = world.resync_epoch();
            for slot in 0..self.slots.len() {
                let s = self.slots[slot];
                if s.alive
                    && world
                        .get::<Replicated>(s.entity)
                        .is_none_or(|r| r.id != slot as u32 + 1)
                {
                    self.free_entity(s.entity);
                }
            }
        }
        self.update_arch_info(world);
        self.pending.clear();
        for (ai, arch) in world.archetypes().iter().enumerate() {
            let Some(info) = &self.arch_info[ai] else {
                continue;
            };
            for chunk in 0..arch.chunk_count() {
                let Some(reps) = arch
                    .pod_column(chunk, info.rep)
                    .and_then(|b| bytemuck::try_cast_slice::<u8, Replicated>(b).ok())
                else {
                    continue;
                };
                for (row, r) in reps.iter().enumerate() {
                    if r.id == 0 {
                        self.pending.push(arch.chunk_entities(chunk)[row]);
                    }
                }
            }
        }
        for i in 0..self.pending.len() {
            let entity = self.pending[i];
            let Some(rep) = world.get::<Replicated>(entity).copied() else {
                continue;
            };
            let slot = match self.free.pop() {
                Some(s) => s,
                None => {
                    self.slots.push(Slot {
                        entity,
                        generation: u16::MAX,
                        alive: false,
                        owner: 0,
                        changed_at: 0,
                        has_relevance: false,
                        relevance: NetRelevance::default(),
                        position: Vec3::ZERO,
                    });
                    (self.slots.len() - 1) as u32
                }
            };
            let s = &mut self.slots[slot as usize];
            s.entity = entity;
            s.generation = s.generation.wrapping_add(1);
            s.alive = true;
            s.owner = rep.owner;
            s.changed_at = self.tick;
            let index = entity.index() as usize;
            if self.entity_slot.len() <= index {
                self.entity_slot.resize(index + 1, NONE);
            }
            self.entity_slot[index] = slot;
            if let Some(r) = world.get_mut_untracked::<Replicated>(entity) {
                r.id = slot + 1;
                r.generation = s.generation;
            }
            if let Some(owner) = rep.owner.checked_sub(1)
                && let Some(c) = self
                    .clients
                    .get_mut(usize::from(owner))
                    .and_then(|c| c.as_deref_mut())
                && !c.pawns.contains(&entity)
            {
                c.pawns.push(entity);
            }
        }
    }

    fn free_entity(&mut self, entity: Entity) {
        let Some(&slot) = self.entity_slot.get(entity.index() as usize) else {
            return;
        };
        if slot == NONE {
            return;
        }
        let s = &mut self.slots[slot as usize];
        if !s.alive || s.entity != entity {
            return;
        }
        s.alive = false;
        s.changed_at = self.tick;
        self.entity_slot[entity.index() as usize] = NONE;
        self.free.push(slot);
        for c in self.clients.iter_mut().flatten() {
            c.pawns.retain(|&p| p != entity);
        }
    }

    fn update_arch_info(&mut self, world: &World) {
        let archetypes = world.archetypes();
        for arch in archetypes.iter().skip(self.arch_info.len()) {
            let info = arch.column_index(self.replicated_id).map(|rep| {
                let mut comps = Vec::new();
                let mut mask = 0u64;
                let mut words = 0;
                for (si, &id) in self.comp_ids.iter().enumerate() {
                    if let Some(col) = arch.column_index(id) {
                        comps.push((si, col, words));
                        mask |= 1 << si;
                        words += self.schema.components[si].words;
                    }
                }
                ArchInfo {
                    rep,
                    comps,
                    mask,
                    words,
                    relevance: arch.column_index(self.relevance_id),
                    transform: arch.column_index(self.transform_id),
                }
            });
            self.arch_info.push(info);
        }
    }

    fn build_frame(&mut self, world: &mut World) {
        engine_profiler::zone!("net::server::quantize");
        let tick = self.tick;
        let n = self.slots.len();
        let (prev, frame) = {
            let i = (tick % HISTORY) as usize;
            let (a, b) = self.frames.split_at_mut(i);
            let (frame, c) = b.split_first_mut().unwrap_or_else(|| unreachable!());
            // The previous tick's frame: just before `i` (wrapping into `c`).
            let prev = if i > 0 { a.last() } else { c.last() };
            (prev.filter(|p| p.valid && p.tick + 1 == tick), frame)
        };
        frame.tick = tick;
        frame.valid = true;
        frame.offset.clear();
        frame.offset.resize(n, NONE);
        frame.mask.clear();
        frame.mask.resize(n, 0);
        frame.generation.clear();
        frame.generation.resize(n, 0);
        frame.words.clear();
        let schema = &self.schema;
        for ai in 0..self.arch_info.len() {
            let Some(info) = &self.arch_info[ai] else {
                continue;
            };
            let chunks = world.archetypes()[ai].chunk_count();
            for chunk in 0..chunks {
                // The chunk's rows: their slots and where their words go.
                self.chunk_slots.clear();
                {
                    let arch = &world.archetypes()[ai];
                    let Some(reps) = arch
                        .pod_column(chunk, info.rep)
                        .and_then(|b| bytemuck::try_cast_slice::<u8, Replicated>(b).ok())
                    else {
                        continue;
                    };
                    let entities = arch.chunk_entities(chunk);
                    for (r, &entity) in reps.iter().zip(entities) {
                        let s = r.id.wrapping_sub(1);
                        let Some(slot) = self
                            .slots
                            .get_mut(s as usize)
                            .filter(|x| x.entity == entity)
                        else {
                            self.chunk_slots.push(NONE);
                            continue;
                        };
                        let start = frame.words.len();
                        frame.offset[s as usize] = start as u32;
                        frame.mask[s as usize] = info.mask;
                        frame.generation[s as usize] = slot.generation;
                        frame.words.resize(start + info.words, 0);
                        slot.has_relevance = info.relevance.is_some();
                        self.chunk_slots.push(s);
                    }
                    if let Some(col) = info.relevance
                        && let Some(bytes) = arch.pod_column(chunk, col)
                    {
                        let size = core::mem::size_of::<NetRelevance>();
                        let tsize = core::mem::size_of::<Transform>();
                        let transforms = info.transform.and_then(|c| arch.pod_column(chunk, c));
                        for (row, &s) in self.chunk_slots.iter().enumerate() {
                            let Some(slot) = self.slots.get_mut(s as usize) else {
                                continue;
                            };
                            slot.relevance =
                                bytemuck::pod_read_unaligned(&bytes[row * size..(row + 1) * size]);
                            if let Some(tb) = transforms {
                                let t: Transform = bytemuck::pod_read_unaligned(
                                    &tb[row * tsize..(row + 1) * tsize],
                                );
                                slot.position = t.translation;
                            }
                        }
                    }
                }
                // Quantize each component once; lossy ones are rounded in place, so the
                // server goes on from exactly what clients reconstruct (Fiedler, "State
                // Synchronization").
                for &(si, col, w) in &info.comps {
                    let c = &schema.components[si];
                    // A chunk column not written since the last frame (ECS change versions,
                    // ADR 0017; spawns, despawns and moves stamp it too) holds the same
                    // rows and values: copy their words instead of quantizing again.
                    let unchanged = prev.is_some()
                        && world.archetypes()[ai]
                            .chunk_version(chunk, self.comp_ids[si])
                            .is_some_and(|v| v <= self.built_at);
                    if unchanged && let Some(p) = prev {
                        let bytes = world.archetypes()[ai].pod_column(chunk, col).unwrap_or(&[]);
                        for (row, &s) in self.chunk_slots.iter().enumerate() {
                            if s == NONE {
                                continue;
                            }
                            let o = frame.offset[s as usize] as usize + w;
                            match p.state(s) {
                                Some((pm, _, po)) if pm == info.mask => {
                                    let po = po as usize + w;
                                    frame.words[o..o + c.words]
                                        .copy_from_slice(&p.words[po..po + c.words]);
                                }
                                _ => {
                                    if let Some(value) = bytes.get(row * c.size..(row + 1) * c.size)
                                    {
                                        c.quantize(value, &mut frame.words[o..o + c.words]);
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    if c.lossy {
                        let Some(bytes) = world.pod_column_mut(ai, chunk, col, false) else {
                            continue;
                        };
                        for (row, &s) in self.chunk_slots.iter().enumerate() {
                            if s == NONE {
                                continue;
                            }
                            let o = frame.offset[s as usize] as usize + w;
                            let words = &mut frame.words[o..o + c.words];
                            let value = &mut bytes[row * c.size..(row + 1) * c.size];
                            c.quantize(value, words);
                            c.dequantize(words, value);
                        }
                    } else {
                        let Some(bytes) = world.archetypes()[ai].pod_column(chunk, col) else {
                            continue;
                        };
                        for (row, &s) in self.chunk_slots.iter().enumerate() {
                            if s == NONE {
                                continue;
                            }
                            let o = frame.offset[s as usize] as usize + w;
                            c.quantize(
                                &bytes[row * c.size..(row + 1) * c.size],
                                &mut frame.words[o..o + c.words],
                            );
                        }
                    }
                }
            }
        }
        self.built_at = world.change_tick();
        // What changed since the previous tick.
        for (s, slot) in self.slots.iter_mut().enumerate() {
            if !slot.alive {
                continue;
            }
            let Some((mask, generation, o)) = frame.state(s as u32) else {
                continue;
            };
            let same = prev.is_some_and(|p| {
                p.state(s as u32).is_some_and(|(pm, pg, po)| {
                    let len = words_of(schema, mask);
                    pm == mask
                        && pg == generation
                        && p.words[po as usize..po as usize + len]
                            == frame.words[o as usize..o as usize + len]
                })
            });
            if !same {
                slot.changed_at = tick;
            }
        }
    }

    fn record_hit_shapes(&mut self, world: &World) {
        let Self { lag, hit_query, .. } = self;
        lag.begin(self.tick);
        hit_query.for_each_readonly(world, |(e, capsule, t)| {
            lag.add(e, capsule, t.translation, t.rotation);
        });
        lag.finish();
    }

    /// Builds every client's snapshot (in parallel on `pool` when given: each client's
    /// packing reads the shared frames and writes only its own state, ADR 0019), then
    /// queues them on the transport in client order.
    fn write_snapshots(&mut self, pool: Option<&ThreadPool>) {
        engine_profiler::zone!("net::server::snapshots");
        let Self {
            schema,
            slots,
            frames,
            clients,
            transport,
            settings,
            tick,
            ..
        } = self;
        for c in clients.iter_mut().flatten() {
            c.budget = transport
                .send_budget(c.conn)
                .unwrap_or(0)
                .min(settings.max_snapshot_bytes);
        }
        let shared = Shared {
            schema,
            slots,
            frames,
            tick: *tick,
            owner_priority: settings.owner_priority,
        };
        let shared = &shared;
        match pool {
            Some(pool) if clients.iter().flatten().count() > 1 => pool.scope(|s| {
                for c in clients.iter_mut().flatten() {
                    s.spawn(move || build_snapshot(shared, c));
                }
            }),
            _ => {
                for c in clients.iter_mut().flatten() {
                    build_snapshot(shared, c);
                }
            }
        }
        for c in clients.iter_mut().flatten() {
            if c.out.is_empty() {
                continue;
            }
            let result = transport.send(c.conn, CH_SNAPSHOT, &c.out);
            deliver_snapshot(c, result, *tick);
        }
    }
}

/// What every client's snapshot reads.
struct Shared<'a> {
    schema: &'a NetSchema,
    slots: &'a [Slot],
    frames: &'a [Frame],
    tick: u32,
    owner_priority: f32,
}

/// Packs one client's snapshot into `c.out` (empty: nothing to send this tick) and its
/// records into `c.scratch`.
fn build_snapshot<I: InputData>(sh: &Shared<'_>, c: &mut ClientState<I>) {
    engine_profiler::zone!("net::server::snapshot");
    c.out.clear();
    let tick = sh.tick;
    let budget = c.budget;
    if budget < 32 {
        return;
    }
    let n = sh.slots.len();
    if c.ghosts.len() < n {
        c.ghosts.resize(n, Ghost::default());
    }
    let owner_code = c.id.0 + 1;
    // Candidates and their accumulated priority.
    c.candidates.clear();
    for (s, (slot, g)) in sh.slots.iter().zip(c.ghosts.iter_mut()).enumerate() {
        let relevance = if slot.alive {
            relevance(c.view, slot)
        } else {
            None
        };
        let need = if let Some(rel) = relevance {
            if g.has && g.sent_gen != slot.generation {
                // A new entity in this slot: the client gets it whole.
                g.acked = 0;
                g.acked_prev = 0;
                g.alive_tick = 0;
                g.lost_alive = false;
            }
            let removal_in_flight = g.removal_tick > g.alive_tick;
            let need = if g.acked == 0 {
                g.alive_tick == 0 || g.lost_alive || removal_in_flight
            } else {
                slot.changed_at > g.alive_tick
                    || (g.lost_alive && g.alive_tick > g.acked)
                    || removal_in_flight
            };
            if need {
                g.priority += if slot.owner == owner_code {
                    rel * sh.owner_priority
                } else {
                    rel
                };
            }
            need
        } else if g.has {
            let need = g.removal_tick <= g.alive_tick || g.lost_removal;
            if need {
                g.priority += 10.0;
            }
            need
        } else {
            false
        };
        if need {
            c.candidates.push((g.priority, s as u32));
        }
    }
    // The highest priorities that probably fit.
    let header_bytes = 5;
    let limit = budget - header_bytes;
    let estimate = ((limit * 8) as f32 / c.avg_record_bits.max(8.0) * 1.5) as usize + 8;
    let k = estimate.min(c.candidates.len());
    if k < c.candidates.len() {
        c.candidates
            .select_nth_unstable_by(k, |a, b| b.0.total_cmp(&a.0));
        c.stats.deferred += (c.candidates.len() - k) as u64;
    }
    let chosen = &mut c.candidates[..k];

    let w = &mut c.writer;
    w.reset(limit);
    // Header: input acknowledgement and lead, default baseline.
    let has_input = c.newest_input > 0;
    w.bit(has_input);
    if has_input {
        let lead = (i64::from(c.newest_input) - i64::from(tick)).clamp(-1000, 1000) as i32;
        w.signed(lead, 2);
        c.stats.input_lead = lead;
    }
    let default_base = c.acked_snapshot;
    let has_default = default_base > 0 && tick - default_base < HISTORY;
    w.bit(has_default);
    if has_default {
        w.golomb(tick - default_base, 2);
    }
    let mut records_written = 0usize;
    let mut prev: Option<u32> = None;
    let mut full = 0u64;
    let frame_index = (tick % HISTORY) as usize;
    // Which of the chosen fit, decided in priority order (Fiedler: skip what does not
    // fit, try the next); then they are written in slot order so the slot gaps stay
    // small.
    chosen.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
    let gap_bits =
        1 + crate::bits::golomb_bits(n as u32 / (chosen.len() as u32).max(1), wire::K_GAP) as usize;
    let mut room = (limit * 8).saturating_sub(w.bits() + 1);
    let mut skipped = 0u64;
    c.kept.clear();
    for &(_, s) in chosen.iter() {
        let slot = &sh.slots[s as usize];
        let removal = !(slot.alive && relevance(c.view, slot).is_some());
        let g = &c.ghosts[s as usize];
        let cost = if removal {
            gap_bits + 2 + 16
        } else if g.acked > 0 && g.last_bits > 0 {
            // A delta again: about as large as the last one (+12 %); the final pass
            // skips the rare record that turns out larger than its room.
            gap_bits + usize::from(g.last_bits) * 9 / 8 + 2
        } else {
            c.measure.reset(1 << 16);
            if encode_record(
                sh.schema,
                sh.frames,
                frame_index,
                tick,
                slot,
                g,
                s,
                removal,
                default_base,
                owner_code,
                &mut c.measure,
            )
            .is_none()
            {
                continue;
            }
            gap_bits + c.measure.bits()
        };
        if cost <= room {
            room -= cost;
            c.kept.push(s);
        } else {
            skipped += 1;
        }
    }
    c.kept.sort_unstable();
    // Records go into a scratch list until the message id is known.
    c.scratch.clear();
    let records_start_bits = w.bits();
    for &s in &c.kept {
        let slot = &sh.slots[s as usize];
        let g = &mut c.ghosts[s as usize];
        let mark = w.bits();
        w.bit(true);
        w.golomb(prev.map_or(s, |p| s - p - 1), wire::K_GAP);
        let removal = !(slot.alive && relevance(c.view, slot).is_some());
        let Some(is_full) = encode_record(
            sh.schema,
            sh.frames,
            frame_index,
            tick,
            slot,
            g,
            s,
            removal,
            default_base,
            owner_code,
            w,
        ) else {
            w.rewind(mark);
            continue;
        };
        if w.bits() + 1 > limit * 8 {
            // The estimate was short (rare): leave it for the next snapshot.
            w.rewind(mark);
            skipped += 1;
            continue;
        }
        prev = Some(s);
        records_written += 1;
        full += u64::from(is_full);
        g.priority = 0.0;
        g.last_bits = if is_full {
            0
        } else {
            (w.bits() - mark).min(usize::from(u16::MAX)) as u16
        };
        if removal {
            g.removal_tick = tick;
            g.lost_removal = false;
        } else {
            g.has = true;
            g.sent_gen = slot.generation;
            g.alive_tick = tick;
            g.lost_alive = false;
        }
        c.scratch.push(Record {
            slot: s,
            removal,
            generation: if removal { g.sent_gen } else { slot.generation },
        });
    }
    w.bit(false);
    let record_bits = w.bits() - records_start_bits;
    if records_written > 0 {
        let avg = record_bits as f32 / records_written as f32;
        c.avg_record_bits = c.avg_record_bits * 0.8 + avg * 0.2;
    }
    let bits = w.finish();
    c.out.push(wire::TAG_SNAPSHOT);
    c.out.extend_from_slice(&tick.to_le_bytes());
    c.out.extend_from_slice(bits);
    c.stats.deferred += skipped;
    c.stats.records += records_written as u64;
    c.stats.full_records += full;
}

/// Records a queued snapshot for delivery notification (or marks its records lost).
fn deliver_snapshot<I: InputData>(
    c: &mut ClientState<I>,
    result: Result<MessageId, engine_net_transport::SendError>,
    tick: u32,
) {
    match result {
        Ok(msg) => {
            c.stats.snapshots += 1;
            c.stats.snapshot_bytes += c.out.len() as u64;
            c.stats.last_snapshot_bytes = c.out.len();
            let entry = &mut c.sent[msg.0 as usize % wire::SENT_RING];
            if entry.active {
                // Never resolved (more than the ring in flight): treat as lost.
                c.stats.lost += 1;
                for r in &entry.records {
                    if let Some(g) = c.ghosts.get_mut(r.slot as usize) {
                        if r.removal {
                            g.lost_removal = true;
                        } else {
                            g.lost_alive = true;
                        }
                    }
                }
            }
            let entry = &mut c.sent[msg.0 as usize % wire::SENT_RING];
            core::mem::swap(&mut entry.records, &mut c.scratch);
            entry.active = true;
            entry.msg = msg.0;
            entry.tick = tick;
        }
        Err(_) => {
            // Not queued: as if lost.
            for r in &c.scratch {
                if let Some(g) = c.ghosts.get_mut(r.slot as usize) {
                    if r.removal {
                        g.lost_removal = true;
                    } else {
                        g.lost_alive = true;
                    }
                }
            }
        }
    }
    c.scratch.clear();
}

/// Writes one entity record from its kind on: a removal, a delta against the client's
/// acknowledged state of it, or the whole state. Returns whether it was whole, or `None`
/// if the entity has no state this tick.
#[allow(clippy::too_many_arguments)]
fn encode_record(
    schema: &NetSchema,
    frames: &[Frame],
    frame_index: usize,
    tick: u32,
    slot: &Slot,
    g: &Ghost,
    s: u32,
    removal: bool,
    default_base: u32,
    owner_code: u16,
    w: &mut BitWriter,
) -> Option<bool> {
    if removal {
        w.write(wire::KIND_REMOVE, 2);
        w.write(u32::from(g.sent_gen), 16);
        return Some(false);
    }
    let frame = &frames[frame_index];
    let (mask, generation, o) = frame.state(s)?;
    let base = if g.acked > 0 && g.sent_gen == generation && tick - g.acked < HISTORY {
        let b = &frames[(g.acked % HISTORY) as usize];
        (b.valid && b.tick == g.acked)
            .then(|| b.state(s))
            .flatten()
            .filter(|&(bm, bg, _)| bm == mask && bg == generation)
            .map(|(_, _, bo)| (b, bo))
    } else {
        None
    };
    let cur = &frame.words[o as usize..];
    Some(match base {
        Some((b, bo)) => {
            w.write(wire::KIND_DELTA, 2);
            let default = g.acked == default_base;
            w.bit(default);
            if !default {
                w.golomb(tick - g.acked, 2);
            }
            // An older acknowledged state for predicted baselines, if both sides have it.
            let older = (g.acked_prev > 0 && tick - g.acked_prev < HISTORY)
                .then(|| &frames[(g.acked_prev % HISTORY) as usize])
                .filter(|p| p.valid && p.tick == g.acked_prev)
                .and_then(|p| {
                    p.state(s)
                        .filter(|&(pm, pg, _)| pm == mask && pg == generation)
                        .map(|(_, _, po)| Older {
                            words: &p.words[po as usize..],
                            ahead: i64::from(tick - g.acked),
                            span: i64::from(g.acked - g.acked_prev),
                        })
                });
            w.bit(older.is_some());
            if older.is_some() {
                w.golomb(g.acked - g.acked_prev - 1, 0);
            }
            encode_delta(schema, mask, cur, &b.words[bo as usize..], older, w);
            false
        }
        None => {
            w.write(wire::KIND_FULL, 2);
            w.write(u32::from(generation), 16);
            w.bit(slot.owner == owner_code);
            encode_full(schema, mask, cur, w);
            true
        }
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

fn relevance(view: Option<Vec3>, s: &Slot) -> Option<f32> {
    if !s.has_relevance {
        return Some(1.0);
    }
    let r = s.relevance;
    let p = r.priority.max(0.0);
    match view {
        Some(v) if r.radius > 0.0 => {
            let d2 = (s.position - v).length_squared();
            let r2 = r.radius * r.radius;
            (d2 <= r2).then(|| p * (1.0 - 0.75 * d2 / r2))
        }
        _ => Some(p),
    }
}

/// A second baseline for prediction: the words of an older acknowledged state, and the
/// ticks for extrapolating from it through the baseline to now.
#[derive(Clone, Copy)]
pub(crate) struct Older<'a> {
    pub(crate) words: &'a [u32],
    /// Ticks from the baseline to now.
    pub(crate) ahead: i64,
    /// Ticks from the older state to the baseline.
    pub(crate) span: i64,
}

/// The baseline word `b` extrapolated linearly through the older word `p` (integer
/// arithmetic, truncating: the same on every machine).
#[inline]
pub(crate) fn extrapolate(b: u32, p: u32, older: &Older<'_>) -> u32 {
    let step = i64::from(b.wrapping_sub(p) as i32);
    b.wrapping_add(((step * older.ahead) / older.span.max(1)) as i32 as u32)
}

/// Bits the words of `cur` take against `reference` (Int words only; others cost the
/// same either way).
fn cost(cur: &[u32], reference: impl Iterator<Item = u32>, kinds: &[WordKind]) -> u32 {
    cur.iter()
        .zip(reference)
        .zip(kinds)
        .map(|((&x, r), &k)| match (k, x == r) {
            (_, true) => 1,
            (WordKind::Int, false) => {
                1 + crate::bits::golomb_bits(
                    crate::bits::zigzag(x.wrapping_sub(r) as i32),
                    wire::K_DELTA,
                )
            }
            _ => 0,
        })
        .sum()
}

/// Encodes `cur` against `base` component by component: a changed bit per component and
/// per word, then the value: integer deltas zigzag exponential-Golomb coded, raw words
/// whole, bits implied by their changed bit. With an `older` state, a changed component
/// with quantized floats says (one bit) whether its words are coded against the baseline
/// extrapolated through the older state instead (Unity's predicted baselines; the
/// encoder picks the cheaper).
pub(crate) fn encode_delta(
    schema: &NetSchema,
    mask: u64,
    cur: &[u32],
    base: &[u32],
    older: Option<Older<'_>>,
    w: &mut BitWriter,
) {
    let mut m = mask;
    let mut o = 0;
    while m != 0 {
        let i = m.trailing_zeros() as usize;
        m &= m - 1;
        let c = &schema.components[i];
        let cw = &cur[o..o + c.words];
        let bw = &base[o..o + c.words];
        let pw = older.map(|p| &p.words[o..o + c.words]);
        o += c.words;
        let changed = cw != bw;
        w.bit(changed);
        if !changed {
            continue;
        }
        let mut predicted = false;
        if let (Some(old), Some(pw)) = (older, pw)
            && c.any_linear
        {
            let guess = |j: usize| {
                if c.linear[j] {
                    extrapolate(bw[j], pw[j], &old)
                } else {
                    bw[j]
                }
            };
            predicted = cost(cw, (0..c.words).map(guess), &c.kinds)
                < cost(cw, bw.iter().copied(), &c.kinds);
            w.bit(predicted);
        }
        for (j, (&x, &kind)) in cw.iter().zip(&c.kinds).enumerate() {
            let y = match (predicted, older, pw) {
                (true, Some(old), Some(pw)) if c.linear[j] => extrapolate(bw[j], pw[j], &old),
                _ => bw[j],
            };
            if x == y {
                w.bit(false);
                continue;
            }
            w.bit(true);
            match kind {
                WordKind::Int => w.signed(x.wrapping_sub(y) as i32, wire::K_DELTA),
                WordKind::Raw => w.write(x, 32),
                WordKind::Bit => {}
            }
        }
    }
}

/// Encodes a whole state: the component mask, then every word.
pub(crate) fn encode_full(schema: &NetSchema, mask: u64, cur: &[u32], w: &mut BitWriter) {
    let n = schema.components.len() as u32;
    w.write(mask as u32, n.min(32));
    if n > 32 {
        w.write((mask >> 32) as u32, n - 32);
    }
    let mut m = mask;
    let mut o = 0;
    while m != 0 {
        let i = m.trailing_zeros() as usize;
        m &= m - 1;
        let c = &schema.components[i];
        for (&x, &kind) in cur[o..o + c.words].iter().zip(&c.kinds) {
            match kind {
                WordKind::Int => w.signed(x as i32, wire::K_FULL),
                WordKind::Raw => w.write(x, 32),
                WordKind::Bit => w.write(x, 1),
            }
        }
        o += c.words;
    }
}

fn read_inputs<I: InputData>(c: &mut ClientState<I>, msg: &[u8], server_tick: u32, buf: &mut [u8]) {
    if msg.len() < 6 {
        return;
    }
    let newest = u32::from_le_bytes([msg[1], msg[2], msg[3], msg[4]]);
    let count = u32::from(msg[5]);
    if count == 0 || count > wire::MAX_REDUNDANT_INPUTS || newest < count {
        return;
    }
    let mut r = crate::bits::BitReader::new(&msg[6..]);
    let mut view = 0u32;
    let mut input = I::default();
    for i in 0..count {
        let tick = newest - (count - 1 - i);
        let Some(same_view) = r.bit() else { return };
        if !same_view {
            let Some(v) = r.golomb(6) else { return };
            view = v;
        }
        let Some(same) = r.bit() else { return };
        if !same {
            for b in buf.iter_mut() {
                let Some(x) = r.read(8) else { return };
                *b = x as u8;
            }
            input = bytemuck::pod_read_unaligned(buf);
        }
        if tick < server_tick {
            if tick > c.newest_input {
                c.stats.inputs_late += 1; // arrived after its tick was simulated
                c.newest_input = tick;
            }
            continue;
        }
        if tick >= server_tick + INPUT_RING {
            continue; // implausibly far ahead
        }
        let entry = &mut c.inputs[(tick % INPUT_RING) as usize];
        if entry.0 != tick {
            *entry = (
                tick,
                InputHeader {
                    tick,
                    view_delay: view.min(u32::from(u16::MAX)) as u16,
                    fresh: true,
                },
                input,
            );
        }
        c.newest_input = c.newest_input.max(tick);
    }
}
