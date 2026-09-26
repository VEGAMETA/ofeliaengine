//! Text drawing: an `engine_text` glyph atlas mirrored in a GPU texture, and glyph quads.
//!
//! The atlas is an `R8_UNORM` coverage texture sampled through a `(1, 1, 1, R)` swizzle,
//! so glyphs draw with the ordinary sprite shader: white with coverage in alpha, tinted by
//! the quad color. Glyphs are placed on whole pixels (their quarter-pixel offset is baked
//! into the atlas entry) and sampled with nearest filtering: exact coverage, no blur.

use crate::canvas::{Canvas, Order, Quad};
use core::cell::Cell;
use engine_core::math::{Vec2, Vec4};
use engine_render::graph::FrameGraph;
use engine_render::texture::{GpuTexture, UploadError};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{Allocator, Bindless, Device, RhiError, SamplerKind};
use engine_text::{Fonts, GlyphAtlas, GlyphKey, TextLayout};

/// The GPU side of a [`GlyphAtlas`].
#[derive(Debug)]
pub struct GlyphTexture {
    texture: GpuTexture,
    /// Atlas generation last uploaded in full; `None` before the first upload.
    uploaded: Cell<Option<u64>>,
}

impl GlyphTexture {
    /// A texture matching `atlas`'s size.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        atlas: &GlyphAtlas,
    ) -> Result<Self, RhiError> {
        let (width, height) = atlas.size();
        let one = vk::ComponentSwizzle::ONE;
        let texture = GpuTexture::with_swizzle(
            device,
            allocator,
            bindless,
            vk::Format::R8_UNORM,
            width,
            height,
            vk::ComponentMapping {
                r: one,
                g: one,
                b: one,
                a: vk::ComponentSwizzle::R,
            },
        )?;
        Ok(Self {
            texture,
            uploaded: Cell::new(None),
        })
    }

    /// Bindless index to draw glyph quads with.
    pub fn index(&self) -> u32 {
        self.texture.sampled_index()
    }

    /// The texture (to destroy it at shutdown).
    pub fn texture_mut(&mut self) -> &mut GpuTexture {
        &mut self.texture
    }

    /// Declares the upload of what changed in `atlas` since the last call: the whole atlas
    /// after it was cleared (or the first time), otherwise the band of rows that changed.
    /// Call after drawing this frame's text, before the passes that sample it.
    pub fn sync<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        atlas: &mut GlyphAtlas,
    ) -> Result<(), UploadError> {
        let generation = atlas.generation();
        let dirty = atlas.take_dirty();
        if self.uploaded.get() != Some(generation) {
            self.texture.upload(fg, ring, atlas.pixels())?;
            self.uploaded.set(Some(generation));
            return Ok(());
        }
        let Some(rect) = dirty else {
            return Ok(());
        };
        // Whole rows are contiguous in the atlas: no copy on the CPU side.
        let (width, _) = atlas.size();
        let rows =
            &atlas.pixels()[(rect.y * width) as usize..((rect.y + rect.height) * width) as usize];
        self.texture
            .upload_region(fg, ring, (0, rect.y, width, rect.height), rows)
    }
}

/// Color of drawn text (linear, straight alpha).
pub type TextColor = Vec4;

/// Draws `layout` with its top-left corner at `origin` (pixel space: y down, one unit per
/// pixel) at `size` pixels (the size it was laid out with). Glyphs are rendered into
/// `atlas` on first use; call [`GlyphTexture::sync`] afterwards. Returns the number of
/// glyph quads.
#[allow(clippy::too_many_arguments)]
pub fn draw_text(
    canvas: &mut Canvas,
    layout: &TextLayout,
    fonts: &Fonts,
    atlas: &mut GlyphAtlas,
    texture: &GlyphTexture,
    origin: Vec2,
    size: f32,
    color: TextColor,
    order: Order,
) -> usize {
    let (aw, ah) = atlas.size();
    let (inv_w, inv_h) = (1.0 / aw as f32, 1.0 / ah as f32);
    let mut drawn = 0;
    for g in layout.glyphs() {
        let (key, x) = GlyphKey::at(g.font, g.glyph, size, origin.x + g.x);
        let Some(a) = atlas.get(fonts, key) else {
            continue;
        };
        let left = (x + i32::from(a.left)) as f32;
        let top = (origin.y + g.y).round() - f32::from(a.top);
        let (w, h) = (f32::from(a.width), f32::from(a.height));
        let uv = [
            f32::from(a.x) * inv_w,
            f32::from(a.y) * inv_h,
            (f32::from(a.x) + w) * inv_w,
            (f32::from(a.y) + h) * inv_h,
        ];
        canvas.draw(
            &Quad::sprite(
                texture.index(),
                Vec2::new(left + w * 0.5, top + h * 0.5),
                Vec2::new(w, h),
            )
            .uv(uv)
            .color(color)
            .sampler(SamplerKind::NearestClamp)
            .order(order),
        );
        drawn += 1;
    }
    drawn
}
