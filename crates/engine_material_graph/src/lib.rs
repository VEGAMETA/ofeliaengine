//! Material graphs (ADR 0123): a material type drawn as nodes and links, generated into
//! one Slang module that the engine compiles as it compiles a hand-written type (ADR 0092).
//!
//! - [`library`]: the node kinds, each a snippet of Slang with typed ports;
//! - [`graph`]: a graph in memory: its shading model, its nodes by id, its links by the
//!   input they feed;
//! - [`generate`]: the module a graph makes, each line with the node that made it;
//! - [`file`]: the `.mgraph` file in the editor's grammar.
//!
//! ```
//! use engine_material_graph::{Build, Graph, Model, Node, PortRef, generate, graph::OUTPUT};
//!
//! let mut moss = Graph::new(Model::Lit);
//! moss.add("t", Node::new("texture", [0.0, 0.0]).named("albedo"))
//!     .link(PortRef::new("t", "rgba"), PortRef::new(OUTPUT, "base"));
//! let module = generate(&moss, "Moss", Build::Saved).unwrap();
//! assert!(module.text.contains("struct Moss : IMaterial"));
//! ```
//!
//! Written for this engine after `docs/research/material-graph.md`; Godot, three.js and
//! Material Maker (MIT) were read for ideas only, and no code was carried over.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod file;
pub mod generate;
pub mod graph;
pub mod library;

pub use file::GraphFile;
pub use generate::{Build, Generated, GraphError, Param, ParamType, generate};
pub use graph::{Graph, Model, Node, PortRef};
pub use library::{Kind, PortType, kind};
