//! Entity component system (ADR 0017).
//!
//! - **Storage:** archetypes (one per set of component types) stored in chunks that grow
//!   from 16 to 256 KiB, structure of arrays inside each chunk. Chunks come from a
//!   per-world pool, so spawning and despawning in steady state do not allocate.
//! - **Components** are Rust types ([`Component`]) or run-time descriptors
//!   ([`World::register_dynamic`], for scripts). Plain-data components ([`Pod`]) can be
//!   snapshotted, restored and hashed byte-wise.
//! - **Queries** ([`QueryState`]) cache their matching archetypes, iterate chunk by chunk
//!   (per entity, per chunk array, or in parallel on the job system) and track writes per
//!   chunk and component for [`Changed`] filters.
//! - **Commands** ([`Commands`]) defer structural changes and apply them in recording
//!   order.
//! - **Determinism** (ADR 0019): the same calls give the same entities, rows, iteration
//!   order and [`World::state_hash`]. Iteration order is canonical (by component names),
//!   so it does not depend on the history of which archetypes were created first, and a
//!   [`Snapshot`] restores the exact state for rollback.

mod bundle;
mod commands;
mod component;
mod entity;
mod fxhash;
mod query;
mod snapshot;
mod storage;
mod world;

pub use bundle::{Bundle, MAX_BUNDLE};
pub use bytemuck;
pub use commands::{Commands, PendingEntity, Target};
pub use component::{
    Component, ComponentDesc, ComponentId, Components, MAX_COMPONENT_ALIGN, Pod, RegisterError,
};
pub use entity::Entity;
pub use query::{Changed, Fetch, Filter, QueryIter, QueryState, ReadOnlyFetch, With, Without};
pub use snapshot::{Snapshot, SnapshotError};
pub use storage::{Archetype, CHUNK_CLASSES};
pub use world::World;

#[cfg(test)]
mod tests;
