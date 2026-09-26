//! Glyph atlas: rendered glyphs packed into one 8-bit coverage texture.
//!
//! Glyphs are keyed by font, glyph, size (1/64 px) and subpixel x offset (quarter pixels),
//! rendered on first use and packed on shelves. The atlas records the rectangle changed
//! since the last upload. When it is full it is cleared and repacked from what the
//! current frame asks for; its generation changes so renderers re-upload everything.

use crate::font::{FontId, Fonts};
use crate::raster::{GlyphBitmap, Rasterizer, rasterize};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// Multiply-rotate hash for small fixed keys (FxHash's scheme); SipHash is several times
/// slower here and the keys are not attacker-controlled.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }
    fn write_u8(&mut self, v: u8) {
        self.write_u64(u64::from(v));
    }
    fn write_u16(&mut self, v: u16) {
        self.write_u64(u64::from(v));
    }
    fn write_u32(&mut self, v: u32) {
        self.write_u64(u64::from(v));
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

type KeyMap<V> = HashMap<GlyphKey, V, BuildHasherDefault<KeyHasher>>;

/// Subpixel x positions per pixel.
pub const SUBPIXEL_STEPS: u8 = 4;
/// Empty pixels around each glyph, so bilinear sampling never bleeds.
const PADDING: u32 = 1;

/// What identifies a rendered glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    /// Font.
    pub font: FontId,
    /// Glyph id.
    pub glyph: u16,
    /// Size in 1/64 pixels.
    pub size_64: u32,
    /// Subpixel x offset in 1/[`SUBPIXEL_STEPS`] pixels.
    pub subpixel: u8,
}

impl GlyphKey {
    /// The key for drawing `glyph` at pen x position `x` (fractional) and `size` pixels;
    /// also returns the whole-pixel x to draw at.
    pub fn at(font: FontId, glyph: u16, size: f32, x: f32) -> (Self, i32) {
        let whole = x.floor();
        let steps = f32::from(SUBPIXEL_STEPS);
        let mut sub = ((x - whole) * steps).round() as i32;
        let mut whole = whole as i32;
        if sub == i32::from(SUBPIXEL_STEPS) {
            sub = 0;
            whole += 1;
        }
        (
            Self {
                font,
                glyph,
                size_64: (size * 64.0).round().max(1.0) as u32,
                subpixel: sub as u8,
            },
            whole,
        )
    }
}

/// Where a glyph is in the atlas, and how to place it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasGlyph {
    /// Left edge in the atlas, pixels.
    pub x: u16,
    /// Top edge in the atlas, pixels.
    pub y: u16,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// Offset from the whole-pixel pen x to the bitmap's left edge.
    pub left: i16,
    /// Offset from the baseline up to the bitmap's top edge.
    pub top: i16,
}

/// A rectangle of the atlas, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left.
    pub x: u32,
    /// Top.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

#[derive(Debug, Clone, Copy)]
struct Shelf {
    y: u32,
    height: u32,
    x: u32,
}

/// An 8-bit coverage atlas of rendered glyphs.
#[derive(Debug)]
pub struct GlyphAtlas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    shelves: Vec<Shelf>,
    /// `None`: the glyph renders to nothing (a space).
    map: KeyMap<Option<AtlasGlyph>>,
    dirty: Option<(u32, u32, u32, u32)>,
    generation: u64,
    raster: Rasterizer,
    bitmap: GlyphBitmap,
}

impl GlyphAtlas {
    /// A `width × height` atlas (each at most 65,535).
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width.clamp(16, 65_535), height.clamp(16, 65_535));
        Self {
            width,
            height,
            pixels: vec![0; (width * height) as usize],
            shelves: Vec::new(),
            map: KeyMap::default(),
            dirty: None,
            generation: 0,
            raster: Rasterizer::new(),
            bitmap: GlyphBitmap::default(),
        }
    }

    /// Atlas size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Coverage pixels, row-major.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Increments whenever the atlas was cleared: every glyph moved, upload all of it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Glyphs cached (including empty ones).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The rectangle changed since the last call, if any.
    pub fn take_dirty(&mut self) -> Option<Rect> {
        self.dirty.take().map(|(x0, y0, x1, y1)| Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }

    /// Clears the atlas (all glyphs are re-rendered on next use).
    pub fn clear(&mut self) {
        self.pixels.fill(0);
        self.shelves.clear();
        self.map.clear();
        self.generation += 1;
        self.dirty = Some((0, 0, self.width, self.height));
    }

    /// The glyph for `key`, rendering and packing it on first use. `None` for glyphs with
    /// no pixels, unknown fonts, and glyphs larger than the atlas.
    pub fn get(&mut self, fonts: &Fonts, key: GlyphKey) -> Option<AtlasGlyph> {
        if let Some(&cached) = self.map.get(&key) {
            return cached;
        }
        let font = fonts.get(key.font)?;
        let size = key.size_64 as f32 / 64.0;
        let sub = f32::from(key.subpixel) / f32::from(SUBPIXEL_STEPS);
        let rendered = rasterize(
            font,
            key.glyph,
            size,
            sub,
            &mut self.raster,
            &mut self.bitmap,
        );
        let placed = match rendered {
            None => None,
            Some(()) => {
                let (w, h) = (self.bitmap.width, self.bitmap.height);
                let spot = match self.allocate(w, h) {
                    Some(spot) => Some(spot),
                    None => {
                        self.clear();
                        self.allocate(w, h)
                    }
                };
                match spot {
                    None => None,
                    Some((x, y)) => {
                        for row in 0..h {
                            let src = (row * w) as usize;
                            let dst = ((y + row) * self.width + x) as usize;
                            self.pixels[dst..dst + w as usize]
                                .copy_from_slice(&self.bitmap.coverage[src..src + w as usize]);
                        }
                        self.mark(x, y, w, h);
                        Some(AtlasGlyph {
                            x: x as u16,
                            y: y as u16,
                            width: w as u16,
                            height: h as u16,
                            left: self.bitmap.left as i16,
                            top: self.bitmap.top as i16,
                        })
                    }
                }
            }
        };
        self.map.insert(key, placed);
        placed
    }

    fn mark(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let (x1, y1) = (x + w, y + h);
        self.dirty = Some(match self.dirty {
            None => (x, y, x1, y1),
            Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x1), d.max(y1)),
        });
    }

    /// Finds space on a shelf of similar height (within 25 %), or opens a new shelf.
    fn allocate(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (pw, ph) = (w + PADDING, h + PADDING);
        if pw > self.width || ph > self.height {
            return None;
        }
        let mut best: Option<usize> = None;
        for (i, s) in self.shelves.iter().enumerate() {
            if s.height >= ph && s.height <= ph + ph / 4 + 2 && s.x + pw <= self.width {
                let better = best.is_none_or(|b| self.shelves[b].height > s.height);
                if better {
                    best = Some(i);
                }
            }
        }
        if let Some(i) = best {
            let s = &mut self.shelves[i];
            let at = (s.x, s.y);
            s.x += pw;
            return Some(at);
        }
        let y = self.shelves.last().map_or(0, |s| s.y + s.height);
        if y + ph > self.height {
            return None;
        }
        self.shelves.push(Shelf {
            y,
            height: ph,
            x: pw,
        });
        Some((0, y))
    }
}
