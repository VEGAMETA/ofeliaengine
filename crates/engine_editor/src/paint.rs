//! Painting `engine_ui` frames with `engine_render2d` and `engine_text`.
//!
//! - Rectangles become quads: plain ones are pixel-exact solid quads, rounded ones SDF
//!   shapes with antialiased edges, borders SDF outlines.
//! - Text becomes glyph quads from one shared [`GlyphAtlas`], laid out with the same fonts
//!   and widths the UI measured with ([`UiText`] is the UI's [`TextMeasure`]), so what is
//!   measured is what is drawn.
//! - Laid-out strings are kept while they stay on screen: a label is shaped once, not in
//!   every frame's layout and again in every paint.
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

/// Fonts, laid-out strings and the glyph atlas of the UI.
pub struct UiText {
    /// Loaded fonts.
    pub fonts: Fonts,
    /// The fonts text uses, in fallback order.
    stack: Vec<FontId>,
    layout: TextLayout,
    cache: LayoutCache,
    /// Rendered glyphs; mirror it on the GPU with [`GlyphTexture::sync`] after painting.
    pub atlas: GlyphAtlas,
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
    size: u32,
    /// Wrap width bits, `u32::MAX` for none.
    max_width: u32,
    glyphs: Vec<PositionedGlyph>,
    extent: [f32; 2],
    /// Frame of last use.
    frame: u64,
}

/// Layouts by a hash of (text, size, wrap width); an entry is only used if all three
/// match, so a hash collision costs a layout, never the wrong text.
#[derive(Default)]
struct LayoutCache {
    map: HashMap<u64, CachedLayout>,
    frame: u64,
    layouts: u64,
}

impl LayoutCache {
    /// The layout of `text`, laid out with `shaper` if it is not cached.
    fn get(
        &mut self,
        (fonts, stack, layout): (&mut Fonts, &[FontId], &mut TextLayout),
        text: &str,
        size: f32,
        max_width: Option<f32>,
    ) -> &CachedLayout {
        let (size_bits, width_bits) = (size.to_bits(), max_width.map_or(u32::MAX, f32::to_bits));
        let mut h = StateHasher::new();
        h.write(text.as_bytes());
        h.write_u64(u64::from(size_bits) | (u64::from(width_bits) << 32));
        let frame = self.frame;
        let lay_out = |fonts: &mut Fonts, layout: &mut TextLayout| {
            let style = TextStyle {
                size,
                max_width,
                ..TextStyle::default()
            };
            layout.layout(fonts, stack, text, &style);
            CachedLayout {
                text: text.to_owned(),
                size: size_bits,
                max_width: width_bits,
                glyphs: layout.glyphs().to_vec(),
                extent: [layout.width(), layout.height()],
                frame,
            }
        };
        let entry = match self.map.entry(h.finish()) {
            Entry::Occupied(o)
                if o.get().text == text
                    && o.get().size == size_bits
                    && o.get().max_width == width_bits =>
            {
                o.into_mut()
            }
            Entry::Occupied(mut o) => {
                self.layouts += 1;
                o.insert(lay_out(fonts, layout));
                o.into_mut()
            }
            Entry::Vacant(v) => {
                self.layouts += 1;
                v.insert(lay_out(fonts, layout))
            }
        };
        entry.frame = frame;
        entry
    }

    /// Ends a frame: drops what went unused this frame and the last, once the cache is big.
    fn end_frame(&mut self) {
        if self.map.len() > CACHE_SOFT_LIMIT {
            let keep_from = self.frame.saturating_sub(1);
            self.map.retain(|_, e| e.frame >= keep_from);
        }
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
            layout: TextLayout::new(),
            cache: LayoutCache::default(),
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
}

impl TextMeasure for UiText {
    fn measure(&mut self, text: &str, size: f32, max_width: Option<f32>) -> [f32; 2] {
        let e = self.cache.get(
            (&mut self.fonts, &self.stack, &mut self.layout),
            text,
            size,
            max_width,
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
}

/// Appends `frame`'s commands to `canvas` (layer 0, in order). Glyphs missing from the
/// atlas are rendered into it: call [`GlyphTexture::sync`] afterwards, before the pass that
/// draws the canvas. Leaves the canvas without a clip.
pub fn paint(
    frame: &Frame<'_>,
    text: &mut UiText,
    glyphs: &GlyphTexture,
    canvas: &mut Canvas,
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
                let size = Vec2::new(rect.size[0], rect.size[1]);
                if size.x <= 0.0 || size.y <= 0.0 {
                    continue;
                }
                let center = Vec2::new(rect.pos[0], rect.pos[1]) + size * 0.5;
                if let Some(fill) = fill {
                    let quad = if *radius > 0.0 {
                        Quad::rounded_rect(center, size, *radius)
                    } else {
                        Quad::solid(center, size)
                    };
                    canvas.draw(&quad.color(Vec4::from_array(*fill)).order(order));
                    stats.rects += 1;
                }
                if let Some((color, width)) = border {
                    canvas.draw(
                        &Quad::rounded_rect(center, size, *radius)
                            .outline(*width)
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
            } => {
                let Some(string) = frame.text.get(range.clone()) else {
                    continue;
                };
                let laid_out = text.cache.get(
                    (&mut text.fonts, &text.stack, &mut text.layout),
                    string,
                    *size,
                    wrap.then_some(rect.size[0]),
                );
                let origin = Vec2::new(rect.pos[0], rect.pos[1]);
                stats.glyphs += draw_glyphs(
                    canvas,
                    &laid_out.glyphs,
                    &text.fonts,
                    &mut text.atlas,
                    glyphs,
                    origin,
                    *size,
                    Vec4::from_array(*color),
                    order,
                ) as u32;
            }
            DrawCmd::Clip(clip) => {
                canvas.set_clip(clip.map(|r| {
                    let (x0, y0) = (r.pos[0].floor(), r.pos[1].floor());
                    let (x1, y1) = ((r.pos[0] + r.size[0]).ceil(), (r.pos[1] + r.size[1]).ceil());
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
