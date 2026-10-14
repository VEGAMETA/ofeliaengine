//! The sun's cached cascades (ADR 0093): a view's shadow map kept across frames, a layer
//! per cascade, of which each frame draws only what changed (the planning is
//! [`crate::shadow_cache`]'s).

use super::*;
use crate::graph::{ImportedTexture, TextureState};
use crate::local_shadows::DrawDiff;
use crate::shadow::{Keep, ShadowSettings};
use crate::shadow_cache::{self, Held, NOTHING};

/// More moved boxes than this pending: every cascade is drawn whole (cheaper than
/// gathering them).
const MAX_MOVES: usize = 512;

/// What a view's cached cascades drew ([`CascadeCache::stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CascadeCacheStats {
    /// Frames that drew texels of any cascade.
    pub frames_drawn: u64,
    /// Cascades drawn whole: in the first frame, after the light turned, the camera jumped
    /// or the cascade's texels changed size, when what changed covered most of a cascade,
    /// and when the renderer could not tell what changed.
    pub whole: u64,
    /// Texels drawn in the last frame, over every cascade.
    pub last_texels: u64,
    /// Cascades whose pieces were drawn at once (several pieces with the meshlet path,
    /// where vertex shaders pick viewports): each meshlet once, into the pieces it reaches.
    pub together: u64,
}

/// Where a frame's casters came from, to tell how its changes follow the last frame's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CasterSource {
    /// A draw list, compared with the previous one draw by draw.
    Draws,
    /// A GPU scene: its upload's serial and a hash of its batches' meshes (and their
    /// shapes).
    Scene { serial: u64, meshes: u64 },
}

/// The casters the layers were drawn from, and what moved since.
#[derive(Debug, Default)]
struct Moves {
    /// What the last frame's casters came from, the renderer settings that change what
    /// casts, and the material table's revision.
    last: Option<(CasterSource, u64, u64)>,
    /// A draw list's previous draws.
    draws: DrawDiff,
    /// The world boxes of casters that moved, appeared or went (their old and new places)
    /// since the layers were last drawn.
    boxes: Vec<[Vec3; 2]>,
    /// One cascade's window rectangles of them (scratch).
    rects: Vec<[i32; 4]>,
}

/// A view's sun shadow map kept across frames (ADR 0093): a layer per cascade, each
/// addressed wrap-around so the camera's movement only exposes strips to draw. Give it to
/// [`ForwardView::shadow_cache`] every frame; a still camera over a still scene then draws
/// no shadows at all.
///
/// The cascades keep their texel size while it still covers the view (a quarter larger at
/// most), and their depth range is the scene's: what they hold stays valid from frame to
/// frame. Casters that move, appear or go (a draw list's changed draws, a scene upload's
/// changes, instances moving between ticks) draw their old and new places again. The
/// cascades are drawn whole in the first frame, when the light turns, when the camera
/// jumps or the texel size changes, when what changed covers most of one, and when the
/// renderer cannot tell what changed (another draw list or scene, edited materials or
/// meshes, many hundreds of changes). [`Self::invalidate`] forces it.
///
/// One per view: create it with the renderer's [`ShadowSettings`]
/// ([`ForwardRenderer::shadow_settings`]; a cache made with others is not used), free it
/// with [`Self::destroy`] once the GPU no longer uses it.
pub struct CascadeCache {
    device: engine_rhi::ash::Device,
    image: vk::Image,
    allocation: Option<Allocation>,
    /// The layers, sampled as an array.
    view: vk::ImageView,
    /// Each layer, rendered into on its own.
    layer_views: [vk::ImageView; MAX_CASCADES],
    sampled: u32,
    desc: TextureDesc,
    state: Cell<TextureState>,
    /// What each layer holds, and the reach they were split over.
    held: Cell<[Option<Held>; MAX_CASCADES]>,
    reach: Cell<Option<f32>>,
    /// Every cascade is drawn whole next (until one frame drew them all).
    stale: Cell<bool>,
    redraw_always: Cell<bool>,
    /// The casters the layers were drawn from, and what moved since.
    moves: RefCell<Moves>,
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
        // Each layer on its own: the cascades render one at a time (ADR 0093).
        let mut layer_views = [vk::ImageView::null(); MAX_CASCADES];
        for (layer, out) in layer_views.iter_mut().enumerate().take(layers as usize) {
            // SAFETY: a view of the image just created.
            let made = unsafe {
                device.raw().create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(settings.format)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::DEPTH,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: layer as u32,
                            layer_count: 1,
                        }),
                    None,
                )
            };
            match made {
                Ok(v) => *out = v,
                Err(e) => {
                    // SAFETY: never used by the GPU.
                    unsafe {
                        for v in layer_views.into_iter().chain([view]) {
                            if v != vk::ImageView::null() {
                                device.raw().destroy_image_view(v, None);
                            }
                        }
                        allocator.destroy_image(image, allocation);
                    }
                    return Err(RhiError::vk("vkCreateImageView")(e));
                }
            }
        }
        let Some(sampled) = bindless.add_sampled_image(view) else {
            // SAFETY: never used by the GPU.
            unsafe {
                for v in layer_views.into_iter().chain([view]) {
                    if v != vk::ImageView::null() {
                        device.raw().destroy_image_view(v, None);
                    }
                }
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
            layer_views,
            sampled,
            desc: TextureDesc::new_2d_array(settings.format, size, size, layers),
            state: Cell::new(TextureState::undefined()),
            held: Cell::new([None; MAX_CASCADES]),
            reach: Cell::new(None),
            stale: Cell::new(true),
            redraw_always: Cell::new(false),
            moves: RefCell::new(Moves::default()),
            imported: Cell::new(None),
            stats: Cell::new(CascadeCacheStats::default()),
        })
    }

    /// Draws every cascade whole in the next frame, fitted as a new view's are: after
    /// changes the renderer cannot see (a mesh's vertices rewritten in place), and after a
    /// camera cut (what the cascades kept from before is then forgotten, as a view's
    /// history is).
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
                for view in self.layer_views {
                    if view != vk::ImageView::null() {
                        self.device.destroy_image_view(view, None);
                    }
                }
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
    /// drawn with (none when they are drawn whole anyway: the fit is then what a new view
    /// gets), and the scene's depth range.
    pub(crate) fn keep(&self) -> Keep {
        if self.stale.get() {
            return Keep {
                cached: true,
                ..Keep::default()
            };
        }
        Keep {
            reach: self.reach.get(),
            radius: self.held.get().map(|h| h.map(|h| h.radius)),
            cached: true,
        }
    }

    /// Notes this frame's draw list (`mesh`: a mesh's object box and shape revision), the
    /// renderer settings that change what casts and the materials' revision: the changed
    /// draws' old and new places are drawn again; another list or other settings draw
    /// everything.
    pub(crate) fn note_draws(
        &self,
        draws: &[DrawItem],
        mesh: impl Fn(MeshId) -> (Vec3, Vec3, u32),
        settings: u64,
        materials: u64,
    ) {
        let mut m = self.moves.borrow_mut();
        let now = (CasterSource::Draws, settings, materials);
        let follows = m.last == Some(now);
        m.last = Some(now);
        m.draws.diff(draws, mesh);
        let Moves { draws, boxes, .. } = &mut *m;
        boxes.extend_from_slice(&draws.changes);
        if !follows || boxes.len() > MAX_MOVES {
            self.stale.set(true);
        }
    }

    /// Notes this frame's scene upload (`serial`, its batches' `meshes` hash, its
    /// `changes` and the boxes of its instances `moving` between ticks), the renderer
    /// settings that change what casts and the materials' revision: what changed is drawn
    /// again (the same upload again brings only its moving instances); a skipped upload or
    /// other meshes draw everything.
    pub(crate) fn note_scene(
        &self,
        serial: u64,
        meshes: u64,
        settings: u64,
        materials: u64,
        changes: &[[Vec3; 2]],
        moving: &[[Vec3; 2]],
    ) {
        let mut m = self.moves.borrow_mut();
        let (same, next) = match m.last {
            Some((
                CasterSource::Scene {
                    serial: a,
                    meshes: before,
                },
                s,
                mat,
            )) if before == meshes && s == settings && mat == materials => {
                (serial == a, serial == a + 1)
            }
            _ => (false, false),
        };
        m.last = Some((CasterSource::Scene { serial, meshes }, settings, materials));
        if next {
            m.boxes.extend_from_slice(changes);
        }
        m.boxes.extend_from_slice(moving);
        if !(same || next) || m.boxes.len() > MAX_MOVES {
            self.stale.set(true);
        }
    }

    /// This frame's plan for each cascade of `fit` (with its pieces drawn `together`, see
    /// [`shadow_cache::plan_with_changes`]), and what each layer holds once the plan is
    /// drawn (cascades beyond the fit's keep theirs).
    pub(crate) fn plan(
        &self,
        fit: &CascadeFit,
        together: bool,
    ) -> (
        [shadow_cache::Plan; MAX_CASCADES],
        [Option<Held>; MAX_CASCADES],
    ) {
        let whole = self.stale.get() || self.redraw_always.get();
        let held = self.held.get();
        let mut plans = [NOTHING; MAX_CASCADES];
        let mut now = held;
        let mut m = self.moves.borrow_mut();
        let Moves { boxes, rects, .. } = &mut *m;
        let light = fit.light_from_world;
        // The light view is a rotation: a box's extents there are the absolute rotation's
        // (Arvo).
        let abs = engine_core::math::Mat3::from_cols(
            light.x_axis.truncate().abs(),
            light.y_axis.truncate().abs(),
            light.z_axis.truncate().abs(),
        );
        for k in 0..fit.count.min(self.desc.layers as usize) {
            let mut h = Held::of(&fit.cascades[k], fit.light_from_world, self.desc.width);
            let before = if whole { None } else { held[k].as_ref() };
            rects.clear();
            if before.is_some() {
                for b in boxes.iter() {
                    let c = light.transform_point3((b[0] + b[1]) * 0.5);
                    let e = abs * ((b[1] - b[0]) * 0.5);
                    if let Some(r) = h.window_rect([c.x - e.x, c.y - e.y, c.x + e.x, c.y + e.y]) {
                        rects.push(r);
                    }
                }
            }
            plans[k] = shadow_cache::plan_with_changes(before, &mut h, rects, together);
            now[k] = Some(h);
        }
        (plans, now)
    }

    /// Records a frame that drew `plans` (of `count` cascades split over `reach`, of which
    /// `together` drew their pieces at once): the layers hold `now`.
    pub(crate) fn commit(
        &self,
        plans: &[shadow_cache::Plan; MAX_CASCADES],
        now: [Option<Held>; MAX_CASCADES],
        count: usize,
        reach: f32,
        together: u64,
    ) {
        self.held.set(now);
        if count > 0 {
            self.reach.set(Some(reach));
        }
        // What moved is drawn.
        self.moves.borrow_mut().boxes.clear();
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
        stats.together += together;
        self.stats.set(stats);
    }

    /// Records a frame that drew nothing (a still camera over a still scene, or what moved
    /// lay outside every cascade).
    pub(crate) fn idle(&self) {
        self.moves.borrow_mut().boxes.clear();
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
        fg.set_layer_views(texture, &self.layer_views[..self.desc.layers as usize]);
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

/// One step of the hashes of what the cascades are drawn from (a scene's meshes, the
/// renderer's settings): FxHash's. Each step is a bijection of the hash for a given word,
/// so lists that differ in one word always hash apart.
#[inline]
pub(crate) fn mix(hash: u64, word: u64) -> u64 {
    (hash.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95)
}
