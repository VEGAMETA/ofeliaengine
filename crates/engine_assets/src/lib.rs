//! Asset import (CLAUDE.md §13, ADR 0033). glTF 2.0 into memory and into the ECS;
//! PNG and JPEG textures decoded and given mip chains filtered for their use (ADR 0040),
//! block-compressed (BC7, BC5) and kept in a derived data cache (ADR 0041). Meshes get
//! detail levels from meshoptimizer (ADR 0053). Cooking to a
//! runtime format and streaming come later; the on-disk cooked format is a team decision.

mod ddc;
mod gltf_import;
mod lod;
mod texture_compress;
mod texture_import;

pub use ddc::{
    CacheOutcome, DerivedDataCache, import_texture, texture_from_bytes, texture_key,
    texture_to_bytes,
};
pub use gltf_import::{
    AlphaMode, ImportError, ImportedScene, MaterialData, MeshData, MeshInstance, NodeData,
    Primitive, TextureData, TextureSource, TextureWrap, import_gltf, read_texture, spawn_scene,
};
pub use lod::{Lod, LodSettings, generate_lods};
pub use texture_compress::{ENCODER, EncodedTexture, TextureFormat, compress, compressed_format};
pub use texture_import::{Image, ImageError, TextureImage, TextureUsage, build_mips, decode_image};
