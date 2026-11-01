//! Asset import (CLAUDE.md §13, ADR 0033). glTF 2.0 into memory and into the ECS;
//! PNG and JPEG textures decoded and given mip chains filtered for their use (ADR 0040),
//! block-compressed (BC7, BC5) and kept in a derived data cache (ADR 0041). Meshes get
//! detail levels and meshlets from meshoptimizer (ADRs 0053, 0085; `meshlet`). Skins and animations become `engine_anim`
//! skeletons and compressed clips (`animation`, ADR 0068). Scenes are cooked into packs, the runtime
//! format, and loaded through an I/O ring with zstd per entry (`cook`, `pack`; ADR 0056).
//! HDR panoramas (`.hdr`) are read, and their sun found, for HDRI environments (`hdr`, ADR
//! 0083), and cooked into BC6H cube maps for the derived data cache and packs
//! (`panorama`). Material files (`.material`, ADR 0120) set a material type's parameters and
//! replace a scene's glTF materials (`material_file`, ADR 0092); glTF's unlit, clear coat
//! and sheen become materials of the engine's built-in types (`gltf_models`); PSO records
//! (`pso_record`) let a cook compile only the entry points a game draws. A glTF's outline
//! (its nodes, meshes and materials, from its JSON alone) is what the editor makes a
//! dropped model's entities from ([`GltfOutline`]).

pub mod animation;
pub mod aseprite;
pub mod colliders;
pub mod cook;
mod ddc;
mod gltf_import;
pub mod gltf_models;
mod gltf_outline;
pub mod gltf_physics;
pub mod graph_module;
pub mod hdr;
mod lod;
pub mod material_file;
pub mod material_pack;
pub mod meshlet;
pub mod navigation;
pub mod pack;
pub mod panorama;
pub mod pso_record;
pub mod slicing;
pub mod sounds;
pub mod strings;
mod texture_compress;
mod texture_import;

pub use cook::{
    CookError, CookSettings, CookStats, CookedInstance, CookedMaterial, CookedMesh, CookedScene,
    CookedSkin, CookedTypedMaterial, MeshLevelData, MeshLevelRecord, MeshRecord, SceneLayout,
    StreamedMesh, TextureEntry, TextureSlot, cook_scene, load_pack, open_scene,
    open_scene_streamed, order_for_streaming, read_mesh_level_entry, streamed_vertex_counts,
    write_pack,
};
pub use ddc::{
    CacheOutcome, DERIVED_PACK, DerivedDataCache, EntryName, SOURCE_KIND, cached_texture,
    import_texture, path_under, source_key, texture_from_bytes, texture_key, texture_key_of,
    texture_to_bytes,
};
pub use gltf_import::{
    AlphaMode, AnimationData, ChannelData, ChannelKeys, ClearcoatData, ImportError, ImportedScene,
    MaterialData, MaterialLayers, MaterialTypeData, MeshData, MeshInstance, NodeData, Primitive,
    SheenData, SkinData, TextureData, TextureSource, TextureWrap, TypedMaterialData, import_gltf,
    read_texture, spawn_scene,
};
pub use gltf_outline::{GltfOutline, OutlineMaps, OutlineMesh, OutlineNode, OutlinePrimitive};
pub use lod::{Lod, LodSettings, generate_lods};
pub use texture_compress::{ENCODER, EncodedTexture, TextureFormat, compress, compressed_format};
pub use texture_import::{Image, ImageError, TextureImage, TextureUsage, build_mips, decode_image};
