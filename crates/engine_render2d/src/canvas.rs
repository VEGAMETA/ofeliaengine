//! Recording 2D draws: [`Quad`] (sprites and SDF shapes), draw order, clipping, and the
//! sort + batch step that turns them into GPU instances.

use crate::sort::{self, Entry, ordered_f32};
use core::mem::MaybeUninit;
use engine_core::half;
use engine_core::math::{Affine2, Mat2, Vec2, Vec4};
use engine_rhi::{Bindless, SamplerKind};

/// How a quad combines with what is under it. `Alpha` and `Additive` share a pipeline
/// (premultiplied output, additive = alpha 0), so mixing them does not split a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Blend2d {
    /// Ordinary transparency.
    #[default]
    Alpha,
    /// Adds light (glows, particles). Keeps the destination's alpha.
    Additive,
    /// Darkens: `dst × lerp(1, color, alpha)` (shadows, tinting). Keeps the destination's
    /// alpha.
    Multiply,
}

/// Where a quad goes in the draw order: layers first (lower layers are drawn first), then
/// `key` within a layer (lower first), then submission order. The order is deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Order {
    /// Layer.
    pub layer: i16,
    /// Sort key within the layer.
    pub key: f32,
}

impl Order {
    /// Submission order within `layer`.
    pub const fn layer(layer: i16) -> Self {
        Self { layer, key: 0.0 }
    }

    /// Y-sorting within `layer` for a y-up world: quads further up (further away in a
    /// top-down view) are drawn first. Pass the y of the quad's base (feet).
    pub fn y_sorted(layer: i16, y: f32) -> Self {
        Self { layer, key: -y }
    }

    fn sort_key(self) -> u64 {
        let layer = (self.layer as u16 ^ 0x8000) as u64;
        (layer << 32) | u64::from(ordered_f32(self.key))
    }
}

/// Cells a tile chunk is wide and tall ([`Canvas::draw_tiles`]).
pub const TILE_CHUNK: usize = 32;

/// A tile of a tile set as tile-map chunks draw it (ADR 0125's E13-3b): its image and
/// the region of it, as a sprite's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileEntry {
    /// Bindless sampled-image index, or `None` while its image is not made (drawn clear).
    pub texture: Option<u32>,
    /// Sampler for the image.
    pub sampler: SamplerKind,
    /// Region: min u, min v, max u, max v; the tile's top at min v.
    pub uv: [f32; 4],
}

/// A table of tiles recorded in a canvas ([`Canvas::tile_table`]), for its chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileTable(u32);

/// An SDF outline for a quad: rounded corners, optionally hollow. Edges are antialiased.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Shape {
    /// Corner radii in world units for the quad's local quadrants (+x+y, −x+y, −x−y,
    /// +x−y): in a y-up view, top-right, top-left, bottom-left, bottom-right. Clamped to
    /// half the shorter side.
    pub radii: [f32; 4],
    /// Outline width in world units, inside the edge. 0 fills the shape.
    pub thickness: f32,
}

/// A clip rectangle in target pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipRect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// One quad: a sprite (textured or not) or an SDF shape. `transform` maps the unit square
/// centered at the origin (−0.5…0.5 on both axes) into the world; shapes need it without
/// skew.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    /// Unit square → world.
    pub transform: Affine2,
    /// Texture region: min u, min v, max u, max v. The top of the image (min v) is at the
    /// quad's local +y edge.
    pub uv: [f32; 4],
    /// Linear color with straight alpha; multiplies the texture.
    pub color: Vec4,
    /// Bindless sampled-image index, or `None` for a solid color.
    pub texture: Option<u32>,
    /// Sampler for the texture.
    pub sampler: SamplerKind,
    /// SDF edge: `None` draws the quad with hard edges (pixel-exact sprites).
    pub shape: Option<Shape>,
    /// Blending.
    pub blend: Blend2d,
    /// Draw order.
    pub order: Order,
}

impl Default for Quad {
    fn default() -> Self {
        Self {
            transform: Affine2::IDENTITY,
            uv: [0.0, 0.0, 1.0, 1.0],
            color: Vec4::ONE,
            texture: None,
            sampler: SamplerKind::LinearClamp,
            shape: None,
            blend: Blend2d::Alpha,
            order: Order::default(),
        }
    }
}

impl Quad {
    /// A textured, axis-aligned sprite of `size` centered at `center`.
    pub fn sprite(texture: u32, center: Vec2, size: Vec2) -> Self {
        Self {
            transform: axis_aligned(center, size),
            texture: Some(texture),
            ..Self::default()
        }
    }

    /// A solid, axis-aligned rectangle with hard, pixel-exact edges (a sprite without a
    /// texture: pixel art, UI blocks).
    pub fn solid(center: Vec2, size: Vec2) -> Self {
        Self {
            transform: axis_aligned(center, size),
            ..Self::default()
        }
    }

    /// A solid rectangle with antialiased edges.
    pub fn rect(center: Vec2, size: Vec2) -> Self {
        Self {
            transform: axis_aligned(center, size),
            shape: Some(Shape::default()),
            ..Self::default()
        }
    }

    /// A rectangle with all corners rounded by `radius`.
    pub fn rounded_rect(center: Vec2, size: Vec2, radius: f32) -> Self {
        Self {
            shape: Some(Shape {
                radii: [radius; 4],
                thickness: 0.0,
            }),
            ..Self::rect(center, size)
        }
    }

    /// A disc.
    pub fn circle(center: Vec2, radius: f32) -> Self {
        Self::rounded_rect(center, Vec2::splat(2.0 * radius), radius)
    }

    /// A line from `a` to `b` with round caps (a capsule).
    pub fn line(a: Vec2, b: Vec2, width: f32) -> Self {
        let delta = b - a;
        let length = delta.length();
        let angle = delta.y.atan2(delta.x);
        Self {
            transform: Affine2::from_scale_angle_translation(
                Vec2::new(length + width, width),
                angle,
                (a + b) * 0.5,
            ),
            shape: Some(Shape {
                radii: [width * 0.5; 4],
                thickness: 0.0,
            }),
            ..Self::default()
        }
    }

    /// Sets the color (linear, straight alpha).
    pub fn color(mut self, color: Vec4) -> Self {
        self.color = color;
        self
    }

    /// Sets the texture region.
    pub fn uv(mut self, uv: [f32; 4]) -> Self {
        self.uv = uv;
        self
    }

    /// Sets the sampler.
    pub fn sampler(mut self, sampler: SamplerKind) -> Self {
        self.sampler = sampler;
        self
    }

    /// Sets the blend mode.
    pub fn blend(mut self, blend: Blend2d) -> Self {
        self.blend = blend;
        self
    }

    /// Sets the draw order.
    pub fn order(mut self, order: Order) -> Self {
        self.order = order;
        self
    }

    /// Rotates by `angle` radians (counter-clockwise in a y-up view) about the quad's
    /// center.
    pub fn rotated(mut self, angle: f32) -> Self {
        self.transform.matrix2 = Mat2::from_angle(angle) * self.transform.matrix2;
        self
    }

    /// Draws only an outline of `thickness` (inside the edge); makes the quad a shape.
    pub fn outline(mut self, thickness: f32) -> Self {
        self.shape.get_or_insert_with(Shape::default).thickness = thickness;
        self
    }
}

/// Unit square → an axis-aligned rectangle (no trigonometry).
fn axis_aligned(center: Vec2, size: Vec2) -> Affine2 {
    Affine2 {
        matrix2: Mat2::from_diagonal(size),
        translation: center,
    }
}

/// The GPU form of a quad: `Instance` in `shaders/sprite.slang` (64 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Instance {
    pub(crate) axis_x: [f32; 2],
    pub(crate) axis_y: [f32; 2],
    pub(crate) center: [f32; 2],
    pub(crate) color: [u32; 2],
    pub(crate) uv: [f32; 4],
    pub(crate) texture: u32,
    pub(crate) flags: u32,
    pub(crate) radii: [u32; 2],
}

const _: () = assert!(core::mem::size_of::<Instance>() == 64);

pub(crate) const NO_TEXTURE: u32 = u32::MAX;
const FLAG_SDF: u32 = 0x8;
const FLAG_ADDITIVE: u32 = 0x10;
/// A tile chunk: `texture` is its record's byte offset in the canvas's tile data, made an
/// address (low 32 bits there, high 16 in the flags' top) when the canvas is staged.
pub(crate) const FLAG_TILES: u32 = 0x20;

/// Pipeline selector in a batch's state: 0 premultiplied (alpha and additive), 1 multiply,
/// 2 tile chunks.
pub(crate) const PIPELINE_MULTIPLY: u16 = 1;
/// See [`PIPELINE_MULTIPLY`].
pub(crate) const PIPELINE_TILES: u16 = 2;
/// Bits of a batch's state that select its pipeline; the clip index is above them.
const PIPELINE_BITS: u16 = 2;

/// `u32`s of a tile table entry: uv (4 floats), texture, sampler, 2 spare.
const TILE_ENTRY_WORDS: usize = 8;
/// `u32`s of a chunk record before its cells: the table's offset from the record (in
/// `u32`s, signed), 3 spare (16-byte alignment).
const TILE_HEADER_WORDS: usize = 4;

impl Instance {
    fn new(quad: &Quad) -> Self {
        let m = quad.transform.matrix2;
        let axis_x = m.x_axis * 0.5;
        let axis_y = m.y_axis * 0.5;
        let mut flags = Bindless::sampler(quad.sampler) & 0x7;
        if quad.blend == Blend2d::Additive {
            flags |= FLAG_ADDITIVE;
        }
        let mut radii = [0; 2];
        if let Some(shape) = quad.shape {
            flags |= FLAG_SDF;
            let limit = axis_x.length().min(axis_y.length());
            radii = half::pack4(shape.radii.map(|r| r.clamp(0.0, limit)));
            let thickness = half::from_f32(shape.thickness.max(0.0));
            flags |= u32::from(thickness) << 16;
        }
        Self {
            axis_x: axis_x.to_array(),
            axis_y: axis_y.to_array(),
            center: quad.transform.translation.to_array(),
            color: half::pack4(quad.color.to_array()),
            uv: quad.uv,
            texture: quad.texture.unwrap_or(NO_TEXTURE),
            flags,
            radii,
        }
    }
}

/// A run of sorted instances drawn with one pipeline and one clip rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Batch {
    pub(crate) first: u32,
    pub(crate) count: u32,
    /// Bits 0-1: pipeline ([`PIPELINE_MULTIPLY`], [`PIPELINE_TILES`]); the rest: clip
    /// index (0 = none).
    pub(crate) state: u16,
}

impl Batch {
    pub(crate) fn pipeline(self) -> u16 {
        self.state & ((1 << PIPELINE_BITS) - 1)
    }

    pub(crate) fn clip(self) -> usize {
        usize::from(self.state >> PIPELINE_BITS)
    }
}

/// Statistics of the last prepared canvas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CanvasStats {
    /// Quads.
    pub quads: u32,
    /// Batches (pipeline or clip changes; texture changes do not count).
    pub batches: u32,
    /// Whether the quads were already in order (no sorting needed).
    pub presorted: bool,
}

/// A list of 2D draws for one frame. Reuse it across frames ([`Self::clear`]): its storage
/// is kept, so steady-state frames do not allocate.
#[derive(Debug, Default)]
pub struct Canvas {
    instances: Vec<Instance>,
    entries: Vec<Entry>,
    clips: Vec<ClipRect>,
    current_state: u16,
    scratch: Vec<Entry>,
    batches: Vec<Batch>,
    stats: CanvasStats,
    prepared: bool,
    /// Tile tables and chunk records ([`Self::draw_tiles`]), staged with the instances.
    tile_data: Vec<u32>,
}

/// Largest number of distinct clip rectangles per canvas.
pub const MAX_CLIPS: usize = (u16::MAX >> PIPELINE_BITS) as usize;

impl Canvas {
    /// An empty canvas.
    pub fn new() -> Self {
        Self::default()
    }

    /// Removes every draw and clip; keeps the storage.
    pub fn clear(&mut self) {
        self.instances.clear();
        self.entries.clear();
        self.clips.clear();
        self.current_state = 0;
        self.batches.clear();
        self.tile_data.clear();
        self.prepared = false;
    }

    /// Clips later draws to `clip` (target pixels), or stops clipping with `None`.
    /// Setting the same rectangle again reuses it. Clip changes split batches.
    pub fn set_clip(&mut self, clip: Option<ClipRect>) {
        let index = match clip {
            None => 0,
            Some(rect) => match self.clips.last() {
                Some(last) if *last == rect => self.clips.len(),
                _ if self.clips.len() >= MAX_CLIPS => self.clips.len(),
                _ => {
                    self.clips.push(rect);
                    self.clips.len()
                }
            },
        };
        self.current_state = (index as u16) << PIPELINE_BITS;
        self.prepared = false;
    }

    /// Adds a quad.
    pub fn draw(&mut self, quad: &Quad) {
        let index = self.instances.len() as u32;
        self.prepared = false;
        self.instances.push(Instance::new(quad));
        let pipeline = u16::from(quad.blend == Blend2d::Multiply) * PIPELINE_MULTIPLY;
        self.entries.push(Entry {
            key: quad.order.sort_key(),
            index,
            state: self.current_state | pipeline,
        });
    }

    /// Records a table of tiles for the chunks drawn after it ([`Self::draw_tiles`]):
    /// a chunk's cell value `k + 1` shows `entries[k]`.
    pub fn tile_table(&mut self, entries: &[TileEntry]) -> TileTable {
        let start = self.tile_data.len() as u32;
        for e in entries {
            self.tile_data.extend(e.uv.map(f32::to_bits));
            self.tile_data.push(e.texture.unwrap_or(NO_TEXTURE));
            self.tile_data.push(Bindless::sampler(e.sampler));
            self.tile_data.extend([0, 0]);
        }
        debug_assert_eq!(self.tile_data.len() % 4, 0);
        let _ = TILE_ENTRY_WORDS;
        TileTable(start)
    }

    /// Adds a chunk of a flat tile layer (ADR 0125's E13-3b): one quad that `quad` places
    /// (its transform, color and order) over [`TILE_CHUNK`]² cells, drawn by its own
    /// fragment entry point. `cells` holds them row by row from the chunk's bottom (in a
    /// y-up view), each 0 for empty or a tile of `table` plus one (20 bits) with its
    /// orientation above (bit 20 flip x, 21 flip y, 22 transpose).
    pub fn draw_tiles(
        &mut self,
        quad: &Quad,
        table: TileTable,
        cells: &[u32; TILE_CHUNK * TILE_CHUNK],
    ) {
        let record = self.tile_data.len() as u32;
        let offset = table.0 as i64 - i64::from(record);
        self.tile_data.extend([offset as i32 as u32, 0, 0, 0]);
        self.tile_data.extend_from_slice(cells);
        let _ = TILE_HEADER_WORDS;
        let index = self.instances.len() as u32;
        self.prepared = false;
        let chunk = TILE_CHUNK as f32;
        let mut instance = Instance::new(&Quad {
            uv: [0.0, chunk, chunk, 0.0],
            texture: None,
            shape: None,
            ..*quad
        });
        instance.texture = record * 4;
        instance.flags = FLAG_TILES;
        self.instances.push(instance);
        self.entries.push(Entry {
            key: quad.order.sort_key(),
            index,
            state: self.current_state | PIPELINE_TILES,
        });
    }

    /// The tile tables and chunk records, as the GPU reads them.
    pub(crate) fn tile_data(&self) -> &[u32] {
        &self.tile_data
    }

    /// Number of quads.
    pub fn len(&self) -> usize {
        self.instances.len()
    }

    /// Whether nothing was drawn.
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Whether [`Self::prepare`] ran after the last change.
    pub fn is_prepared(&self) -> bool {
        self.prepared
    }

    /// Statistics of the last [`Self::prepare`].
    pub fn stats(&self) -> CanvasStats {
        self.stats
    }

    /// Sorts the quads into draw order and forms batches. Call it after recording and
    /// before [`crate::Renderer2d::add_pass`]; it can run on any thread (a job preparing
    /// its own canvas).
    pub fn prepare(&mut self) {
        engine_profiler::zone!("render2d::Canvas::prepare");
        let presorted = sort::sort(&mut self.entries, &mut self.scratch);
        self.batches.clear();
        for (i, entry) in self.entries.iter().enumerate() {
            let state = entry.state;
            match self.batches.last_mut() {
                Some(batch) if batch.state == state => batch.count += 1,
                _ => self.batches.push(Batch {
                    first: i as u32,
                    count: 1,
                    state,
                }),
            }
        }
        self.prepared = true;
        self.stats = CanvasStats {
            quads: self.instances.len() as u32,
            batches: self.batches.len() as u32,
            presorted,
        };
    }

    /// The instances in submission order (the GPU reads them through the draw order).
    pub(crate) fn instances(&self) -> &[Instance] {
        &self.instances
    }

    /// Writes the draw order (instance indices, as long as [`Self::len`]) into `target`.
    pub(crate) fn write_order(&self, target: &mut [MaybeUninit<u32>]) {
        for (slot, entry) in target.iter_mut().zip(&self.entries) {
            slot.write(entry.index);
        }
    }

    pub(crate) fn batches(&self) -> &[Batch] {
        &self.batches
    }

    pub(crate) fn clip_rect(&self, index: usize) -> Option<ClipRect> {
        index
            .checked_sub(1)
            .and_then(|i| self.clips.get(i))
            .copied()
    }

    #[cfg(test)]
    pub(crate) fn sorted_instances(&self) -> Vec<Instance> {
        self.entries
            .iter()
            .map(|e| self.instances[e.index as usize])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagged(canvas: &mut Canvas, tag: f32, order: Order) {
        canvas.draw(&Quad::rect(Vec2::new(tag, 0.0), Vec2::ONE).order(order));
    }

    fn tags(canvas: &Canvas) -> Vec<f32> {
        canvas
            .sorted_instances()
            .iter()
            .map(|i| i.center[0])
            .collect()
    }

    #[test]
    fn layers_then_keys_then_submission_order() {
        let mut canvas = Canvas::new();
        tagged(&mut canvas, 0.0, Order::layer(1));
        tagged(&mut canvas, 1.0, Order::layer(-3));
        tagged(&mut canvas, 2.0, Order::y_sorted(0, 5.0));
        tagged(&mut canvas, 3.0, Order::y_sorted(0, -5.0));
        tagged(&mut canvas, 4.0, Order::layer(1));
        tagged(&mut canvas, 5.0, Order::y_sorted(0, 5.0));
        canvas.prepare();
        // Layer −3; layer 0 by descending y, ties in submission order; layer 1 in order.
        assert_eq!(tags(&canvas), [1.0, 2.0, 5.0, 3.0, 0.0, 4.0]);
        assert!(!canvas.stats().presorted);

        // Same input, same output.
        let first = tags(&canvas);
        canvas.prepare();
        assert_eq!(tags(&canvas), first);
    }

    #[test]
    fn batches_split_on_pipeline_and_clip_only() {
        let mut canvas = Canvas::new();
        let clip = ClipRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        for texture in 0..4 {
            canvas.draw(&Quad::sprite(texture, Vec2::ZERO, Vec2::ONE));
        }
        canvas.draw(&Quad::rect(Vec2::ZERO, Vec2::ONE).blend(Blend2d::Additive));
        canvas.draw(&Quad::rect(Vec2::ZERO, Vec2::ONE).blend(Blend2d::Multiply));
        canvas.set_clip(Some(clip));
        canvas.draw(&Quad::circle(Vec2::ZERO, 1.0));
        canvas.set_clip(Some(clip));
        canvas.draw(&Quad::circle(Vec2::ZERO, 1.0));
        canvas.set_clip(None);
        canvas.draw(&Quad::circle(Vec2::ZERO, 1.0));
        canvas.prepare();
        let batches: Vec<(u32, u32, u16, usize)> = canvas
            .batches()
            .iter()
            .map(|b| (b.first, b.count, b.pipeline(), b.clip()))
            .collect();
        assert_eq!(
            batches,
            [(0, 5, 0, 0), (5, 1, 1, 0), (6, 2, 0, 1), (8, 1, 0, 0)]
        );
        assert_eq!(canvas.clip_rect(1), Some(clip));
        assert_eq!(canvas.clip_rect(0), None);
        assert!(canvas.stats().presorted);
        assert!(canvas.is_prepared());
        canvas.draw(&Quad::circle(Vec2::ZERO, 1.0));
        assert!(
            !canvas.is_prepared(),
            "a change invalidates the preparation"
        );
    }

    #[test]
    fn instance_encoding() {
        let quad = Quad::line(Vec2::new(0.0, 0.0), Vec2::new(0.0, 4.0), 2.0)
            .color(Vec4::new(1.0, 0.5, 0.0, 1.0))
            .blend(Blend2d::Additive)
            .outline(0.5);
        let i = Instance::new(&quad);
        // Rotated 90°: local x runs along +y with half length (4 + 2) / 2.
        assert!((i.axis_x[0]).abs() < 1e-6 && (i.axis_x[1] - 3.0).abs() < 1e-6);
        assert!((i.axis_y[0] + 1.0).abs() < 1e-6 && i.axis_y[1].abs() < 1e-6);
        assert_eq!(i.center, [0.0, 2.0]);
        assert_eq!(i.texture, NO_TEXTURE);
        assert_eq!(
            i.flags & (FLAG_SDF | FLAG_ADDITIVE),
            FLAG_SDF | FLAG_ADDITIVE
        );
        assert_eq!(half::to_f32((i.flags >> 16) as u16), 0.5);
        assert_eq!(i.radii, half::pack4([1.0; 4]));
        assert_eq!(i.color, half::pack4([1.0, 0.5, 0.0, 1.0]));

        // Radii are clamped to half the shorter side.
        let big = Instance::new(&Quad::rounded_rect(Vec2::ZERO, Vec2::new(4.0, 2.0), 5.0));
        assert_eq!(big.radii, half::pack4([1.0; 4]));
    }
}
