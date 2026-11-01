//! Aseprite files (`.aseprite`, `.ase`; ADR 0125's E13-2f), read by a parser of our own
//! written from the published specification (`docs/ase-file-specs.md` of
//! aseprite/aseprite at `3669f477`, read 2026-11-01; no code of Aseprite or of the
//! `asefile` and `aseprite-loader` crates carried over).
//!
//! A file is drawn as a sheet: its frames (the visible layers merged with their blend
//! modes, the W3C compositing modes plus addition, subtract and divide) in a grid of
//! cells, left to right and top to bottom. Its frames become slices (`frame_0`, ...), its
//! tags clips with the frames' durations, its own slices more slices (nine-slice centres
//! as borders, pivots), and a cel's user data lines `event:<name> [data]` events of the
//! clips that show its frame. What the editor adds lives in the file's `.meta`
//! ([`Aseprite::sprite`] merges it).

use crate::texture_import::{Image, ImageError};
use std::path::Path;

use engine_scene::{ClipEvent, SliceImport, SpriteClipImport, SpriteClipMode, SpriteImport};

/// The magic number at bytes 4..6 of every Aseprite file.
const MAGIC: u16 = 0xA5E0;
/// The magic number of each frame's header.
const FRAME_MAGIC: u16 = 0xF1FA;
/// The widest or tallest sheet made (the textures' limit on every GPU we support).
const MAX_SHEET: u32 = 16384;

/// Whether `path` names an Aseprite file (by its extension).
pub fn is_aseprite_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("aseprite") || e.eq_ignore_ascii_case("ase"))
}

/// The sprite settings of the image file at `path` whose `.meta` holds `meta`: an Aseprite
/// file's own merged in ([`Aseprite::sprite`], its pixels left unread), any other image's
/// as they are.
///
/// # Errors
///
/// An Aseprite file that cannot be read.
pub fn sprite_settings(path: &Path, meta: &SpriteImport) -> Result<SpriteImport, ImageError> {
    if !is_aseprite_path(path) {
        return Ok(meta.clone());
    }
    let bytes = std::fs::read(path).map_err(|e| bad(&format!("{}: {e}", path.display())))?;
    Ok(Aseprite::read_layout(&bytes)?.sprite(meta))
}

/// What a `.meta` keeps of `edited`, sprite settings of an Aseprite file whose own are
/// `base` ([`Aseprite::sprite`] of the same settings without slices or clips): its slices
/// whose pivot or border differ from the file's, slices and clips the file does not have,
/// and events the file's clips do not. Merged again they give `edited` back unless it
/// changed what only the file says (a frame's rectangle, a tag's frames or times, a name).
pub fn overrides(
    base: &SpriteImport,
    edited: &SpriteImport,
) -> (Vec<SliceImport>, Vec<SpriteClipImport>) {
    let mut slices = Vec::new();
    for s in &edited.slices {
        match base.slices.iter().find(|b| b.name == s.name) {
            Some(b) if b.pivot == s.pivot && b.border == s.border => {}
            _ => slices.push(s.clone()),
        }
    }
    let mut clips = Vec::new();
    for c in &edited.clips {
        match base.clips.iter().find(|b| b.name == c.name) {
            Some(b) => {
                let events: Vec<ClipEvent> = c
                    .events
                    .iter()
                    .filter(|e| !b.events.contains(e))
                    .cloned()
                    .collect();
                if !events.is_empty() {
                    clips.push(SpriteClipImport {
                        name: c.name.clone(),
                        events,
                        ..SpriteClipImport::default()
                    });
                }
            }
            None => clips.push(c.clone()),
        }
    }
    (slices, clips)
}

/// Whether `bytes` start like an Aseprite file.
pub fn is_aseprite(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && u16::from_le_bytes([bytes[4], bytes[5]]) == MAGIC
}

/// A read Aseprite file: its frames, tags and slices, and (read with
/// [`Aseprite::read`]) what its frames are drawn from.
#[derive(Debug, Clone, Default)]
pub struct Aseprite {
    /// The canvas's width in pixels: each frame's.
    pub width: u32,
    /// The canvas's height in pixels.
    pub height: u32,
    /// Its frames in order.
    pub frames: Vec<AseFrame>,
    /// Its tags: named runs of frames.
    pub tags: Vec<AseTag>,
    /// Its slices: named rectangles.
    pub slices: Vec<AseSlice>,
    depth: u16,
    flags: u32,
    transparent: u8,
    layers: Vec<Layer>,
    cels: Vec<Cel>,
    palette: Vec<[u8; 4]>,
    tilesets: Vec<Tileset>,
}

/// One frame of an Aseprite file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AseFrame {
    /// How long it shows, in milliseconds.
    pub duration: u32,
    /// Its events: its cels' user data lines `event:<name> [data]`, as (name, data).
    pub events: Vec<(String, String)>,
}

/// A tag: a named run of frames.
#[derive(Debug, Clone, PartialEq)]
pub struct AseTag {
    /// Its name.
    pub name: String,
    /// Its first frame.
    pub from: u32,
    /// Its last frame.
    pub to: u32,
    /// Which way it plays.
    pub direction: TagDirection,
    /// How many times it plays: 0 not said (round and round), 1 once.
    pub repeat: u16,
}

/// Which way a tag plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagDirection {
    /// First to last.
    Forward,
    /// Last to first.
    Reverse,
    /// First to last and back.
    PingPong,
    /// Last to first and back.
    PingPongReverse,
}

/// A slice of an Aseprite file: a name and its rectangle from some frame on.
#[derive(Debug, Clone, PartialEq)]
pub struct AseSlice {
    /// Its name.
    pub name: String,
    /// Its keys, each valid from its frame on.
    pub keys: Vec<SliceKey>,
}

/// A slice's rectangle from frame `frame` on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SliceKey {
    /// The first frame it holds for.
    pub frame: u32,
    /// Left, top, width, height in the canvas's pixels.
    pub rect: [i32; 4],
    /// Its nine-slice centre (left, top, width, height within the slice), if it has one.
    pub center: Option<[i32; 4]>,
    /// Its pivot in pixels from the slice's top left, if it has one.
    pub pivot: Option<[i32; 2]>,
}

#[derive(Debug, Clone)]
struct Layer {
    flags: u16,
    kind: u16,
    blend: u16,
    opacity: u8,
    tileset: u32,
    parent: Option<usize>,
}

impl Layer {
    const VISIBLE: u16 = 1;
    const BACKGROUND: u16 = 8;
    const REFERENCE: u16 = 64;
    const GROUP: u16 = 1;
    const TILEMAP: u16 = 2;
}

#[derive(Debug, Clone)]
struct Cel {
    layer: u16,
    frame: u32,
    x: i32,
    y: i32,
    opacity: u8,
    z: i16,
    data: CelData,
}

#[derive(Debug, Clone)]
enum CelData {
    /// Pixels in the file's colour depth, rows from the top.
    Image {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    /// The same layer's cel of that frame.
    Linked(u32),
    /// Tiles of the layer's tileset.
    Tilemap {
        width: u32,
        height: u32,
        tiles: Vec<u32>,
        id: u32,
        flip_x: u32,
        flip_y: u32,
        flip_d: u32,
    },
    /// Read without its pixels ([`Aseprite::read_layout`]).
    Skipped,
}

#[derive(Debug, Clone)]
struct Tileset {
    id: u32,
    flags: u32,
    width: u32,
    height: u32,
    count: u32,
    pixels: Vec<u8>,
}

/// What a user data chunk belongs to: the chunk read before it.
#[derive(Clone, Copy)]
enum Last {
    Other,
    Cel,
    Tags,
}

fn bad(what: &str) -> ImageError {
    ImageError::Decode(format!("Aseprite file: {what}"))
}

/// A cursor over a file's bytes that never reads past them.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ImageError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.bytes.len())
            .ok_or_else(|| bad("cut short"))?;
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn skip(&mut self, n: usize) -> Result<(), ImageError> {
        self.take(n).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8, ImageError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ImageError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn i16(&mut self) -> Result<i16, ImageError> {
        Ok(self.u16()? as i16)
    }

    fn u32(&mut self) -> Result<u32, ImageError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, ImageError> {
        Ok(self.u32()? as i32)
    }

    fn string(&mut self) -> Result<String, ImageError> {
        let n = self.u16()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }

    fn rest(&mut self) -> &'a [u8] {
        let s = &self.bytes[self.at..];
        self.at = self.bytes.len();
        s
    }
}

/// `data` inflated (zlib), expected to be `len` bytes.
fn inflate(data: &[u8], len: usize) -> Result<Vec<u8>, ImageError> {
    let out = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, len)
        .map_err(|e| bad(&format!("compressed pixels: {e:?}")))?;
    if out.len() != len {
        return Err(bad("compressed pixels of the wrong size"));
    }
    Ok(out)
}

impl Aseprite {
    /// Reads everything in `bytes`, its pixels too (to draw [`Self::sheet`]).
    pub fn read(bytes: &[u8]) -> Result<Self, ImageError> {
        Self::parse(bytes, true)
    }

    /// Reads `bytes`' frames, tags and slices, leaving its pixels compressed and unread
    /// (what a sprite's settings need: cheap even for a big file).
    pub fn read_layout(bytes: &[u8]) -> Result<Self, ImageError> {
        Self::parse(bytes, false)
    }

    fn parse(bytes: &[u8], pixels: bool) -> Result<Self, ImageError> {
        engine_profiler::zone!("assets::aseprite::read");
        let mut r = Reader { bytes, at: 0 };
        r.skip(4)?;
        if r.u16()? != MAGIC {
            return Err(bad("not an Aseprite file"));
        }
        let frames = r.u16()?;
        let width = u32::from(r.u16()?);
        let height = u32::from(r.u16()?);
        let depth = r.u16()?;
        if !matches!(depth, 8 | 16 | 32) {
            return Err(bad("unknown colour depth"));
        }
        if width == 0 || height == 0 {
            return Err(bad("an empty canvas"));
        }
        let flags = r.u32()?;
        let speed = u32::from(r.u16()?);
        r.skip(8)?;
        let transparent = r.u8()?;
        r.at = 128;
        if bytes.len() < 128 {
            return Err(bad("cut short"));
        }
        let mut ase = Self {
            width,
            height,
            depth,
            flags,
            transparent,
            ..Self::default()
        };
        let bpp = usize::from(depth / 8);
        let mut new_palette = false;
        let mut groups: Vec<usize> = Vec::new();
        let mut last = Last::Other;
        for f in 0..u32::from(frames) {
            let start = r.at;
            let size = r.u32()? as usize;
            if r.u16()? != FRAME_MAGIC {
                return Err(bad("a frame without its magic number"));
            }
            let old_chunks = r.u16()?;
            let duration = u32::from(r.u16()?);
            r.skip(2)?;
            let new_chunks = r.u32()?;
            let end = start
                .checked_add(size)
                .filter(|&e| e <= bytes.len() && size >= 16)
                .ok_or_else(|| bad("a frame past the file's end"))?;
            let chunks = if new_chunks == 0 {
                u32::from(old_chunks)
            } else {
                new_chunks
            };
            ase.frames.push(AseFrame {
                duration: if duration > 0 { duration } else { speed.max(1) },
                events: Vec::new(),
            });
            for _ in 0..chunks {
                if r.at >= end {
                    break;
                }
                let chunk_start = r.at;
                let chunk_size = r.u32()? as usize;
                let kind = r.u16()?;
                let chunk_end = chunk_start
                    .checked_add(chunk_size)
                    .filter(|&e| e <= end && chunk_size >= 6)
                    .ok_or_else(|| bad("a chunk past its frame's end"))?;
                let mut c = Reader {
                    bytes: &bytes[..chunk_end],
                    at: r.at,
                };
                match kind {
                    0x0004 | 0x0011 if !new_palette => {
                        let packets = c.u16()?;
                        let mut index = 0usize;
                        for _ in 0..packets {
                            index += usize::from(c.u8()?);
                            let n = match c.u8()? {
                                0 => 256,
                                n => usize::from(n),
                            };
                            for _ in 0..n {
                                let rgb = c.take(3)?;
                                let scale = |v: u8| {
                                    if kind == 0x0011 {
                                        (u32::from(v.min(63)) * 255 / 63) as u8
                                    } else {
                                        v
                                    }
                                };
                                if ase.palette.len() <= index {
                                    ase.palette.resize(index + 1, [0; 4]);
                                }
                                ase.palette[index] =
                                    [scale(rgb[0]), scale(rgb[1]), scale(rgb[2]), 255];
                                index += 1;
                            }
                        }
                        last = Last::Other;
                    }
                    0x2004 => {
                        let layer_flags = c.u16()?;
                        let layer_kind = c.u16()?;
                        let level = usize::from(c.u16()?);
                        c.skip(4)?;
                        let blend = c.u16()?;
                        let opacity = c.u8()?;
                        c.skip(3)?;
                        let _name = c.string()?;
                        let tileset = if layer_kind == Layer::TILEMAP {
                            c.u32()?
                        } else {
                            0
                        };
                        groups.truncate(level);
                        let parent = groups.last().copied();
                        if layer_kind == Layer::GROUP {
                            groups.push(ase.layers.len());
                        }
                        ase.layers.push(Layer {
                            flags: layer_flags,
                            kind: layer_kind,
                            blend,
                            opacity,
                            tileset,
                            parent,
                        });
                        last = Last::Other;
                    }
                    0x2005 => {
                        let layer = c.u16()?;
                        let x = i32::from(c.i16()?);
                        let y = i32::from(c.i16()?);
                        let opacity = c.u8()?;
                        let cel_kind = c.u16()?;
                        let z = c.i16()?;
                        c.skip(5)?;
                        let data = match cel_kind {
                            0 | 2 => {
                                let w = u32::from(c.u16()?);
                                let h = u32::from(c.u16()?);
                                let len = (w as usize) * (h as usize) * bpp;
                                if !pixels {
                                    CelData::Skipped
                                } else if cel_kind == 0 {
                                    CelData::Image {
                                        width: w,
                                        height: h,
                                        pixels: c.take(len)?.to_vec(),
                                    }
                                } else {
                                    CelData::Image {
                                        width: w,
                                        height: h,
                                        pixels: inflate(c.rest(), len)?,
                                    }
                                }
                            }
                            1 => CelData::Linked(u32::from(c.u16()?)),
                            3 => {
                                let w = u32::from(c.u16()?);
                                let h = u32::from(c.u16()?);
                                let bits = c.u16()?;
                                let id = c.u32()?;
                                let flip_x = c.u32()?;
                                let flip_y = c.u32()?;
                                let flip_d = c.u32()?;
                                c.skip(10)?;
                                let size = usize::from(bits / 8);
                                if !matches!(size, 1 | 2 | 4) {
                                    return Err(bad("tiles of an unknown size"));
                                }
                                if pixels {
                                    let raw =
                                        inflate(c.rest(), (w as usize) * (h as usize) * size)?;
                                    let tiles = raw
                                        .chunks_exact(size)
                                        .map(|t| {
                                            let mut v = [0u8; 4];
                                            v[..size].copy_from_slice(t);
                                            u32::from_le_bytes(v)
                                        })
                                        .collect();
                                    CelData::Tilemap {
                                        width: w,
                                        height: h,
                                        tiles,
                                        id,
                                        flip_x,
                                        flip_y,
                                        flip_d,
                                    }
                                } else {
                                    CelData::Skipped
                                }
                            }
                            _ => CelData::Skipped,
                        };
                        ase.cels.push(Cel {
                            layer,
                            frame: f,
                            x,
                            y,
                            opacity,
                            z,
                            data,
                        });
                        last = Last::Cel;
                    }
                    0x2018 => {
                        let n = c.u16()?;
                        c.skip(8)?;
                        for _ in 0..n {
                            let from = u32::from(c.u16()?);
                            let to = u32::from(c.u16()?);
                            let direction = match c.u8()? {
                                1 => TagDirection::Reverse,
                                2 => TagDirection::PingPong,
                                3 => TagDirection::PingPongReverse,
                                _ => TagDirection::Forward,
                            };
                            let repeat = c.u16()?;
                            c.skip(10)?;
                            let name = c.string()?;
                            ase.tags.push(AseTag {
                                name,
                                from,
                                to,
                                direction,
                                repeat,
                            });
                        }
                        last = Last::Tags;
                    }
                    0x2019 => {
                        let size = c.u32()? as usize;
                        let from = c.u32()? as usize;
                        let to = c.u32()? as usize;
                        c.skip(8)?;
                        if size > 65536 || to < from || to >= size.max(1) {
                            return Err(bad("a palette out of range"));
                        }
                        ase.palette.resize(size, [0; 4]);
                        for entry in &mut ase.palette[from..=to] {
                            let entry_flags = c.u16()?;
                            let rgba = c.take(4)?;
                            *entry = [rgba[0], rgba[1], rgba[2], rgba[3]];
                            if entry_flags & 1 != 0 {
                                c.string()?;
                            }
                        }
                        new_palette = true;
                        last = Last::Other;
                    }
                    0x2020 => {
                        let data_flags = c.u32()?;
                        if data_flags & 1 != 0 && matches!(last, Last::Cel) {
                            let text = c.string()?;
                            if let Some(frame) = ase.frames.last_mut() {
                                frame.events.extend(events_of(&text));
                            }
                        }
                        // A tag's user data follows its tags chunk, one each: still tags'.
                        if !matches!(last, Last::Tags) {
                            last = Last::Other;
                        }
                    }
                    0x2022 => {
                        let keys = c.u32()?;
                        let slice_flags = c.u32()?;
                        c.skip(4)?;
                        let name = c.string()?;
                        let mut slice = AseSlice {
                            name,
                            keys: Vec::new(),
                        };
                        for _ in 0..keys.min(65536) {
                            let frame = c.u32()?;
                            let rect = [c.i32()?, c.i32()?, c.i32()?, c.i32()?];
                            let center = if slice_flags & 1 != 0 {
                                Some([c.i32()?, c.i32()?, c.i32()?, c.i32()?])
                            } else {
                                None
                            };
                            let pivot = if slice_flags & 2 != 0 {
                                Some([c.i32()?, c.i32()?])
                            } else {
                                None
                            };
                            slice.keys.push(SliceKey {
                                frame,
                                rect,
                                center,
                                pivot,
                            });
                        }
                        ase.slices.push(slice);
                        last = Last::Other;
                    }
                    0x2023 => {
                        let id = c.u32()?;
                        let set_flags = c.u32()?;
                        let count = c.u32()?;
                        let w = u32::from(c.u16()?);
                        let h = u32::from(c.u16()?);
                        c.skip(16)?;
                        let _name = c.string()?;
                        if set_flags & 1 != 0 {
                            c.skip(8)?;
                        }
                        let mut set_pixels = Vec::new();
                        if set_flags & 2 != 0 {
                            let len = c.u32()? as usize;
                            let data = c.take(len)?;
                            if pixels {
                                let size = (w as usize)
                                    .checked_mul(h as usize)
                                    .and_then(|v| v.checked_mul(count as usize))
                                    .and_then(|v| v.checked_mul(bpp))
                                    .filter(|&v| v <= 1 << 30)
                                    .ok_or_else(|| bad("a tileset too large"))?;
                                set_pixels = inflate(data, size)?;
                            }
                        }
                        ase.tilesets.push(Tileset {
                            id,
                            flags: set_flags,
                            width: w,
                            height: h,
                            count,
                            pixels: set_pixels,
                        });
                        last = Last::Other;
                    }
                    _ => last = Last::Other,
                }
                r.at = chunk_end;
            }
            r.at = end;
        }
        if ase.frames.is_empty() {
            return Err(bad("no frames"));
        }
        Ok(ase)
    }

    /// The sheet's columns and rows: as square as the frames allow.
    pub fn grid(&self) -> (u32, u32) {
        let n = self.frames.len().max(1) as u32;
        let columns = (n as f64).sqrt().ceil() as u32;
        (columns, n.div_ceil(columns))
    }

    /// Frame `k`'s cell in the sheet: left, top, width, height in pixels.
    pub fn frame_rect(&self, k: u32) -> [u32; 4] {
        let (columns, _) = self.grid();
        [
            (k % columns) * self.width,
            (k / columns) * self.height,
            self.width,
            self.height,
        ]
    }

    /// Every frame drawn into one image, each in its cell ([`Self::frame_rect`]). Needs
    /// the pixels ([`Self::read`]).
    pub fn sheet(&self) -> Result<Image, ImageError> {
        engine_profiler::zone!("assets::aseprite::sheet");
        let (columns, rows) = self.grid();
        let (sheet_w, sheet_h) = (columns * self.width, rows * self.height);
        if sheet_w > MAX_SHEET || sheet_h > MAX_SHEET {
            return Err(bad("too many or too large frames for one sheet"));
        }
        let mut rgba = vec![0u8; (sheet_w as usize) * (sheet_h as usize) * 4];
        let mut canvas = vec![[0.0f32; 4]; (self.width as usize) * (self.height as usize)];
        for f in 0..self.frames.len() as u32 {
            canvas.fill([0.0; 4]);
            self.draw_children(None, f, &mut canvas);
            let [x0, y0, ..] = self.frame_rect(f);
            for y in 0..self.height {
                for x in 0..self.width {
                    let p = canvas[(y * self.width + x) as usize];
                    let at = (((y0 + y) * sheet_w + x0 + x) * 4) as usize;
                    for (k, v) in p.iter().enumerate() {
                        rgba[at + k] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                    }
                }
            }
        }
        Ok(Image {
            width: sheet_w,
            height: sheet_h,
            rgba,
        })
    }

    /// The children of group `parent` (the top level for `None`) merged onto `canvas` for
    /// frame `f`, bottom first, in their cels' z-index order (NOTE.5 of the
    /// specification, among siblings).
    fn draw_children(&self, parent: Option<usize>, f: u32, canvas: &mut [[f32; 4]]) {
        let mut order: Vec<(usize, i32)> = (0..self.layers.len())
            .filter(|&i| self.layers[i].parent == parent)
            .map(|i| (i, self.cel(i, f).map_or(0, |c| i32::from(c.z))))
            .collect();
        order.sort_by_key(|&(i, z)| (i as i32 + z, z));
        let layer_opacity = self.flags & 1 != 0;
        for (i, _) in order {
            let layer = &self.layers[i];
            if layer.flags & Layer::VISIBLE == 0 || layer.flags & Layer::REFERENCE != 0 {
                continue;
            }
            let opacity = if layer_opacity {
                f32::from(layer.opacity) / 255.0
            } else {
                1.0
            };
            if layer.kind == Layer::GROUP {
                // Groups merged on their own first when the file says so (header flag 2).
                if self.flags & 2 != 0 {
                    let mut group = vec![[0.0f32; 4]; canvas.len()];
                    self.draw_children(Some(i), f, &mut group);
                    for (b, s) in canvas.iter_mut().zip(&group) {
                        *b = composite(*b, *s, opacity, layer.blend);
                    }
                } else {
                    self.draw_children(Some(i), f, canvas);
                }
                continue;
            }
            let Some(cel) = self.cel(i, f) else {
                continue;
            };
            let opacity = opacity * f32::from(cel.opacity) / 255.0;
            let background = layer.flags & Layer::BACKGROUND != 0;
            match &cel.data {
                CelData::Image {
                    width,
                    height,
                    pixels,
                } => {
                    for v in 0..*height {
                        for u in 0..*width {
                            let k = (v * width + u) as usize;
                            let s = self.pixel(pixels, k, background);
                            self.put(
                                canvas,
                                cel.x + u as i32,
                                cel.y + v as i32,
                                s,
                                opacity,
                                layer.blend,
                            );
                        }
                    }
                }
                CelData::Tilemap {
                    width,
                    height,
                    tiles,
                    id,
                    flip_x,
                    flip_y,
                    flip_d,
                } => {
                    let Some(set) = self.tilesets.iter().find(|t| t.id == layer.tileset) else {
                        continue;
                    };
                    let (tw, th) = (set.width, set.height);
                    for ty in 0..*height {
                        for tx in 0..*width {
                            let value = tiles[(ty * width + tx) as usize];
                            let tile = value & id;
                            // Tile 0 is empty in files of this format (tileset flag 4).
                            let empty = if set.flags & 4 != 0 {
                                tile == 0
                            } else {
                                tile == *id
                            };
                            if empty || tile >= set.count {
                                continue;
                            }
                            for v in 0..th {
                                for u in 0..tw {
                                    let (mut su, mut sv) = (u, v);
                                    if value & flip_d != 0 && tw == th {
                                        (su, sv) = (sv, su);
                                    }
                                    if value & flip_x != 0 {
                                        su = tw - 1 - su;
                                    }
                                    if value & flip_y != 0 {
                                        sv = th - 1 - sv;
                                    }
                                    let k = ((tile * th + sv) * tw + su) as usize;
                                    let s = self.pixel(&set.pixels, k, background);
                                    let x = cel.x + (tx * tw + u) as i32;
                                    let y = cel.y + (ty * th + v) as i32;
                                    self.put(canvas, x, y, s, opacity, layer.blend);
                                }
                            }
                        }
                    }
                }
                CelData::Linked(_) | CelData::Skipped => {}
            }
        }
    }

    /// Layer `layer`'s cel of frame `f`, a linked one followed to the cel it shows.
    fn cel(&self, layer: usize, f: u32) -> Option<&Cel> {
        let find = |f: u32| {
            self.cels
                .iter()
                .find(|c| usize::from(c.layer) == layer && c.frame == f)
        };
        let cel = find(f)?;
        match cel.data {
            CelData::Linked(to) => find(to).filter(|c| !matches!(c.data, CelData::Linked(_))),
            _ => Some(cel),
        }
    }

    /// Pixel `k` of `pixels` (the file's colour depth) as straight RGBA in 0..1.
    fn pixel(&self, pixels: &[u8], k: usize, background: bool) -> [f32; 4] {
        let n = |v: u8| f32::from(v) / 255.0;
        match self.depth {
            32 => match pixels.get(k * 4..k * 4 + 4) {
                Some(p) => [n(p[0]), n(p[1]), n(p[2]), n(p[3])],
                None => [0.0; 4],
            },
            16 => match pixels.get(k * 2..k * 2 + 2) {
                Some(p) => [n(p[0]), n(p[0]), n(p[0]), n(p[1])],
                None => [0.0; 4],
            },
            _ => match pixels.get(k) {
                Some(&i) if background || i != self.transparent => {
                    let c = self.palette.get(usize::from(i)).copied().unwrap_or([0; 4]);
                    [n(c[0]), n(c[1]), n(c[2]), n(c[3])]
                }
                _ => [0.0; 4],
            },
        }
    }

    /// `s` merged onto `canvas` at (`x`, `y`) when that is inside it.
    fn put(&self, canvas: &mut [[f32; 4]], x: i32, y: i32, s: [f32; 4], opacity: f32, blend: u16) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let at = (y as u32 * self.width + x as u32) as usize;
        canvas[at] = composite(canvas[at], s, opacity, blend);
    }

    /// The sprite settings of this file: `meta`'s (its `.meta`, what the editor adds) with
    /// the file's frames as slices `frame_0`, ... (in their cells), its slices after them
    /// and its tags as clips. A `.meta` slice of the same name gives its pivot and border;
    /// a `.meta` clip of the same name adds its events; the others follow the file's.
    pub fn sprite(&self, meta: &SpriteImport) -> SpriteImport {
        let mut slices: Vec<SliceImport> = (0..self.frames.len() as u32)
            .map(|k| SliceImport {
                name: format!("frame_{k}"),
                rect: self.frame_rect(k),
                pivot: meta.pivot,
                border: [0; 4],
            })
            .collect();
        for slice in &self.slices {
            // The key of its first frame, in that frame's cell.
            let Some(key) = slice.keys.iter().min_by_key(|k| k.frame) else {
                continue;
            };
            let [x, y, w, h] = key.rect;
            let (x0, y0) = (x.max(0), y.max(0));
            let x1 = (x + w).min(self.width as i32);
            let y1 = (y + h).min(self.height as i32);
            if x1 <= x0 || y1 <= y0 || key.frame as usize >= self.frames.len() {
                continue;
            }
            let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
            let [cx, cy, ..] = self.frame_rect(key.frame);
            let border = key.center.map_or([0; 4], |[l, t, cw, ch]| {
                let l = l.clamp(0, w as i32) as u32;
                let t = t.clamp(0, h as i32) as u32;
                let r = (w as i32 - l as i32 - cw).clamp(0, w as i32) as u32;
                let b = (h as i32 - t as i32 - ch).clamp(0, h as i32) as u32;
                [l, t, r, b]
            });
            let pivot = key.pivot.map_or(meta.pivot, |[px, py]| {
                [px as f32 / w as f32, 1.0 - py as f32 / h as f32]
            });
            let mut name = slice.name.clone();
            let mut n = 2;
            while slices.iter().any(|s| s.name == name) {
                name = format!("{}_{n}", slice.name);
                n += 1;
            }
            slices.push(SliceImport {
                name,
                rect: [cx + x0 as u32, cy + y0 as u32, w, h],
                pivot,
                border,
            });
        }
        for m in &meta.slices {
            match slices.iter_mut().find(|s| s.name == m.name) {
                Some(s) => {
                    s.pivot = m.pivot;
                    s.border = m.border;
                }
                None => slices.push(m.clone()),
            }
        }
        let mut clips: Vec<SpriteClipImport> = self.tags.iter().map(|t| self.clip_of(t)).collect();
        if self.tags.is_empty() && self.frames.len() > 1 {
            clips.push(self.clip_of(&AseTag {
                name: String::from("all"),
                from: 0,
                to: self.frames.len() as u32 - 1,
                direction: TagDirection::Forward,
                repeat: 0,
            }));
        }
        for m in &meta.clips {
            match clips.iter_mut().find(|c| c.name == m.name) {
                Some(c) => c.events.extend(m.events.iter().cloned()),
                None => clips.push(m.clone()),
            }
        }
        SpriteImport {
            slices,
            clips,
            ..meta.clone()
        }
    }

    /// Tag `tag` as a clip: its run of frames each shown for its duration, its frames'
    /// events at the times they first show.
    fn clip_of(&self, tag: &AseTag) -> SpriteClipImport {
        let last_frame = self.frames.len() as u32 - 1;
        let (from, to) = (tag.from.min(last_frame), tag.to.min(last_frame));
        let (lo, hi) = (from.min(to), from.max(to));
        let reverse = matches!(
            tag.direction,
            TagDirection::Reverse | TagDirection::PingPongReverse
        );
        let (first, last) = if reverse { (hi, lo) } else { (lo, hi) };
        let run: Vec<u32> = if reverse {
            (lo..=hi).rev().collect()
        } else {
            (lo..=hi).collect()
        };
        let times: Vec<f32> = run
            .iter()
            .map(|&k| self.frames[k as usize].duration as f32 / 1000.0)
            .collect();
        let mode = match tag.direction {
            TagDirection::PingPong | TagDirection::PingPongReverse => SpriteClipMode::PingPong,
            _ if tag.repeat == 1 => SpriteClipMode::Once,
            _ => SpriteClipMode::Loop,
        };
        let mut events = Vec::new();
        let mut at = 0.0;
        for (&k, &t) in run.iter().zip(&times) {
            for (name, data) in &self.frames[k as usize].events {
                events.push(ClipEvent {
                    time: at,
                    name: name.clone(),
                    data: data.clone(),
                    sound: String::new(),
                });
            }
            at += t;
        }
        let frame_time = times.first().copied().unwrap_or(0.1);
        SpriteClipImport {
            name: tag.name.clone(),
            first,
            last,
            frame_time,
            durations: if times.iter().all(|&t| t == frame_time) {
                Vec::new()
            } else {
                times
            },
            mode,
            events,
        }
    }
}

/// A cel's user data text's events: its lines `event:<name> [data]`.
fn events_of(text: &str) -> impl Iterator<Item = (String, String)> + '_ {
    text.lines().filter_map(|line| {
        let rest = line.trim().strip_prefix("event:")?.trim();
        let (name, data) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        (!name.is_empty()).then(|| (name.to_owned(), data.trim().to_owned()))
    })
}

/// `s` (straight RGBA, at `opacity`) merged onto `b` with blend mode `mode`: the W3C
/// compositing rule (the source's colour mixed with the blend where the backdrop is,
/// then source over).
fn composite(b: [f32; 4], s: [f32; 4], opacity: f32, mode: u16) -> [f32; 4] {
    let sa = s[3] * opacity;
    if sa <= 0.0 {
        return b;
    }
    let ba = b[3];
    let cb = [b[0], b[1], b[2]];
    let cs = [s[0], s[1], s[2]];
    let mixed = blend(mode, cb, cs);
    let ra = sa + ba * (1.0 - sa);
    let mut out = [0.0; 4];
    for k in 0..3 {
        let c = (1.0 - ba) * cs[k] + ba * mixed[k];
        out[k] = (c * sa + cb[k] * ba * (1.0 - sa)) / ra;
    }
    out[3] = ra;
    out
}

/// Blend mode `mode` (the file's numbering) of backdrop `b` and source `s`.
fn blend(mode: u16, b: [f32; 3], s: [f32; 3]) -> [f32; 3] {
    let each = |f: fn(f32, f32) -> f32| [f(b[0], s[0]), f(b[1], s[1]), f(b[2], s[2])];
    match mode {
        1 => each(|b, s| b * s),
        2 => each(screen),
        3 => each(|b, s| hard_light(s, b)),
        4 => each(f32::min),
        5 => each(f32::max),
        6 => each(|b, s| {
            if b <= 0.0 {
                0.0
            } else if s >= 1.0 {
                1.0
            } else {
                (b / (1.0 - s)).min(1.0)
            }
        }),
        7 => each(|b, s| {
            if b >= 1.0 {
                1.0
            } else if s <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - b) / s).min(1.0)
            }
        }),
        8 => each(hard_light),
        9 => each(|b, s| {
            if s <= 0.5 {
                b - (1.0 - 2.0 * s) * b * (1.0 - b)
            } else {
                let d = if b <= 0.25 {
                    ((16.0 * b - 12.0) * b + 4.0) * b
                } else {
                    b.sqrt()
                };
                b + (2.0 * s - 1.0) * (d - b)
            }
        }),
        10 => each(|b, s| (b - s).abs()),
        11 => each(|b, s| b + s - 2.0 * b * s),
        12 => set_lum(set_sat(s, sat(b)), lum(b)),
        13 => set_lum(set_sat(b, sat(s)), lum(b)),
        14 => set_lum(s, lum(b)),
        15 => set_lum(b, lum(s)),
        16 => each(|b, s| (b + s).min(1.0)),
        17 => each(|b, s| (b - s).max(0.0)),
        18 => each(|b, s| {
            if b <= 0.0 {
                0.0
            } else if s <= 0.0 {
                1.0
            } else {
                (b / s).min(1.0)
            }
        }),
        _ => s,
    }
}

fn screen(b: f32, s: f32) -> f32 {
    b + s - b * s
}

fn hard_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b * 2.0 * s
    } else {
        screen(b, 2.0 * s - 1.0)
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn clip_color(c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    c.map(|v| {
        let mut v = v;
        if n < 0.0 && l - n > 0.0 {
            v = l + (v - l) * l / (l - n);
        }
        if x > 1.0 && x - l > 0.0 {
            v = l + (v - l) * (1.0 - l) / (x - l);
        }
        v
    })
}

fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip_color(c.map(|v| v + d))
}

fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let (n, x) = (c[0].min(c[1]).min(c[2]), c[0].max(c[1]).max(c[2]));
    if x - n <= 0.0 {
        return [0.0; 3];
    }
    c.map(|v| (v - n) * s / (x - n))
}

/// Aseprite files written for tests (this crate's and others'), chunk by chunk.
#[doc(hidden)]
pub mod sample {
    use super::{FRAME_MAGIC, MAGIC};

    /// An Aseprite file being written, chunk by chunk as the specification lays them out:
    /// each frame's duration (ms) and chunks.
    pub struct Writer {
        pub width: u16,
        pub height: u16,
        pub depth: u16,
        pub flags: u32,
        pub transparent: u8,
        pub frames: Vec<(u16, Vec<Vec<u8>>)>,
    }

    pub fn chunk(kind: u16, body: &[u8]) -> Vec<u8> {
        let mut c = ((body.len() + 6) as u32).to_le_bytes().to_vec();
        c.extend(kind.to_le_bytes());
        c.extend(body);
        c
    }

    pub fn string(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u16).to_le_bytes().to_vec();
        v.extend(s.as_bytes());
        v
    }

    pub fn layer(
        flags: u16,
        kind: u16,
        level: u16,
        blend: u16,
        opacity: u8,
        name: &str,
    ) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [flags, kind, level, 0, 0, blend] {
            b.extend(v.to_le_bytes());
        }
        b.push(opacity);
        b.extend([0; 3]);
        b.extend(string(name));
        chunk(0x2004, &b)
    }

    pub fn cel(layer: u16, x: i16, y: i16, w: u16, h: u16, pixels: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(layer.to_le_bytes());
        b.extend(x.to_le_bytes());
        b.extend(y.to_le_bytes());
        b.push(255);
        b.extend(2u16.to_le_bytes());
        b.extend(0i16.to_le_bytes());
        b.extend([0; 5]);
        b.extend(w.to_le_bytes());
        b.extend(h.to_le_bytes());
        b.extend(miniz_oxide::deflate::compress_to_vec_zlib(pixels, 6));
        chunk(0x2005, &b)
    }

    pub fn linked(layer: u16, frame: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(layer.to_le_bytes());
        b.extend([0; 4]);
        b.push(255);
        b.extend(1u16.to_le_bytes());
        b.extend([0; 7]);
        b.extend(frame.to_le_bytes());
        chunk(0x2005, &b)
    }

    pub fn user_text(text: &str) -> Vec<u8> {
        let mut b = 1u32.to_le_bytes().to_vec();
        b.extend(string(text));
        chunk(0x2020, &b)
    }

    pub fn tags(list: &[(u16, u16, u8, u16, &str)]) -> Vec<u8> {
        let mut b = (list.len() as u16).to_le_bytes().to_vec();
        b.extend([0; 8]);
        for &(from, to, dir, repeat, name) in list {
            b.extend(from.to_le_bytes());
            b.extend(to.to_le_bytes());
            b.push(dir);
            b.extend(repeat.to_le_bytes());
            b.extend([0; 10]);
            b.extend(string(name));
        }
        chunk(0x2018, &b)
    }

    impl Writer {
        pub fn new(width: u16, height: u16, depth: u16) -> Self {
            Self {
                width,
                height,
                depth,
                flags: 1,
                transparent: 0,
                frames: Vec::new(),
            }
        }

        pub fn bytes(&self) -> Vec<u8> {
            let mut out = vec![0u8; 128];
            out[4..6].copy_from_slice(&MAGIC.to_le_bytes());
            out[6..8].copy_from_slice(&(self.frames.len() as u16).to_le_bytes());
            out[8..10].copy_from_slice(&self.width.to_le_bytes());
            out[10..12].copy_from_slice(&self.height.to_le_bytes());
            out[12..14].copy_from_slice(&self.depth.to_le_bytes());
            out[14..18].copy_from_slice(&self.flags.to_le_bytes());
            out[18..20].copy_from_slice(&100u16.to_le_bytes());
            out[28] = self.transparent;
            for (duration, chunks) in &self.frames {
                let body: Vec<u8> = chunks.concat();
                out.extend(((body.len() + 16) as u32).to_le_bytes());
                out.extend(FRAME_MAGIC.to_le_bytes());
                out.extend((chunks.len() as u16).to_le_bytes());
                out.extend(duration.to_le_bytes());
                out.extend([0; 2]);
                out.extend((chunks.len() as u32).to_le_bytes());
                out.extend(body);
            }
            let len = out.len() as u32;
            out[0..4].copy_from_slice(&len.to_le_bytes());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sample::*;
    use super::*;

    fn px(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * image.width + x) * 4) as usize;
        image.rgba[at..at + 4].try_into().unwrap()
    }

    /// Two layers (normal and multiply) over two frames, the second linking its first
    /// layer's cel: the sheet holds both frames side by side, merged.
    #[test]
    fn layers_merge_with_their_blend_modes_into_a_sheet() {
        let mut w = Writer::new(2, 1, 32);
        let red = [255, 0, 0, 255, 255, 0, 0, 255];
        let grey = [128, 128, 128, 255];
        w.frames.push((
            100,
            vec![
                layer(1, 0, 0, 0, 255, "base"),
                layer(1, 0, 0, 1, 255, "shade"),
                cel(0, 0, 0, 2, 1, &red),
                cel(1, 1, 0, 1, 1, &grey),
            ],
        ));
        w.frames.push((150, vec![linked(0, 0)]));
        let bytes = w.bytes();
        assert!(is_aseprite(&bytes));
        let ase = Aseprite::read(&bytes).unwrap();
        assert_eq!(
            ase.frames.iter().map(|f| f.duration).collect::<Vec<_>>(),
            [100, 150]
        );
        assert_eq!(ase.grid(), (2, 1));
        let sheet = ase.sheet().unwrap();
        assert_eq!((sheet.width, sheet.height), (4, 1));
        assert_eq!(px(&sheet, 0, 0), [255, 0, 0, 255]);
        assert_eq!(px(&sheet, 1, 0), [128, 0, 0, 255], "red multiplied by grey");
        assert_eq!(px(&sheet, 2, 0), [255, 0, 0, 255], "the linked cel");
        assert_eq!(px(&sheet, 3, 0), [255, 0, 0, 255], "no shade in frame 1");
    }

    /// Hidden layers, layers in hidden groups and reference layers are not drawn; a
    /// layer's opacity is.
    #[test]
    fn hidden_layers_are_left_out_and_opacity_applies() {
        let mut w = Writer::new(1, 1, 32);
        let white = [255, 255, 255, 255];
        w.frames.push((
            100,
            vec![
                layer(1, 0, 0, 0, 128, "half"),
                layer(0, 0, 0, 0, 255, "hidden"),
                layer(0, 1, 0, 0, 255, "hidden group"),
                layer(1, 0, 1, 0, 255, "in it"),
                layer(1 | 64, 0, 0, 0, 255, "reference"),
                cel(0, 0, 0, 1, 1, &white),
                cel(1, 0, 0, 1, 1, &[0, 0, 0, 255]),
                cel(3, 0, 0, 1, 1, &[0, 0, 0, 255]),
                cel(4, 0, 0, 1, 1, &[0, 0, 0, 255]),
            ],
        ));
        let sheet = Aseprite::read(&w.bytes()).unwrap().sheet().unwrap();
        assert_eq!(px(&sheet, 0, 0), [255, 255, 255, 128]);
    }

    /// An indexed file's palette colours its pixels; the transparent index is clear on
    /// all but the background layer.
    #[test]
    fn indexed_pixels_take_the_palette() {
        let mut w = Writer::new(2, 1, 8);
        w.transparent = 0;
        let mut palette = Vec::new();
        for v in [2u32, 0, 1] {
            palette.extend(v.to_le_bytes());
        }
        palette.extend([0; 8]);
        for c in [[0, 0, 0, 255], [0, 255, 0, 255]] {
            palette.extend(0u16.to_le_bytes());
            palette.extend(c);
        }
        w.frames.push((
            100,
            vec![
                chunk(0x2019, &palette),
                layer(1, 0, 0, 0, 255, "a"),
                cel(0, 0, 0, 2, 1, &[0, 1]),
            ],
        ));
        let sheet = Aseprite::read(&w.bytes()).unwrap().sheet().unwrap();
        assert_eq!(px(&sheet, 0, 0), [0, 0, 0, 0], "transparent index");
        assert_eq!(px(&sheet, 1, 0), [0, 255, 0, 255]);
    }

    /// Tags become clips (reverse, ping-pong, once), frames slices in their cells, a
    /// slice its nine-slice border and pivot, and cel user data events; the `.meta`'s
    /// same-named slice and clip add to them.
    #[test]
    fn tags_slices_and_events_make_the_sprite_settings() {
        let mut w = Writer::new(8, 4, 32);
        let mut slice = Vec::new();
        for v in [1u32, 3, 0] {
            slice.extend(v.to_le_bytes());
        }
        slice.extend(string("panel"));
        for v in [0i32, 0, 0, 8, 4, 2, 1, 4, 2, 4, 4] {
            slice.extend(v.to_le_bytes());
        }
        let pixels = vec![255u8; 8 * 4 * 4];
        w.frames.push((
            100,
            vec![
                layer(1, 0, 0, 0, 255, "a"),
                cel(0, 0, 0, 8, 4, &pixels),
                tags(&[
                    (0, 2, 0, 0, "run"),
                    (1, 2, 1, 1, "back"),
                    (0, 2, 2, 0, "bounce"),
                ]),
                chunk(0x2022, &slice),
            ],
        ));
        w.frames.push((
            200,
            vec![
                cel(0, 0, 0, 8, 4, &pixels),
                user_text("event:footstep left\nnote"),
            ],
        ));
        w.frames.push((100, vec![cel(0, 0, 0, 8, 4, &pixels)]));
        let ase = Aseprite::read_layout(&w.bytes()).unwrap();
        assert_eq!(
            ase.frames[1].events,
            [(String::from("footstep"), String::from("left"))]
        );
        let meta = SpriteImport {
            slices: vec![SliceImport {
                name: String::from("frame_1"),
                pivot: [0.5, 0.0],
                ..SliceImport::default()
            }],
            clips: vec![SpriteClipImport {
                name: String::from("run"),
                events: vec![ClipEvent {
                    time: 0.0,
                    name: String::from("start"),
                    data: String::new(),
                    sound: String::new(),
                }],
                ..SpriteClipImport::default()
            }],
            ..SpriteImport::default()
        };
        let sprite = ase.sprite(&meta);
        let names: Vec<&str> = sprite.slices.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["frame_0", "frame_1", "frame_2", "panel"]);
        assert_eq!(ase.grid(), (2, 2));
        assert_eq!(sprite.slices[2].rect, [0, 4, 8, 4], "frame 2's cell");
        assert_eq!(sprite.slices[1].pivot, [0.5, 0.0], "the .meta's pivot");
        assert_eq!(sprite.slices[3].border, [2, 1, 2, 1]);
        assert_eq!(sprite.slices[3].pivot, [0.5, 0.0]);
        let run = &sprite.clips[0];
        assert_eq!(
            (run.first, run.last, run.mode),
            (0, 2, SpriteClipMode::Loop)
        );
        assert_eq!(run.durations, [0.1, 0.2, 0.1]);
        let events: Vec<(f32, &str)> = run
            .events
            .iter()
            .map(|e| (e.time, e.name.as_str()))
            .collect();
        assert_eq!(events, [(0.1, "footstep"), (0.0, "start")]);
        let back = &sprite.clips[1];
        assert_eq!(
            (back.first, back.last, back.mode),
            (2, 1, SpriteClipMode::Once)
        );
        assert_eq!(back.durations, [0.1, 0.2]);
        assert_eq!(back.events[0].time, 0.1);
        assert_eq!(sprite.clips[2].mode, SpriteClipMode::PingPong);
    }

    /// A file without tags plays all its frames as one clip.
    #[test]
    fn a_file_without_tags_is_one_clip() {
        let mut w = Writer::new(1, 1, 32);
        w.frames.push((100, vec![layer(1, 0, 0, 0, 255, "a")]));
        w.frames.push((100, Vec::new()));
        let sprite = Aseprite::read(&w.bytes())
            .unwrap()
            .sprite(&SpriteImport::default());
        assert_eq!(sprite.clips.len(), 1);
        assert_eq!((sprite.clips[0].first, sprite.clips[0].last), (0, 1));
        assert_eq!(sprite.clips[0].frame_time, 0.1);
        assert!(sprite.clips[0].durations.is_empty());
    }

    /// Every cut of a file is an error, never a panic.
    #[test]
    fn cut_files_are_errors() {
        let mut w = Writer::new(2, 2, 32);
        w.frames.push((
            100,
            vec![
                layer(1, 0, 0, 0, 255, "a"),
                cel(0, 0, 0, 2, 2, &[9; 16]),
                tags(&[(0, 0, 0, 0, "t")]),
                user_text("event:x"),
            ],
        ));
        let bytes = w.bytes();
        for n in 0..bytes.len() {
            assert!(Aseprite::read(&bytes[..n]).is_err(), "{n} bytes");
        }
        assert!(Aseprite::read(&bytes).is_ok());
    }

    /// The blend modes on known values (W3C's formulas).
    #[test]
    fn blend_modes_follow_the_w3c_formulas() {
        let b = [0.5, 0.25, 1.0];
        let s = [0.5, 0.5, 0.0];
        assert_eq!(blend(2, b, s), [0.75, 0.625, 1.0], "screen");
        assert_eq!(blend(10, b, s), [0.0, 0.25, 1.0], "difference");
        assert_eq!(blend(16, b, s), [1.0, 0.75, 1.0], "addition");
        assert_eq!(blend(17, b, s), [0.0, 0.0, 1.0], "subtract");
        assert_eq!(blend(18, b, s), [1.0, 0.5, 1.0], "divide");
        let l = blend(15, b, s);
        assert!(
            (lum(l) - lum(s)).abs() < 1e-5,
            "luminosity keeps the source's"
        );
        // Half a white pixel over opaque black: grey.
        let c = composite([0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0], 0.5, 0);
        assert_eq!(c, [0.5, 0.5, 0.5, 1.0]);
    }
}
