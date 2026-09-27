//! Asset import (CLAUDE.md §13, ADR 0033). glTF 2.0 into memory and into the ECS;
//! PNG and JPEG textures decoded and given mip chains filtered for their use (ADR 0040).
//! Cooking to a runtime format, the derived data cache and streaming come later; the
//! on-disk cooked format is a team decision.

mod gltf_import;
mod texture_import;

pub use gltf_import::{
    AlphaMode, ImportError, ImportedScene, MaterialData, MeshData, MeshInstance, NodeData,
    Primitive, TextureData, TextureSource, TextureWrap, import_gltf, read_texture, spawn_scene,
};
pub use texture_import::{Image, ImageError, TextureImage, TextureUsage, build_mips, decode_image};
