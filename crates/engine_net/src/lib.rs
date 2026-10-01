//! Replication, prediction and lag compensation (ADR 0074) on the transport of ADR 0070.
//!
//! The model is ADR 0019's: an authoritative server simulates at a fixed tick; clients
//! receive snapshots of the components marked for the network, predict their own
//! entities with the same systems the server runs, and see everything else interpolated
//! a little in the past. Player actions travel in the input stream with sub-tick
//! timestamps; the server checks hits against the shapes the shooter saw. Events are for
//! one-shot things only.
//!
//! What a game writes:
//!
//! - a [`NetSchema`]: one `replicate::<T>()` line per networked component (precision from
//!   reflection attributes such as `#[reflect(net_quantum = 0.001)]`), one `event::<E>()`
//!   line per event type;
//! - an input type (plain data) and the tick systems, shared by server and client: they
//!   read [`NetInput`] and write the replicated components;
//! - on the server, `spawn((Replicated::new(), ...))` for networked entities and
//!   [`Server::set_owner`] for a player's pawn;
//! - the loops: [`Server::update`] → simulate → [`Server::send`] per tick, and
//!   [`Client::frame`] per rendered frame.
//!
//! Singleplayer creates none of this (ADR 0019 addendum: no loopback). A listen server
//! is a [`Server`] in the host's process; the host's own pawn is fed its input directly.
//!
//! The wire format (ADR 0074): snapshots are bit-packed, each entity's state is a delta
//! against the newest state of it the client acknowledged (Quake III), chosen under a byte
//! budget by a priority accumulator (Tribes, Fiedler), and resent only after a loss
//! notification (Tribes) or a newer change.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod bits;
mod client;
mod clock;
mod lagcomp;
mod schema;
mod server;
mod wire;

pub use client::{Client, ClientEvent, ClientSettings, ClientStats, FrameInfo};
pub use clock::SubTick;
pub use lagcomp::{HitCapsule, LagCompensation, LagHit};
pub use schema::{FieldHints, MAX_COMPONENTS, NetSchema};
pub use server::{ClientId, Server, ServerEvent, ServerSettings, ServerStats, Target};

use engine_ecs::{Component, bytemuck, pod_component};

/// Bumped when the snapshot, input or event format changes (part of the schema hash).
pub const WIRE_VERSION: u64 = 1;

/// Marks an entity as networked. On the server, spawn it with [`Replicated::new`]; the
/// server assigns the network id on its next [`Server::send`]. On a client, replicated
/// entities carry it with the server's id.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Replicated {
    /// Network slot + 1 (0: not assigned yet).
    pub(crate) id: u32,
    /// Generation of the slot.
    pub(crate) generation: u16,
    /// Owner: 0 for the server (or the listen server's host), client index + 1 otherwise.
    pub(crate) owner: u16,
}
pod_component!(Replicated);

impl Replicated {
    /// A networked entity owned by the server.
    pub const fn new() -> Self {
        Self {
            id: 0,
            generation: 0,
            owner: 0,
        }
    }

    /// The network id, once assigned (the same on server and clients).
    pub fn net_id(self) -> Option<NetId> {
        (self.id != 0).then_some(NetId {
            slot: self.id - 1,
            generation: self.generation,
        })
    }

    /// The owning client, if a client owns it.
    pub fn owner(self) -> Option<ClientId> {
        self.owner.checked_sub(1).map(ClientId)
    }
}

/// A network id: the same entity on the server and every client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NetId {
    /// Slot (reused after despawn).
    pub slot: u32,
    /// Reuse counter of the slot.
    pub generation: u16,
}

/// On a client: this entity is predicted (the client owns it). Its replicated components
/// are the client's prediction; the server's state corrects them by rollback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Predicted;
pod_component!(Predicted);

/// The input of the player that controls an entity, for the tick being simulated.
/// Present on every owned pawn on the server and on the client's own pawns, so systems
/// that query it run for exactly the entities this side simulates.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NetInput<I> {
    /// The game's input.
    pub input: I,
    /// When and what the player saw (lag compensation).
    pub header: InputHeader,
}

impl<I: Send + Sync + 'static> Component for NetInput<I> {}

/// Timing of one input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputHeader {
    /// The tick the input is for.
    pub tick: u32,
    /// How far in the past remote entities were shown when the input was sampled, in
    /// 1/256 ticks (the client's interpolation delay plus its lead).
    pub view_delay: u16,
    /// Whether the input arrived (false: the server repeated the previous one).
    pub fresh: bool,
}

impl InputHeader {
    /// The server time (in ticks) the player saw at `subtick` within this input's tick:
    /// what lag compensation rewinds hit shapes to.
    pub fn view_time(&self, subtick: SubTick) -> f64 {
        // The input for tick T covers the interval that ends when T is simulated; an
        // action at fraction f happened (1 - f) ticks before that point.
        f64::from(self.tick)
            - f64::from(self.view_delay) / 256.0
            - (1.0 - f64::from(subtick.fraction()))
    }
}

/// Optional interest management: the entity is replicated to a client only within
/// `radius` of the client's view position ([`Server::set_view`]), and its priority falls
/// with distance. Without it an entity is relevant to every client.
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct NetRelevance {
    /// Radius (m); 0: everywhere.
    pub radius: f32,
    /// Priority scale (1: normal).
    pub priority: f32,
}
pod_component!(NetRelevance);

impl Default for NetRelevance {
    fn default() -> Self {
        Self {
            radius: 0.0,
            priority: 1.0,
        }
    }
}

/// Plain-data input types ([`NetInput`]).
pub trait InputData: bytemuck::Pod + Default + PartialEq + Send + Sync + 'static {}
impl<T: bytemuck::Pod + Default + PartialEq + Send + Sync + 'static> InputData for T {}

/// Encoding internals for the variant measurements of ADR 0074 (`examples/replication_bench.rs`).
/// Not a stable API.
#[doc(hidden)]
pub mod analysis {
    use crate::NetSchema;
    use crate::bits::BitWriter;

    /// Words of component `index`.
    pub fn words(schema: &NetSchema, index: usize) -> usize {
        schema.components[index].words
    }

    /// Quantizes component `index` from its bytes.
    pub fn quantize(schema: &NetSchema, index: usize, bytes: &[u8], out: &mut [u32]) {
        schema.components[index].quantize(bytes, out);
    }

    /// Bits of a whole state (mask + every word).
    pub fn bits_full(schema: &NetSchema, mask: u64, cur: &[u32], w: &mut BitWriter) -> usize {
        w.reset(1 << 16);
        crate::server::encode_full(schema, mask, cur, w);
        w.bits()
    }

    /// Bits of a delta against `base`.
    pub fn bits_delta(
        schema: &NetSchema,
        mask: u64,
        cur: &[u32],
        base: &[u32],
        w: &mut BitWriter,
    ) -> usize {
        w.reset(1 << 16);
        crate::server::encode_delta(schema, mask, cur, base, w);
        w.bits()
    }
}
