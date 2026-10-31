//! A build's derived data (ADR 0126's G3a): what the player would compute from the game's
//! assets on every run without the editor's cache, computed at build time and carried in
//! `data/derived.pack` (`engine_assets::DerivedDataCache::open_pack`).
//!
//! - **Textures:** every image the build's models, material files and sprites use, for the
//!   use the player makes of it (a base color with its alpha cutoff, data, a normal map),
//!   block-compressed with its mip chain as the player's loader compresses it (BC7, BC5 for
//!   normal maps); an image whose `.meta` keeps it uncompressed is left out, as the player
//!   leaves it.
//! - **Colliders:** every model's meshes cooked by its colliders' rules, as the player cooks
//!   them when a model is asked for its collider (convex parts take seconds).
//!
//! The imports run through the editor's own cache (`.ofelia/ddc`), so what the editor has
//! made is not made again; the entries they read or stored are packed. A pack holding the
//! same entries (their keys hash their sources and settings) is kept.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use engine_assets::colliders::{ColliderRules, cook_node_colliders};
use engine_assets::material_file::{MaterialFile, ParamValue, TextureEncoding};
use engine_assets::{DerivedDataCache, TextureSource, TextureUsage, import_gltf, import_texture};
use engine_core::jobs::ThreadPool;
use engine_scene::{AssetMeta, TextureCompression};

use crate::{BuildError, Plan, extension};

/// The editor's derived data cache under a game's folder.
pub const EDITOR_CACHE: &str = ".ofelia/ddc";

/// What went into a derived data pack.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Derived {
    /// Textures (an image for one use).
    pub textures: usize,
    /// Models whose colliders were cooked.
    pub models: usize,
    /// Entries packed.
    pub entries: usize,
    /// The pack's bytes.
    pub bytes: u64,
    /// Whether the pack was written (not kept).
    pub written: bool,
    /// Seconds it took.
    pub seconds: f64,
}

/// Whether `path` names an image file the player decodes.
fn is_image(path: &str) -> bool {
    matches!(extension(path).as_str(), "png" | "jpg" | "jpeg")
}

/// Whether an image's settings keep it uncompressed.
fn uncompressed(image: &Path) -> bool {
    AssetMeta::read(image).texture.compression == TextureCompression::Uncompressed
}

/// The derived data of the plan's assets (of the game at `root`) packed at `pack`, through
/// the cache in `cache` (the editor's, [`EDITOR_CACHE`]); work on `jobs` when given.
///
/// # Errors
///
/// A model or an image that cannot be read or decoded (named), a material file that does
/// not resolve, or the pack not written.
pub fn derived_pack(
    root: &Path,
    plan: &Plan,
    cache: &Path,
    pack: &Path,
    jobs: Option<&ThreadPool>,
) -> Result<Derived, BuildError> {
    let cache = DerivedDataCache::new(cache)
        .map_err(|e| BuildError::new(format!("{}: {e}", cache.display())))?
        .recording();
    let started = std::time::Instant::now();
    let mut derived = Derived::default();
    let mut done: Vec<(PathBuf, TextureUsage)> = Vec::new();
    let mut image = |path: &Path, usage: TextureUsage, derived: &mut Derived| {
        if uncompressed(path) || done.contains(&(path.to_owned(), usage)) {
            return Ok(());
        }
        done.push((path.to_owned(), usage));
        let bytes =
            std::fs::read(path).map_err(|e| BuildError::new(format!("{}: {e}", path.display())))?;
        import_texture(&bytes, usage, Some(&cache), jobs)
            .map_err(|e| BuildError::new(format!("{}: {e}", path.display())))?;
        derived.textures += 1;
        Ok::<(), BuildError>(())
    };
    // The models' textures and colliders; the images they name are theirs.
    let mut used_images = BTreeSet::new();
    for file in plan
        .files
        .iter()
        .filter(|f| matches!(extension(f).as_str(), "gltf" | "glb"))
    {
        let full = root.join(file);
        let failed = |e: String| BuildError::new(format!("{file}: {e}"));
        let bytes = std::fs::read(&full).map_err(|e| failed(e.to_string()))?;
        let dir = full.parent().map(Path::to_path_buf).unwrap_or_default();
        let scene = import_gltf(&bytes, Some(&dir)).map_err(|e| failed(e.to_string()))?;
        let meta = AssetMeta::read(&full);
        for (texture, usage) in engine_assets::cook::texture_uses(&scene) {
            let Some(data) = scene.textures.get(texture) else {
                continue;
            };
            match &data.source {
                TextureSource::Uri(uri) => {
                    let path = dir.join(uri);
                    used_images.insert(path.clone());
                    image(&path, usage, &mut derived)?;
                }
                TextureSource::Embedded { bytes, .. } => {
                    if meta.texture.compression == TextureCompression::Uncompressed {
                        continue;
                    }
                    import_texture(bytes, usage, Some(&cache), jobs)
                        .map_err(|e| failed(e.to_string()))?;
                    derived.textures += 1;
                }
            }
        }
        let rules = ColliderRules::for_scene(&full).map_err(|e| failed(e.to_string()))?;
        cook_node_colliders(&scene, &[], &rules, Some(&cache), jobs);
        derived.models += 1;
    }
    // Material files' texture parameters, for their encodings.
    for file in plan
        .files
        .iter()
        .filter(|f| extension(f) == engine_assets::material_file::MATERIAL_FILE_EXTENSION)
    {
        let failed = |message: String| BuildError::new(format!("{file}: {message}"));
        let resolved =
            MaterialFile::load_with_bases(&root.join(file), root).map_err(|e| failed(e.message))?;
        let Some(typed) = resolved
            .file
            .typed(&resolved.report.assets, root)
            .map_err(|e| failed(e.message))?
        else {
            continue;
        };
        for value in typed.params.values() {
            if let ParamValue::Texture(path, encoding) = value {
                let usage = match encoding {
                    TextureEncoding::Color => TextureUsage::Color,
                    TextureEncoding::Data => TextureUsage::Data,
                    TextureEncoding::Normal => TextureUsage::Normal,
                };
                used_images.insert(path.clone());
                image(path, usage, &mut derived)?;
            }
        }
    }
    // The other images (sprites): drawn as color.
    for file in plan.files.iter().filter(|f| is_image(f)) {
        let path = root.join(file);
        if !used_images.contains(&path) {
            image(&path, TextureUsage::Color, &mut derived)?;
        }
    }
    let entries = cache.used();
    derived.entries = entries.len();
    if DerivedDataCache::pack_entries(pack).as_ref() == Some(&entries) {
        derived.bytes = std::fs::metadata(pack).map_or(0, |m| m.len());
        derived.seconds = started.elapsed().as_secs_f64();
        return Ok(derived);
    }
    derived.bytes = cache
        .write_pack(&entries, pack)
        .map_err(|e| BuildError::new(format!("{}: {e}", pack.display())))?;
    derived.written = true;
    derived.seconds = started.elapsed().as_secs_f64();
    Ok(derived)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{project, write};

    fn png(width: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, width);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let data: Vec<u8> = (0..width * width)
            .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
            .collect();
        writer.write_image_data(&data).unwrap();
        drop(writer);
        bytes
    }

    /// A sprite's image, one kept uncompressed, and a material type's texture parameter:
    /// each compressed once for its use into the pack, which the player's lookup finds;
    /// the pack kept when nothing changed.
    #[test]
    fn images_are_compressed_into_the_pack_for_their_uses() {
        let root = project("derived");
        std::fs::create_dir_all(root.join("sprites")).unwrap();
        std::fs::write(root.join("sprites/hero.png"), png(8, [200, 30, 20])).unwrap();
        std::fs::write(root.join("sprites/pixel.png"), png(8, [20, 30, 200])).unwrap();
        write(
            &root,
            "sprites/pixel.png.meta",
            "[asset version=1 id=00000000000000000000000000000001]\n\n\
             texture.compression = Uncompressed\n",
        );
        std::fs::write(root.join("sprites/bumps.png"), png(8, [128, 128, 255])).unwrap();
        write(&root, "materials/bumpy.slang", "// a type");
        write(
            &root,
            "materials/bumpy.material",
            "[material version=2 id=00000000000000000000000000000002]\n\n\
             shader = asset(\"materials/bumpy.slang\", 00000000000000000000000000000003)\n\
             type = \"Bumpy\"\n\
             textures[\"bumps\"].image = asset(\"sprites/bumps.png\", 00000000000000000000000000000004)\n\
             textures[\"bumps\"].encoding = Normal\n",
        );
        let mut plan = Plan::default();
        for f in [
            "sprites/hero.png",
            "sprites/pixel.png",
            "sprites/bumps.png",
            "materials/bumpy.slang",
            "materials/bumpy.material",
        ] {
            plan.files.insert(String::from(f));
        }
        let cache = root.join(EDITOR_CACHE);
        let pack = root.join("out/derived.pack");
        let derived = derived_pack(&root, &plan, &cache, &pack, None).unwrap();
        assert_eq!(
            (derived.textures, derived.entries, derived.written),
            (2, 2, true),
            "the hero as color, the bumps as a normal map; the pixel art left out"
        );
        let read = DerivedDataCache::open_pack(&pack).unwrap();
        assert!(read.read_only());
        let hero = std::fs::read(root.join("sprites/hero.png")).unwrap();
        let bumps = std::fs::read(root.join("sprites/bumps.png")).unwrap();
        assert!(engine_assets::cached_texture(&read, &hero, TextureUsage::Color).is_some());
        assert!(engine_assets::cached_texture(&read, &bumps, TextureUsage::Normal).is_some());
        assert!(engine_assets::cached_texture(&read, &bumps, TextureUsage::Color).is_none());
        assert!(read.put(1, "texture", b"no").is_err(), "read-only");
        // Again: every entry from the editor's cache, the pack kept.
        let again = derived_pack(&root, &plan, &cache, &pack, None).unwrap();
        assert_eq!((again.entries, again.written), (2, false));
        let _ = std::fs::remove_dir_all(&root);
    }
}
