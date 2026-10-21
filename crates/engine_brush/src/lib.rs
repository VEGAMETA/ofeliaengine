//! Blockout brushes (ADR 0110): convex brushes held as integer planes, their CSG in
//! Godot's order, and what a game needs from them.
//!
//! - [`exact`]: integer planes and the points where three meet, every side exact in
//!   `i128` (no epsilon decides anything);
//! - [`polygon`]: a group's planes, each once, and convex polygons bounded by them;
//! - [`solid`]: convex solids split by planes, one cut out of another, one clipped to
//!   another;
//! - [`csg`]: a group of brushes evaluated in order into convex pieces (the colliders)
//!   and the surface left (the meshes, the navigation's input);
//! - [`group`]: a group kept evaluated through edits, each redoing only what it touches;
//! - [`shape`]: the shapes brushes start as (box, wedge, stairs, cylinder, sphere);
//! - [`mesh`]: triangles of a brush's polygons, grown so T-junctions show no cracks.
//!
//! Written for this engine after the sources in `docs/research/blockout.md`; TrenchBroom
//! and ericw-tools (GPL) were read for ideas only, and no code was carried over.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod csg;
pub mod exact;
pub mod group;
pub mod mesh;
pub mod polygon;
pub mod shape;
pub mod solid;

pub use csg::{Brush, Evaluated, Op, Piece, evaluate};
pub use exact::{Plane, Point};
pub use group::{Group, Redone};
pub use mesh::{Triangles, triangles};
pub use polygon::{PlaneId, Planes, Polygon};
pub use shape::Shape;
pub use solid::{Owner, Solid, SolidError};
