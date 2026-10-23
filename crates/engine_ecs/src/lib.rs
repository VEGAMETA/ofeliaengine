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
//! - **Hierarchy** ([`Parent`], [`Children`]): intrusive, plain-data sibling lists, so
//!   parent links are snapshotted and hashed like other components.
//! - **Determinism** (ADR 0019): the same calls give the same entities, rows, iteration
//!   order and [`World::state_hash`]. Iteration order is canonical (by component names),
//!   so it does not depend on the history of which archetypes were created first, and a
//!   [`Snapshot`] restores the exact state for rollback.

mod bundle;
mod commands;
mod component;
mod entity;
mod fxhash;
mod hierarchy;
mod query;
pub mod reflected;
mod resource;
mod schedule;
mod snapshot;
mod storage;
mod transform;
mod world;

pub use bundle::{Bundle, MAX_BUNDLE};
pub use bytemuck;
pub use commands::{Commands, PendingEntity, Target};
pub use component::{
    Component, ComponentDesc, ComponentId, Components, DropGlue, MAX_COMPONENT_ALIGN, Pod,
    RegisterError,
};
/// Reflection (`engine_reflect`), re-exported for [`reflect_component!`].
pub use engine_reflect as reflect;
pub use entity::Entity;
pub use hierarchy::{ChildIter, Children, HierarchyError, Parent};
#[doc(hidden)]
pub use linkme;
pub use query::{Changed, Fetch, Filter, Or, QueryIter, QueryState, ReadOnlyFetch, With, Without};
pub use reflected::{
    DefaultValue, FieldMove, ReflectedKind, ReflectedType, Relayout, RuntimeType, TextField,
    add_runtime_type, find_reflected, reflected_types, runtime_types, runtime_types_version,
};
pub use schedule::{
    Access, FnSystem, Res, ResMut, Schedule, System, SystemContext, SystemQuery, system,
};
pub use snapshot::{Snapshot, SnapshotError};
pub use storage::{Archetype, CHUNK_CLASSES};
pub use transform::{GlobalTransform, PropagationStats, Transform, TransformPropagation};
pub use world::World;

#[cfg(test)]
mod tests;
