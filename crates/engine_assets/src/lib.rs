//! Asset import (CLAUDE.md §13, ADR 0033). First piece: glTF 2.0 into memory and into
//! the ECS. Cooking to a runtime format, the derived data cache and streaming come later;
//! the on-disk cooked format is a team decision.

mod gltf_import;

pub use gltf_import::{
    AlphaMode, ImportError, ImportedScene, MaterialData, MeshData, MeshInstance, NodeData,
    Primitive, TextureSource, import_gltf, spawn_scene,
};
