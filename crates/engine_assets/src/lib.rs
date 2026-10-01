//! Asset import (CLAUDE.md §13, ADR 0033). glTF 2.0 into memory and into the ECS;
//! PNG and JPEG textures decoded and given mip chains filtered for their use (ADR 0040),
//! block-compressed (BC7, BC5) and kept in a derived data cache (ADR 0041). Meshes get
//! detail levels from meshoptimizer (ADR 0053). Skins and animations become `engine_anim`
//! skeletons and compressed clips (`animation`, ADR 0068). Scenes are cooked into packs, the runtime
//! format, and loaded through an I/O ring with zstd per entry (`cook`, `pack`; ADR 0056).

pub mod animation;
pub mod cook;
mod ddc;
mod gltf_import;
mod lod;
pub mod pack;
mod texture_compress;
mod texture_import;

pub use cook::{
    CookError, CookSettings, CookStats, CookedInstance, CookedMaterial, CookedMesh, CookedScene,
    CookedSkin, SceneLayout, TextureEntry, TextureSlot, cook_scene, load_pack, open_scene,
    write_pack,
};
pub use ddc::{
    CacheOutcome, DerivedDataCache, import_texture, texture_from_bytes, texture_key,
    texture_to_bytes,
};
pub use gltf_import::{
    AlphaMode, AnimationData, ChannelData, ChannelKeys, ImportError, ImportedScene, MaterialData,
    MeshData, MeshInstance, NodeData, Primitive, SkinData, TextureData, TextureSource, TextureWrap,
    import_gltf, read_texture, spawn_scene,
};
pub use lod::{Lod, LodSettings, generate_lods};
pub use texture_compress::{ENCODER, EncodedTexture, TextureFormat, compress, compressed_format};
pub use texture_import::{Image, ImageError, TextureImage, TextureUsage, build_mips, decode_image};
