//! Sprite images' opaque pixels for the 2D view's picking (ADR 0125's E13-1b-6): a click
//! takes the sprite drawn there only where its image shows.
//!
//! The game keeps no image on the CPU, so the editor decodes an image file the first time
//! a click lands on it and keeps one bit a pixel (a 512 × 512 image: 32 KiB), decoded again
//! when the file changes. Images it cannot decode are picked by their rectangles.

use engine_core::math::Vec2;
use engine_scene::{AssetDatabase, AssetId};
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

/// The least alpha that counts as the image being there: about 10 %, so faint glows and
/// shadows around a figure do not take clicks meant for what is behind them.
pub(crate) const OPAQUE_ALPHA: u8 = 26;

/// An image's pixels at least [`OPAQUE_ALPHA`] opaque, one bit each, row by row from the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mask {
    width: u32,
    height: u32,
    bits: Vec<u64>,
}

impl Mask {
    /// The mask of `width` × `height` RGBA8 pixels `rgba`.
    pub(crate) fn of_rgba(width: u32, height: u32, rgba: &[u8]) -> Self {
        let count = width as usize * height as usize;
        let mut bits = vec![0u64; count.div_ceil(64)];
        for (k, pixel) in rgba.chunks_exact(4).take(count).enumerate() {
            if pixel[3] >= OPAQUE_ALPHA {
                bits[k / 64] |= 1 << (k % 64);
            }
        }
        Self {
            width,
            height,
            bits,
        }
    }

    /// Whether the pixel at texture coordinates `uv` (0 to 1 from the top left) is opaque.
    pub(crate) fn at(&self, uv: Vec2) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        let x = ((uv.x * self.width as f32) as u32).min(self.width - 1);
        let y = ((uv.y * self.height as f32) as u32).min(self.height - 1);
        let k = y as usize * self.width as usize + x as usize;
        self.bits[k / 64] & (1 << (k % 64)) != 0
    }
}

/// The masks decoded so far, by image file, each with the modification time it was decoded
/// at (`None` inside: not decodable, picked by its rectangle).
#[derive(Default)]
pub(crate) struct SpriteMasks {
    masks: HashMap<AssetId, (Option<SystemTime>, Option<Mask>)>,
}

impl SpriteMasks {
    /// Whether image `file` (found through `db`) shows at texture coordinates `uv`: its mask
    /// says so, or it has none (not an image the editor reads, or no file).
    pub(crate) fn opaque(&mut self, db: &AssetDatabase, file: AssetId, uv: Vec2) -> bool {
        let Some(entry) = db.get(file) else {
            return true;
        };
        let path = db.root().join(&entry.path);
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let fresh = self.masks.get(&file).is_some_and(|(at, _)| *at == modified);
        if !fresh {
            self.masks.insert(file, (modified, decode(&path)));
        }
        match self.masks.get(&file) {
            Some((_, Some(mask))) => mask.at(uv),
            _ => true,
        }
    }
}

/// The mask of the image file at `path`, or `None` when it cannot be read as one.
fn decode(path: &Path) -> Option<Mask> {
    engine_profiler::zone!("editor::sprite_mask");
    let bytes = std::fs::read(path).ok()?;
    let image = engine_assets::decode_image(&bytes).ok()?;
    Some(Mask::of_rgba(image.width, image.height, &image.rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4 × 2 image whose left half is opaque, its right half faint: the bits follow
    /// alpha at the threshold, and coordinates land on their pixels (edges clamped).
    #[test]
    fn a_mask_follows_alpha_at_its_threshold() {
        let mut rgba = Vec::new();
        for _ in 0..2 {
            for alpha in [255, OPAQUE_ALPHA, OPAQUE_ALPHA - 1, 0] {
                rgba.extend_from_slice(&[9, 9, 9, alpha]);
            }
        }
        let mask = Mask::of_rgba(4, 2, &rgba);
        assert!(mask.at(Vec2::new(0.1, 0.2)));
        assert!(mask.at(Vec2::new(0.3, 0.9)), "at the threshold: opaque");
        assert!(!mask.at(Vec2::new(0.6, 0.2)), "below it: not");
        assert!(!mask.at(Vec2::new(1.0, 1.0)), "the far corner, clamped");
        assert!(mask.at(Vec2::new(0.0, 0.0)));
    }

    fn png(alpha: impl Fn(u32) -> u8) -> Vec<u8> {
        let rgba: Vec<u8> = (0..4 * 4)
            .flat_map(|k| [200, 100, 50, alpha(k % 4)])
            .collect();
        engine_render::png::encode_png(4, 4, engine_render::png::PngFormat::Rgba, &rgba).unwrap()
    }

    /// Through the asset database: the file decoded at the first ask, decoded again when
    /// it changes; a file it does not know is opaque.
    #[test]
    fn masks_come_from_the_files_and_follow_their_changes() {
        let dir = std::env::temp_dir().join(format!("ofelia-sprite-masks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("half.png");
        // The left half opaque, the right half clear.
        std::fs::write(&file, png(|x| if x < 2 { 255 } else { 0 })).unwrap();
        let mut db = AssetDatabase::new(&dir);
        db.scan();
        let id = db.find("half.png").expect("found").id;
        let mut masks = SpriteMasks::default();
        assert!(masks.opaque(&db, id, Vec2::new(0.2, 0.5)));
        assert!(!masks.opaque(&db, id, Vec2::new(0.8, 0.5)));
        // All clear now, written later.
        std::fs::write(&file, png(|_| 0)).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(!masks.opaque(&db, id, Vec2::new(0.2, 0.5)), "decoded again");
        assert!(
            masks.opaque(&db, AssetId::new(), Vec2::ZERO),
            "unknown: opaque"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
