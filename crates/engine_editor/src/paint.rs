//! Painting `engine_ui` frames with `engine_render2d` and `engine_text`.
//!
//! - Rectangles become quads: plain ones are pixel-exact solid quads, rounded ones SDF
//!   shapes with antialiased edges, borders SDF outlines.
//! - Text becomes glyph quads from one shared [`GlyphAtlas`], laid out with the same fonts
//!   and widths the UI measured with ([`UiText`] is the UI's [`TextMeasure`]), so what is
//!   measured is what is drawn.
//! - Laid-out strings are kept while they stay on screen: a label is shaped once, not in
//!   every frame's layout and again in every paint. A text that changes (a counter, the
//!   frame rate) is laid out into a layout long out of use, its buffers reused: nothing is
//!   allocated once the editor has run a while.
//! - Clip commands become canvas clip rectangles.
//!
//! Quads keep the UI's paint order (one canvas layer, submission order). Things drawn
//! below the UI, such as a 3D viewport's image, go on a lower layer.

use engine_core::hash::StateHasher;
use engine_core::math::{Vec2, Vec4};
use engine_render2d::{Canvas, ClipRect, GlyphTexture, Order, Quad, draw_glyphs};
use engine_text::{FontError, FontId, Fonts, GlyphAtlas, PositionedGlyph, TextLayout, TextStyle};
use engine_ui::{DrawCmd, Frame, TextMeasure};
use std::collections::HashMap;
use std::collections::hash_map::Entry;

/// Side of the glyph atlas in pixels: room for the editor's sizes of Latin and Cyrillic.
pub const ATLAS_SIZE: u32 = 1024;

/// Above this many cached layouts, those unused this frame and the last are dropped.
const CACHE_SOFT_LIMIT: usize = 4096;

/// A layout unused for this many frames (two seconds at 60 Hz) is laid out into again by
/// the next new text, its buffers reused; one used more lately waits for its text.
const STALE_FRAMES: u64 = 120;
/// The text bytes and glyphs a layout has room for at least.
const LAYOUT_ROOM: usize = 32;
/// Layouts out of use kept at hand: below this many, a frame that laid texts out takes back
/// those long out of use, then those unused this frame and the last, then makes new ones,
/// so a frame that needs several new texts at once (the numbers of a vector a clip moves
/// every frame) never runs out halfway, and texts that change every frame make none in
/// steady state.
const SPARE_LOW: usize = 64;

/// Fonts, laid-out strings and the glyph atlas of the UI.
pub struct UiText {
    /// Loaded fonts.
    pub fonts: Fonts,
    /// The fonts text uses, in fallback order; code's, the monospace face first.
    stack: Vec<FontId>,
    mono: Vec<FontId>,
    layout: TextLayout,
    cache: LayoutCache,
    /// Rendered glyphs; mirror it on the GPU with [`GlyphTexture::sync`] after painting.
    pub atlas: GlyphAtlas,
}

impl UiText {
    /// Draws `s` at `size` pixels with its laid-out box centered on `center` (an icon over
    /// the viewport, outside the UI's layout): the glyphs drawn.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_centered(
        &mut self,
        canvas: &mut Canvas,
        texture: &GlyphTexture,
        s: &str,
        size: f32,
        center: Vec2,
        color: Vec4,
        order: Order,
    ) -> usize {
        let laid_out = self.cache.get(
            (&mut self.fonts, &self.stack, &mut self.layout),
            s,
            size,
            None,
            false,
        );
        let origin = center - Vec2::new(laid_out.extent[0], laid_out.extent[1]) * 0.5;
        draw_glyphs(
            canvas,
            &laid_out.glyphs,
            &self.fonts,
            &mut self.atlas,
            texture,
            origin,
            size,
            color,
            order,
        )
    }
}

impl UiText {
    /// Draws `s` at `size` pixels from `left`, its laid-out box's left edge at `left.x`
    /// and its middle at `left.y` (a graph's labels, ADR 0123): the glyphs drawn.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_left(
        &mut self,
        canvas: &mut Canvas,
        texture: &GlyphTexture,
        s: &str,
        size: f32,
        left: Vec2,
        color: Vec4,
        order: Order,
    ) -> usize {
        let laid_out = self.cache.get(
            (&mut self.fonts, &self.stack, &mut self.layout),
            s,
            size,
            None,
            false,
        );
        let origin = left - Vec2::new(0.0, laid_out.extent[1] * 0.5);
        draw_glyphs(
            canvas,
            &laid_out.glyphs,
            &self.fonts,
            &mut self.atlas,
            texture,
            origin,
            size,
            color,
            order,
        )
    }

    /// A character's width in the monospace face at `size` pixels, not rounded: what a
    /// code view's columns are placed by.
    pub fn mono_advance(&mut self, size: f32) -> f32 {
        let laid_out = self.cache.get(
            (&mut self.fonts, &self.mono, &mut self.layout),
            "0000000000",
            size,
            None,
            true,
        );
        laid_out.extent[0] / 10.0
    }
}

impl core::fmt::Debug for UiText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UiText")
            .field("fonts", &self.stack.len())
            .field("cached_layouts", &self.cache.map.len())
            .finish_non_exhaustive()
    }
}

/// A string laid out once, reused while it stays on screen.
struct CachedLayout {
    text: String,
    /// Laid out in the monospace stack (code).
    mono: bool,
    size: u32,
    /// Wrap width bits, `u32::MAX` for none.
    max_width: u32,
    glyphs: Vec<PositionedGlyph>,
    extent: [f32; 2],
    /// Frame of last use.
    frame: u64,
}

impl CachedLayout {
    /// A layout with no text, its buffers with room for a short one ([`LAYOUT_ROOM`]).
    fn with_room() -> Self {
        Self {
            text: String::with_capacity(LAYOUT_ROOM),
            glyphs: Vec::with_capacity(LAYOUT_ROOM),
            ..Self::empty()
        }
    }

    /// A layout with no text and no buffers (nothing allocated).
    const fn empty() -> Self {
        Self {
            text: String::new(),
            mono: false,
            size: 0,
            max_width: 0,
            glyphs: Vec::new(),
            extent: [0.0; 2],
            frame: 0,
        }
    }
}

/// Layouts by a hash of (text, size, wrap width); an entry is only used if all three
/// match, so a hash collision costs a layout, never the wrong text.
#[derive(Default)]
struct LayoutCache {
    map: HashMap<u64, CachedLayout>,
    /// Layouts out of use, their buffers kept for the next new texts.
    spare: Vec<CachedLayout>,
    frame: u64,
    layouts: u64,
    /// A text was laid out this frame.
    missed: bool,
}

impl LayoutCache {
    /// An empty cache with room for twice [`CACHE_SOFT_LIMIT`] layouts, made once: the
    /// entries [`Self::end_frame`] removes leave deleted slots behind, and with at most half
    /// its room in use the map rehashes them away in place when its room runs out, never by
    /// growing into a larger table (which allocates, at a frame the hash's seed decides).
    /// The layouts out of use have room for as many as the map keeps before it drops them
    /// (taken out of use only below that) beside the few left when more are taken
    /// ([`SPARE_LOW`]), so many going out of use at once allocate nothing.
    fn new() -> Self {
        Self {
            map: HashMap::with_capacity(2 * CACHE_SOFT_LIMIT),
            spare: Vec::with_capacity(CACHE_SOFT_LIMIT + SPARE_LOW),
            ..Self::default()
        }
    }

    /// The layout of `text`, laid out with `shaper` if it is not cached.
    fn get(
        &mut self,
        (fonts, stack, layout): (&mut Fonts, &[FontId], &mut TextLayout),
        text: &str,
        size: f32,
        max_width: Option<f32>,
        mono: bool,
    ) -> &CachedLayout {
        let (size_bits, width_bits) = (size.to_bits(), max_width.map_or(u32::MAX, f32::to_bits));
        let mut h = StateHasher::new();
        h.write(text.as_bytes());
        h.write_u64(u64::from(size_bits) | (u64::from(width_bits) << 32));
        // The monospace stack's layouts kept apart.
        h.write_u64(u64::from(mono));
        let spare = &mut self.spare;
        let (entry, hit) = match self.map.entry(h.finish()) {
            Entry::Occupied(o) => {
                let e = o.into_mut();
                let hit = e.text == text
                    && e.mono == mono
                    && e.size == size_bits
                    && e.max_width == width_bits;
                (e, hit)
            }
            // A new text: into a layout out of use where there is one.
            Entry::Vacant(v) => (
                v.insert(spare.pop().unwrap_or(CachedLayout::empty())),
                false,
            ),
        };
        if !hit {
            self.layouts += 1;
            self.missed = true;
            let style = TextStyle {
                size,
                max_width,
                ..TextStyle::default()
            };
            layout.layout(fonts, stack, text, &style);
            // Room to the next power of two, and for a short text at least: a text a little
            // longer fits the next time, and a layout taken out of use fits any short text
            // (a clip's time, a frame's numbers) without growing.
            entry.text.clear();
            entry
                .text
                .reserve(text.len().next_power_of_two().max(LAYOUT_ROOM));
            entry.text.push_str(text);
            entry.mono = mono;
            entry.size = size_bits;
            entry.max_width = width_bits;
            entry.glyphs.clear();
            entry
                .glyphs
                .reserve(layout.glyphs().len().next_power_of_two().max(LAYOUT_ROOM));
            entry.glyphs.extend_from_slice(layout.glyphs());
            entry.extent = [layout.width(), layout.height()];
        }
        entry.frame = self.frame;
        entry
    }

    /// Ends a frame: drops what went unused this frame and the last, once the cache is big.
    /// After a frame that laid texts out with few spare layouts left ([`SPARE_LOW`]), takes
    /// those long out of use for the next ([`STALE_FRAMES`]), then, still short, those
    /// unused this frame and the last, then makes new ones up to it (the map keeps its
    /// capacity: nothing freed; made only while texts outgrow what was ever kept).
    fn end_frame(&mut self) {
        if self.map.len() > CACHE_SOFT_LIMIT {
            let keep_from = self.frame.saturating_sub(1);
            self.map.retain(|_, e| e.frame >= keep_from);
        } else if self.missed && self.spare.len() < SPARE_LOW {
            for unused in [STALE_FRAMES, 1] {
                let Some(before) = self.frame.checked_sub(unused) else {
                    continue;
                };
                if self.spare.len() >= SPARE_LOW {
                    break;
                }
                let spare = &mut self.spare;
                self.map.retain(|_, e| {
                    if e.frame >= before {
                        return true;
                    }
                    spare.push(core::mem::replace(e, CachedLayout::empty()));
                    false
                });
            }
            while self.spare.len() < SPARE_LOW {
                self.spare.push(CachedLayout::with_room());
            }
        }
        self.missed = false;
        self.frame += 1;
    }
}

impl UiText {
    /// The editor's font stack (ADR 0036) and an empty atlas: Noto Sans for text, JetBrains
    /// Mono for the arrows and symbols Noto Sans lacks, Symbols Nerd Font for icons.
    pub fn new() -> Result<Self, FontError> {
        let mut fonts = Fonts::new();
        let text = fonts.load(engine_text::fonts::NOTO_SANS, 0)?;
        let symbols = fonts.load(engine_text::fonts::JETBRAINS_MONO, 0)?;
        let icons = fonts.load(engine_text::fonts::NERD_SYMBOLS, 0)?;
        Ok(Self {
            fonts,
            stack: vec![text, symbols, icons],
            mono: vec![symbols, text, icons],
            layout: TextLayout::new(),
            cache: LayoutCache::new(),
            atlas: GlyphAtlas::new(ATLAS_SIZE, ATLAS_SIZE),
        })
    }

    /// How many strings were laid out (cache misses) since creation.
    pub fn layouts_made(&self) -> u64 {
        self.cache.layouts
    }

    /// How many laid-out strings are kept.
    pub fn cached_layouts(&self) -> usize {
        self.cache.map.len()
    }

    /// Ends a frame of the laid-out strings, as painting one does: those long out of use
    /// are laid out into again.
    pub fn end_frame(&mut self) {
        self.cache.end_frame();
    }
}

impl TextMeasure for UiText {
    fn measure(&mut self, text: &str, size: f32, max_width: Option<f32>) -> [f32; 2] {
        let e = self.cache.get(
            (&mut self.fonts, &self.stack, &mut self.layout),
            text,
            size,
            max_width,
            false,
        );
        [e.extent[0].ceil(), e.extent[1].ceil()]
    }

    fn measure_mono(&mut self, text: &str, size: f32) -> [f32; 2] {
        let e = self.cache.get(
            (&mut self.fonts, &self.mono, &mut self.layout),
            text,
            size,
            None,
            true,
        );
        [e.extent[0].ceil(), e.extent[1].ceil()]
    }
}

/// What [`paint`] produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaintStats {
    /// Rectangle and outline quads.
    pub rects: u32,
    /// Glyph quads.
    pub glyphs: u32,
    /// Clip changes.
    pub clips: u32,
    /// Pictures.
    pub images: u32,
}

/// Appends `frame`'s commands to `canvas` (layer 0, in order), in physical pixels: the UI
/// is laid out in logical pixels and `scale` is the number of physical pixels per logical
/// one (the window's DPI / 96). Text is laid out and rasterized at its physical size, so
/// it stays sharp at any scale. Glyphs missing from the atlas are rendered into it: call
/// [`GlyphTexture::sync`] afterwards, before the pass that draws the canvas. Leaves the
/// canvas without a clip.
pub fn paint(
    frame: &Frame<'_>,
    text: &mut UiText,
    glyphs: &GlyphTexture,
    canvas: &mut Canvas,
    scale: f32,
) -> PaintStats {
    engine_profiler::zone!("editor::paint");
    let mut stats = PaintStats::default();
    let order = Order::layer(0);
    for command in frame.commands {
        match command {
            DrawCmd::Rect {
                rect,
                fill,
                border,
                radius,
            } => {
                let size = Vec2::new(rect.size[0], rect.size[1]) * scale;
                if size.x <= 0.0 || size.y <= 0.0 {
                    continue;
                }
                let center = Vec2::new(rect.pos[0], rect.pos[1]) * scale + size * 0.5;
                let radius = radius * scale;
                if let Some(fill) = fill {
                    let quad = if radius > 0.0 {
                        Quad::rounded_rect(center, size, radius)
                    } else {
                        Quad::solid(center, size)
                    };
                    canvas.draw(&quad.color(Vec4::from_array(*fill)).order(order));
                    stats.rects += 1;
                }
                if let Some((color, width)) = border {
                    canvas.draw(
                        &Quad::rounded_rect(center, size, radius)
                            .outline(width * scale)
                            .color(Vec4::from_array(*color))
                            .order(order),
                    );
                    stats.rects += 1;
                }
            }
            DrawCmd::Text {
                rect,
                range,
                size,
                color,
                wrap,
                mono,
            } => {
                let Some(string) = frame.text.get(range.clone()) else {
                    continue;
                };
                let size = size * scale;
                let stack = if *mono { &text.mono } else { &text.stack };
                let laid_out = text.cache.get(
                    (&mut text.fonts, stack, &mut text.layout),
                    string,
                    size,
                    wrap.then_some(rect.size[0] * scale),
                    *mono,
                );
                let origin = Vec2::new(rect.pos[0], rect.pos[1]) * scale;
                stats.glyphs += draw_glyphs(
                    canvas,
                    &laid_out.glyphs,
                    &text.fonts,
                    &mut text.atlas,
                    glyphs,
                    origin,
                    size,
                    Vec4::from_array(*color),
                    order,
                ) as u32;
            }
            DrawCmd::Line { a, b, width, color } => {
                canvas.draw(
                    &Quad::line(
                        Vec2::from(*a) * scale,
                        Vec2::from(*b) * scale,
                        width * scale,
                    )
                    .color(Vec4::from_array(*color))
                    .order(order),
                );
                stats.rects += 1;
            }
            DrawCmd::Image { rect, image } => {
                let size = Vec2::new(rect.size[0], rect.size[1]) * scale;
                if size.x <= 0.0 || size.y <= 0.0 {
                    continue;
                }
                let center = Vec2::new(rect.pos[0], rect.pos[1]) * scale + size * 0.5;
                let mut quad = Quad::sprite(image.texture, center, size).order(order);
                quad.uv = image.uv;
                canvas.draw(&quad);
                stats.images += 1;
            }
            DrawCmd::Clip(clip) => {
                canvas.set_clip(clip.map(|r| {
                    let (x0, y0) = ((r.pos[0] * scale).floor(), (r.pos[1] * scale).floor());
                    let (x1, y1) = (
                        ((r.pos[0] + r.size[0]) * scale).ceil(),
                        ((r.pos[1] + r.size[1]) * scale).ceil(),
                    );
                    ClipRect {
                        x: x0 as i32,
                        y: y0 as i32,
                        width: (x1 - x0).max(0.0) as u32,
                        height: (y1 - y0).max(0.0) as u32,
                    }
                }));
                stats.clips += 1;
            }
        }
    }
    canvas.set_clip(None);
    text.cache.end_frame();
    stats
}
