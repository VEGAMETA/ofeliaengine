//! The sun's cached cascades (ADR 0093): a view's shadow map kept across frames, a layer
//! per cascade, of which each frame draws only what changed (the planning is
//! [`crate::shadow_cache`]'s).

use super::*;
use crate::graph::{ImportedTexture, TextureState};
use crate::shadow::{Keep, ShadowSettings};
use crate::shadow_cache::{self, Held, NOTHING};

/// What a view's cached cascades drew ([`CascadeCache::stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CascadeCacheStats {
    /// Frames that drew texels of any cascade.
    pub frames_drawn: u64,
    /// Cascades drawn whole: in the first frame, after the light turned, the camera jumped
    /// or the cascade's texels changed size, and whenever the casters changed.
    pub whole: u64,
    /// Texels drawn in the last frame, over every cascade.
    pub last_texels: u64,
}

/// The casters a cache's layers were drawn from, to find their changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Casters {
    /// A draw list: a hash of its meshes (and their shapes), materials and transforms.
    Draws(u64),
    /// A GPU scene: its upload's serial, a hash of its batches' meshes (and their shapes),
    /// whether the upload moved, added or removed instances, and whether instances move
    /// between ticks (on the GPU, every frame).
    Scene {
        serial: u64,
        meshes: u64,
        changed: bool,
        moving: bool,
    },
}

/// A view's sun shadow map kept across frames (ADR 0093): a layer per cascade, each
/// addressed wrap-around so the camera's movement only exposes strips to draw. Give it to
/// [`ForwardView::shadow_cache`] every frame; a still camera over a still scene then draws
/// no shadows at all.
///
/// The cascades keep their texel size while it still covers the view (a quarter larger at
/// most), and their depth range is the scene's: what they hold stays valid from frame to
/// frame. They are drawn whole again in the first frame, when the light turns, when the
/// camera jumps or the texel size changes, and (for now) whenever the casters change: a
/// draw list that is not the same as last frame's, a scene upload that moved anything,
/// changed materials or meshes. [`Self::invalidate`] forces it.
///
/// One per view: create it with the renderer's [`ShadowSettings`]
/// ([`ForwardRenderer::shadow_settings`]; a cache made with others is not used), free it
/// with [`Self::destroy`] once the GPU no longer uses it.
pub struct CascadeCache {
    device: engine_rhi::ash::Device,
    image: vk::Image,
    allocation: Option<Allocation>,
    view: vk::ImageView,
    sampled: u32,
    desc: TextureDesc,
    state: Cell<TextureState>,
    /// What each layer holds, and the reach they were split over.
    held: Cell<[Option<Held>; MAX_CASCADES]>,
    reach: Cell<Option<f32>>,
    /// Every cascade is drawn whole next (until one frame drew them all).
    stale: Cell<bool>,
    redraw_always: Cell<bool>,
    /// The casters the layers were drawn from, and the materials' revision.
    casters: Cell<Option<(Casters, u64)>>,
    /// The frame (graph serial) the layers were last imported into, and their handle.
    imported: Cell<Option<(u64, Texture)>>,
    stats: Cell<CascadeCacheStats>,
}

impl core::fmt::Debug for CascadeCache {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CascadeCache")
            .field("desc", &self.desc)
            .field("held", &self.held.get())
            .field("stale", &self.stale.get())
            .field("stats", &self.stats.get())
            .finish_non_exhaustive()
    }
}

impl CascadeCache {
    /// A cache for a renderer with `settings`: `settings.cascades` layers of
    /// `resolution²` texels in device memory (4 MiB each at the default 1024² of 32-bit
    /// depth).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        settings: &ShadowSettings,
    ) -> Result<Self, RhiError> {
        let size = settings.resolution.max(1);
        let layers = settings.cascade_count() as u32;
        let (image, allocation) = allocator.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(settings.format)
                .extent(vk::Extent3D {
                    width: size,
                    height: size,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(layers)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            MemoryLocation::Device,
        )?;
        // SAFETY: a view of the image just created.
        let view = unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D_ARRAY)
                    .format(settings.format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::DEPTH,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: layers,
                    }),
                None,
            )
        };
        let view = match view {
            Ok(view) => view,
            Err(e) => {
                // SAFETY: never used by the GPU.
                unsafe { allocator.destroy_image(image, allocation) };
                return Err(RhiError::vk("vkCreateImageView")(e));
            }
        };
        let Some(sampled) = bindless.add_sampled_image(view) else {
            // SAFETY: never used by the GPU.
            unsafe {
                device.raw().destroy_image_view(view, None);
                allocator.destroy_image(image, allocation);
            }
            return Err(RhiError::Vulkan {
                call: "bindless set full",
                result: vk::Result::ERROR_OUT_OF_POOL_MEMORY,
            });
        };
        Ok(Self {
            device: device.raw().clone(),
            image,
            allocation: Some(allocation),
            view,
            sampled,
            desc: TextureDesc::new_2d_array(settings.format, size, size, layers),
            state: Cell::new(TextureState::undefined()),
            held: Cell::new([None; MAX_CASCADES]),
            reach: Cell::new(None),
            stale: Cell::new(true),
            redraw_always: Cell::new(false),
            casters: Cell::new(None),
            imported: Cell::new(None),
            stats: Cell::new(CascadeCacheStats::default()),
        })
    }

    /// Draws every cascade whole in the next frame (after changes the renderer cannot see,
    /// such as a mesh's vertices rewritten in place).
    pub fn invalidate(&self) {
        self.stale.set(true);
    }

    /// Draws every cascade whole in every frame, with the cache's fit (the reference its
    /// cached frames are compared with; measurements).
    pub fn set_redraw_always(&self, on: bool) {
        self.redraw_always.set(on);
    }

    /// What the cascades drew so far.
    pub fn stats(&self) -> CascadeCacheStats {
        self.stats.get()
    }

    /// Bytes of device memory the layers take.
    pub fn memory_bytes(&self) -> u64 {
        let texel = match self.desc.format {
            vk::Format::D16_UNORM => 2,
            vk::Format::D32_SFLOAT_S8_UINT => 8,
            _ => 4,
        };
        u64::from(self.desc.width)
            * u64::from(self.desc.height)
            * u64::from(self.desc.layers)
            * texel
    }

    /// Frees the layers.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        if let Some(allocation) = self.allocation.take() {
            bindless.remove_sampled_image(self.sampled, 0);
            // SAFETY: guaranteed by the caller.
            unsafe {
                self.device.destroy_image_view(self.view, None);
                allocator.destroy_image(self.image, allocation);
            }
        }
    }

    /// Whether the layers suit a renderer with `settings`.
    pub(crate) fn fits(&self, settings: &ShadowSettings) -> bool {
        self.allocation.is_some()
            && self.desc.format == settings.format
            && self.desc.width == settings.resolution.max(1)
            && self.desc.layers == settings.cascade_count() as u32
    }

    /// How the fit holds the cascades still: the reach and the radii their layers were
    /// drawn with, and the scene's depth range.
    pub(crate) fn keep(&self) -> Keep {
        Keep {
            reach: self.reach.get(),
            radius: self.held.get().map(|h| h.map(|h| h.radius)),
            cached: true,
        }
    }

    /// Notes this frame's casters and the materials' revision: any change since the
    /// layers were drawn draws them whole.
    pub(crate) fn note_casters(&self, now: Casters, materials: u64) {
        let unchanged = match (self.casters.get(), now) {
            (Some((_, m)), _) if m != materials => false,
            (Some((Casters::Draws(a), _)), Casters::Draws(b)) => a == b,
            (
                Some((
                    Casters::Scene {
                        serial: a,
                        meshes: before,
                        ..
                    },
                    _,
                )),
                Casters::Scene {
                    serial: b,
                    meshes,
                    changed,
                    moving,
                },
            ) => {
                // The same upload again (its changes were seen), or the next one without
                // changes; instances moving between ticks move every frame.
                meshes == before && !moving && (b == a || (b == a + 1 && !changed))
            }
            _ => false,
        };
        if !unchanged {
            self.stale.set(true);
        }
        self.casters.set(Some((now, materials)));
    }

    /// This frame's plan for each cascade of `fit`, and what each layer holds once the
    /// plan is drawn (cascades beyond the fit's keep theirs).
    pub(crate) fn plan(
        &self,
        fit: &CascadeFit,
    ) -> (
        [shadow_cache::Plan; MAX_CASCADES],
        [Option<Held>; MAX_CASCADES],
    ) {
        let whole = self.stale.get() || self.redraw_always.get();
        let held = self.held.get();
        let mut plans = [NOTHING; MAX_CASCADES];
        let mut now = held;
        for k in 0..fit.count.min(self.desc.layers as usize) {
            let mut h = Held::of(&fit.cascades[k], fit.light_from_world, self.desc.width);
            let before = if whole { None } else { held[k].as_ref() };
            plans[k] = shadow_cache::plan(before, &mut h);
            now[k] = Some(h);
        }
        (plans, now)
    }

    /// Records a frame that drew `plans` (of `count` cascades split over `reach`): the
    /// layers hold `now`.
    pub(crate) fn commit(
        &self,
        plans: &[shadow_cache::Plan; MAX_CASCADES],
        now: [Option<Held>; MAX_CASCADES],
        count: usize,
        reach: f32,
    ) {
        self.held.set(now);
        if count > 0 {
            self.reach.set(Some(reach));
        }
        let plans = &plans[..count.min(MAX_CASCADES)];
        if !plans.is_empty() && plans.iter().all(|p| p.full) {
            self.stale.set(false);
        }
        let texels: u64 = plans
            .iter()
            .flat_map(|p| p.rects())
            .flat_map(|r| r.pieces())
            .map(|p| u64::from(p.scissor.extent.width) * u64::from(p.scissor.extent.height))
            .sum();
        let mut stats = self.stats.get();
        stats.frames_drawn += u64::from(texels > 0);
        stats.whole += plans.iter().filter(|p| p.full).count() as u64;
        stats.last_texels = texels;
        self.stats.set(stats);
    }

    /// Records a frame that drew nothing (a still camera over a still scene).
    pub(crate) fn idle(&self) {
        let mut stats = self.stats.get();
        stats.last_texels = 0;
        self.stats.set(stats);
    }

    /// The layers in this frame's graph (imported once per frame), left for the shading
    /// to sample.
    pub(crate) fn import(&self, fg: &mut FrameGraph<'_>) -> Texture {
        let serial = fg.serial();
        if let Some((s, texture)) = self.imported.get()
            && s == serial
        {
            return texture;
        }
        let final_state = TextureState::shader_read(ShaderStages::FRAGMENT);
        let texture = fg.import_texture(
            "cascade cache",
            ImportedTexture {
                image: self.image,
                view: self.view,
                desc: self.desc,
                initial: self.state.get(),
                final_state: Some(final_state),
                sampled_index: Some(self.sampled),
                storage_index: None,
            },
        );
        self.state.set(final_state);
        self.imported.set(Some((serial, texture)));
        texture
    }
}

/// The plans of cascades drawn whole every frame (a view without a cache, a probe's
/// capture), and what they hold.
pub(crate) fn whole_plans(
    fit: &CascadeFit,
    resolution: u32,
) -> (
    [shadow_cache::Plan; MAX_CASCADES],
    [Option<Held>; MAX_CASCADES],
) {
    let mut plans = [NOTHING; MAX_CASCADES];
    let mut held = [None; MAX_CASCADES];
    for k in 0..fit.count {
        let mut h = Held::of(&fit.cascades[k], fit.light_from_world, resolution);
        plans[k] = shadow_cache::plan(None, &mut h);
        held[k] = Some(h);
    }
    (plans, held)
}

/// One step of the hash of what the cascades are drawn from ([`Casters`]): FxHash's. Each
/// step is a bijection of the hash for a given word, so lists that differ in one word
/// always hash apart.
#[inline]
pub(crate) fn mix(hash: u64, word: u64) -> u64 {
    (hash.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95)
}
