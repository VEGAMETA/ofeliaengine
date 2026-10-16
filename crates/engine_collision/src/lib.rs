//! Colliders in the engine's own form (ADR 0101).
//!
//! The cook makes colliders from meshes (`engine_assets`), a physics backend bakes them into
//! its own form for the pack, and the editor will edit them. This crate is the part that
//! needs no backend:
//!
//! - [`hull`]: convex hulls (QuickHull), with an optional vertex limit;
//! - [`fit`]: a box, sphere, capsule or cylinder around a hull, and whether one replaces it;
//! - [`shape`]: the collider's data (shapes with stable ids) and what a shape becomes under
//!   any scale;
//! - [`mesh`]: a mesh's volume, whether it is closed, its vertices welded.

pub mod fit;
pub mod hull;
pub mod mesh;
pub mod shape;

pub use fit::Primitive;
pub use hull::{Hull, HullError, HullSettings};
pub use shape::{Collider, ColliderShape, Origin, Shape};
