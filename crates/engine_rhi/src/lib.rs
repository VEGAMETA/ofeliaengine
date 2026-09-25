//! Thin Vulkan 1.3 layer (CLAUDE.md §6): not a cross-API abstraction, just the engine's
//! way of creating and owning Vulkan objects.
//!
//! - [`Instance`]: loads the Vulkan loader, creates the instance, and optionally enables
//!   the validation layer with a messenger that counts errors (tests assert zero).
//! - [`Adapter`]: a physical device with its properties, the required-feature check
//!   ([`Adapter::missing_features`]) and optional capabilities.
//! - [`Device`]: the logical device with the engine's required feature set and its
//!   queues (graphics, async compute, transfer).
//!
//! The minimum GPU tier is Vulkan 1.3 with dynamic rendering, synchronization2, timeline
//! semaphores, buffer device address and bindless descriptor indexing (ADR 0006).

mod adapter;
mod device;
mod error;
mod instance;

pub use adapter::{Adapter, AdapterKind, AdapterPreference, OptionalCaps};
pub use ash;
pub use device::{Device, Queue};
pub use error::RhiError;
pub use instance::{Instance, InstanceDesc, ValidationReport};
