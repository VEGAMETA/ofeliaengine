//! Viewports (CLAUDE.md §6): first-class render targets with their own size, update rate
//! and post-processing, rendered through the one render graph.
//!
//! - **A viewport** owns a persistent color texture, registered in the bindless set, so any
//!   other viewport can show it: in 2D (a minimap sprite), in UI, or on a 3D surface.
//! - **Updates:** every frame, every n-th frame (a render tick), on request, or never.
//!   Between updates the texture keeps its last contents.
//! - **Order:** children (viewports whose `parent` is another viewport) render before
//!   their parents, so a parent can sample them in the same frame. Siblings keep creation
//!   order.
//! - **Placement:** viewports with a [`Placement`] are composited into the window by
//!   [`Viewports::composite`], giving split screen and picture-in-picture. Sizes can
//!   follow the placement or be fixed.
//! - **Pixel-perfect scaling** ([`Scaling::PixelPerfect`]): the largest integer scale that
//!   fits, centered, with nearest filtering.
//! - **Graph scopes:** each viewport's passes form a graph scope, so captures label them
//!   with the viewport's name and [`Viewports::gpu_time_us`] reports each viewport's GPU
//!   cost.
//!
//! The same system serves games and the editor: an editor viewport is a viewport.

use crate::graph::{
    Access, FrameGraph, ImportedTexture, LoadOp, RenderGraph, ShaderStages, StoreOp, Texture,
    TextureDesc, TextureState,
};
use crate::post::{Composite, HDR_FORMAT, Tonemap};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError, SamplerKind};

/// A viewport handle. Stale handles (of destroyed viewports) are ignored by every call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ViewportId {
    index: u16,
    generation: u16,
}

impl ViewportId {
    /// A number unique among live viewports: the graph scope id of its passes.
    pub fn raw(self) -> u32 {
        (u32::from(self.generation) << 16) | u32::from(self.index)
    }
}

/// How big a viewport's texture is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewportSize {
    /// A fixed size in pixels (pixel art, minimaps, render-to-texture).
    Fixed {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },
    /// The pixel size of its placement in the window times `scale` (below 1: rendered at a
    /// lower resolution and scaled up when composited). Without a placement: 1 × 1.
    Placement {
        /// Render scale.
        scale: f32,
    },
}

/// When a viewport renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMode {
    /// Every frame.
    Always,
    /// On frames where `frame_number % n == offset` (a render tick; offsets spread the work
    /// of several such viewports over frames).
    Every {
        /// Period in frames.
        n: u32,
        /// Phase.
        offset: u32,
    },
    /// Only after [`Viewports::request_update`] (portraits, baked views, captures).
    OnRequest,
    /// Never (keeps its last contents).
    Disabled,
}

/// How a placed viewport is scaled into its placement rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaling {
    /// Fill the rectangle, filtered linearly (bilinear upscaling of a lower render scale).
    Stretch,
    /// Fill the rectangle with nearest filtering: the cheapest upscale, blocky at
    /// non-integer ratios (for very weak hardware and retro looks).
    StretchNearest,
    /// The largest integer scale that fits, centered, nearest filtering; the rest of the
    /// rectangle shows the clear color.
    PixelPerfect,
}

/// Where a viewport appears in the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Left, top, width, height as fractions of the window (0..1).
    pub rect: [f32; 4],
    /// Composition order: higher is drawn later (on top).
    pub order: i32,
}

impl Placement {
    /// The whole window.
    pub const FULL: Self = Self {
        rect: [0.0, 0.0, 1.0, 1.0],
        order: 0,
    };
}

/// A viewport's settings. Change them through [`Viewports::desc_mut`]; size changes take
/// effect on the next [`Viewports::render`].
#[derive(Debug, Clone, PartialEq)]
pub struct ViewportDesc {
    /// Name, shown in GPU captures and profilers.
    pub name: String,
    /// Texture size.
    pub size: ViewportSize,
    /// Texture format ([`HDR_FORMAT`] by default).
    pub format: vk::Format,
    /// When it renders.
    pub update: UpdateMode,
    /// The viewport that shows this one (renders after it), if any.
    pub parent: Option<ViewportId>,
    /// Where it appears in the window, if it does.
    pub placement: Option<Placement>,
    /// How it is scaled into its placement.
    pub scaling: Scaling,
    /// Exposure applied when composited.
    pub exposure: f32,
    /// Tone map when composited (HDR contents); `false` copies.
    pub tonemap: bool,
    /// Clear color suggested to the content ([`ViewportFrame::load`]).
    pub clear: Option<[f32; 4]>,
}

impl Default for ViewportDesc {
    fn default() -> Self {
        Self {
            name: String::from("viewport"),
            size: ViewportSize::Placement { scale: 1.0 },
            format: HDR_FORMAT,
            update: UpdateMode::Always,
            parent: None,
            placement: Some(Placement::FULL),
            scaling: Scaling::Stretch,
            exposure: 1.0,
            tonemap: true,
            clear: Some([0.0, 0.0, 0.0, 1.0]),
        }
    }
}

/// A viewport being rendered this frame, passed to the content callback.
#[derive(Debug, Clone, Copy)]
pub struct ViewportFrame {
    /// Which viewport.
    pub id: ViewportId,
    /// Its color texture in this frame's graph: the content's render target.
    pub color: Texture,
    /// Texture width in pixels.
    pub width: u32,
    /// Texture height in pixels.
    pub height: u32,
    /// Load operation for the content's first pass: the desc's clear color, or
    /// `DontCare` when the content covers every pixel.
    pub load: LoadOp,
}

struct ViewportTexture {
    image: vk::Image,
    view: vk::ImageView,
    allocation: Allocation,
    sampled_index: u32,
    width: u32,
    height: u32,
}

struct ViewportState {
    desc: ViewportDesc,
    texture: Option<ViewportTexture>,
    /// The texture holds rendered contents (it is in `READ_ONLY_OPTIMAL`).
    initialized: bool,
    update_requested: bool,
    /// Graph handle in the frame being built, once imported.
    handle: Option<Texture>,
    handle_frame: u64,
}

struct Slot {
    generation: u16,
    state: Option<ViewportState>,
}

/// Stages that sample viewport textures: any shader.
const READERS: ShaderStages = ShaderStages::ALL;

/// Every viewport of an application.
pub struct Viewports {
    device: engine_rhi::ash::Device,
    slots: Vec<Slot>,
    frames_in_flight: u64,
    retired: Vec<(u64, ViewportTexture)>,
    order: Vec<(u32, u16)>,
    composite: Vec<(i32, u16)>,
}

impl core::fmt::Debug for Viewports {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Viewports")
            .field(
                "live",
                &self.slots.iter().filter(|s| s.state.is_some()).count(),
            )
            .field("retired", &self.retired.len())
            .finish()
    }
}

impl Viewports {
    /// No viewports yet. `frames_in_flight` bounds how long replaced textures are kept.
    pub fn new(device: &Device, frames_in_flight: usize) -> Self {
        Self {
            device: device.raw().clone(),
            slots: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            retired: Vec::new(),
            order: Vec::new(),
            composite: Vec::new(),
        }
    }

    /// Adds a viewport. Its texture is created on its first [`Self::render`].
    pub fn create(&mut self, desc: ViewportDesc) -> ViewportId {
        let state = ViewportState {
            desc,
            texture: None,
            initialized: false,
            update_requested: false,
            handle: None,
            handle_frame: 0,
        };
        if let Some(index) = self.slots.iter().position(|s| s.state.is_none()) {
            let slot = &mut self.slots[index];
            slot.state = Some(state);
            return ViewportId {
                index: index as u16,
                generation: slot.generation,
            };
        }
        self.slots.push(Slot {
            generation: 0,
            state: Some(state),
        });
        ViewportId {
            index: (self.slots.len() - 1) as u16,
            generation: 0,
        }
    }

    /// Removes a viewport. Its texture is freed once frames up to `frame_number` (the
    /// frame being built, or the last submitted one) have completed.
    pub fn destroy(&mut self, id: ViewportId, bindless: &Bindless, frame_number: u64) {
        if self.get(id).is_none() {
            return;
        }
        let slot = &mut self.slots[usize::from(id.index)];
        if let Some(texture) = slot.state.take().and_then(|s| s.texture) {
            bindless.remove_sampled_image(texture.sampled_index, frame_number);
            self.retired.push((frame_number, texture));
        }
        slot.generation = slot.generation.wrapping_add(1);
    }

    fn get(&self, id: ViewportId) -> Option<&ViewportState> {
        let slot = self.slots.get(usize::from(id.index))?;
        (slot.generation == id.generation)
            .then_some(slot.state.as_ref())
            .flatten()
    }

    fn get_mut(&mut self, id: ViewportId) -> Option<&mut ViewportState> {
        let slot = self.slots.get_mut(usize::from(id.index))?;
        if slot.generation == id.generation {
            slot.state.as_mut()
        } else {
            None
        }
    }

    /// The viewport's settings.
    pub fn desc(&self, id: ViewportId) -> Option<&ViewportDesc> {
        self.get(id).map(|s| &s.desc)
    }

    /// The viewport's settings, for changing them.
    pub fn desc_mut(&mut self, id: ViewportId) -> Option<&mut ViewportDesc> {
        self.get_mut(id).map(|s| &mut s.desc)
    }

    /// Renders the viewport on the next frame ([`UpdateMode::OnRequest`], or an extra
    /// update for any other mode).
    pub fn request_update(&mut self, id: ViewportId) {
        if let Some(state) = self.get_mut(id) {
            state.update_requested = true;
        }
    }

    /// Bindless sampled-image index of the viewport's texture (to show it in another
    /// viewport). `None` before its first render. The index changes when the texture is
    /// resized.
    pub fn texture_index(&self, id: ViewportId) -> Option<u32> {
        let state = self.get(id)?;
        if !state.initialized {
            return None;
        }
        state.texture.as_ref().map(|t| t.sampled_index)
    }

    /// Texture size in pixels (0 × 0 before its first render).
    pub fn extent(&self, id: ViewportId) -> (u32, u32) {
        self.get(id)
            .and_then(|s| s.texture.as_ref())
            .map_or((0, 0), |t| (t.width, t.height))
    }

    /// GPU time of the viewport's passes in the most recently completed frame of `graph`,
    /// in microseconds (0 if it did not render then or timestamps are unavailable).
    pub fn gpu_time_us(&self, graph: &RenderGraph, id: ViewportId) -> f32 {
        let scope = Some(id.raw());
        graph
            .gpu_pass_times()
            .iter()
            .filter(|t| t.scope == scope)
            .fold(0.0, |sum, t| sum + t.gpu_us)
    }

    fn target_extent(desc: &ViewportDesc, window: (u32, u32)) -> (u32, u32) {
        match desc.size {
            ViewportSize::Fixed { width, height } => (width.max(1), height.max(1)),
            ViewportSize::Placement { scale } => match desc.placement {
                Some(p) => {
                    let (w, h) = placement_pixels(p, window);
                    let scale = scale.clamp(0.01, 4.0);
                    (
                        ((w as f32 * scale).round() as u32).max(1),
                        ((h as f32 * scale).round() as u32).max(1),
                    )
                }
                None => (1, 1),
            },
        }
    }

    fn is_due(state: &ViewportState, frame_number: u64) -> bool {
        if state.update_requested {
            return true;
        }
        let due = match state.desc.update {
            UpdateMode::Always => true,
            UpdateMode::Every { n, offset } => {
                let n = u64::from(n.max(1));
                frame_number % n == u64::from(offset) % n
            }
            UpdateMode::OnRequest | UpdateMode::Disabled => false,
        };
        // A viewport renders at least once before it can be shown.
        due || (!state.initialized && state.desc.update != UpdateMode::Disabled)
    }

    /// Depth in the parent tree (roots 0), bounded against cycles.
    fn depth(&self, mut id: ViewportId) -> u32 {
        let mut depth = 0;
        while let Some(parent) = self.get(id).and_then(|s| s.desc.parent) {
            depth += 1;
            if depth > 64 {
                break;
            }
            id = parent;
        }
        depth
    }

    /// Declares every viewport due in frame `frame_number`, children first: creates or
    /// resizes its texture (`window` is the window size in pixels, for placement-sized
    /// viewports), then, inside the viewport's graph scope, calls `content` to add the
    /// passes that draw into [`ViewportFrame::color`], then makes the texture readable
    /// by any shader for the rest of the frame.
    ///
    /// The frame must execute with `bindless`. Replaced textures are freed once
    /// `frames_in_flight` frames have passed; the application still calls
    /// `bindless.collect` for their indices, as for every bindless removal.
    #[allow(clippy::too_many_arguments)]
    pub fn render<'g>(
        &mut self,
        fg: &mut FrameGraph<'g>,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        window: (u32, u32),
        frame_number: u64,
        mut content: impl FnMut(&mut FrameGraph<'g>, &ViewportFrame),
    ) -> Result<(), RhiError> {
        engine_profiler::zone!("render::viewports");
        self.collect_retired(allocator, frame_number);

        // Due viewports, deepest first, then in creation order.
        self.order.clear();
        for index in 0..self.slots.len() {
            let slot = &self.slots[index];
            let Some(state) = &slot.state else { continue };
            if !Self::is_due(state, frame_number) {
                continue;
            }
            let id = ViewportId {
                index: index as u16,
                generation: slot.generation,
            };
            self.order.push((self.depth(id), index as u16));
        }
        self.order
            .sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        for i in 0..self.order.len() {
            let index = usize::from(self.order[i].1);
            let generation = self.slots[index].generation;
            let id = ViewportId {
                index: index as u16,
                generation,
            };
            let Some(state) = self.slots[index].state.as_mut() else {
                continue;
            };
            let (width, height) = Self::target_extent(&state.desc, window);
            let resize = state
                .texture
                .as_ref()
                .is_none_or(|t| t.width != width || t.height != height);
            if resize {
                let texture = create_texture(
                    device,
                    allocator,
                    bindless,
                    state.desc.format,
                    width,
                    height,
                )?;
                if let Some(old) = state.texture.replace(texture) {
                    bindless.remove_sampled_image(old.sampled_index, frame_number);
                    self.retired.push((frame_number, old));
                }
                state.initialized = false;
            }
            let Some(texture) = state.texture.as_ref() else {
                continue;
            };
            let color = fg.import_texture(
                "viewport",
                ImportedTexture {
                    image: texture.image,
                    view: texture.view,
                    desc: TextureDesc::new_2d(state.desc.format, width, height),
                    initial: if state.initialized {
                        TextureState::shader_read(READERS)
                    } else {
                        TextureState::undefined()
                    },
                    final_state: Some(TextureState::shader_read(READERS)),
                    sampled_index: Some(texture.sampled_index),
                    storage_index: None,
                },
            );
            state.handle = Some(color);
            state.handle_frame = frame_number;
            state.update_requested = false;
            state.initialized = true;
            let frame = ViewportFrame {
                id,
                color,
                width,
                height,
                load: state.desc.clear.map_or(LoadOp::DontCare, LoadOp::Clear),
            };
            fg.begin_scope(id.raw(), &state.desc.name);
            content(fg, &frame);
            // Publish: later passes (other viewports, the composite) sample it through
            // bindless without declaring it, so transition it for every shader now.
            fg.add_pass("viewport publish")
                .read(color, Access::Sampled(READERS))
                .side_effects()
                .execute(|_| {});
            fg.end_scope();
        }
        Ok(())
    }

    /// Composites every placed viewport into `output` (the window's image, `extent` pixels)
    /// in placement order, through `tonemap`: tone mapped or copied, stretched or
    /// pixel-perfect. The first composite clears `output` to `clear` (letterbox color);
    /// with no placed viewports it is only cleared.
    #[allow(clippy::too_many_arguments)]
    pub fn composite<'g>(
        &mut self,
        fg: &mut FrameGraph<'g>,
        tonemap: &'g Tonemap,
        bindless: &'g Bindless,
        output: Texture,
        extent: (u32, u32),
        frame_number: u64,
        clear: [f32; 4],
    ) {
        self.composite.clear();
        for (index, slot) in self.slots.iter().enumerate() {
            if let Some(state) = &slot.state
                && let Some(placement) = state.desc.placement
                && state.initialized
            {
                self.composite.push((placement.order, index as u16));
            }
        }
        self.composite.sort_by_key(|&(order, index)| (order, index));
        if self.composite.is_empty() {
            fg.add_pass("clear")
                .color(output, LoadOp::Clear(clear), StoreOp::Store)
                .execute(|_| {});
            return;
        }
        for i in 0..self.composite.len() {
            let index = usize::from(self.composite[i].1);
            let Some(state) = self.slots[index].state.as_mut() else {
                continue;
            };
            let (Some(texture), Some(placement)) = (&state.texture, state.desc.placement) else {
                continue;
            };
            // Viewports not rendered this frame are imported for reading as they are.
            let source = match state.handle {
                Some(handle) if state.handle_frame == frame_number => handle,
                _ => {
                    let handle = fg.import_texture(
                        "viewport",
                        ImportedTexture {
                            image: texture.image,
                            view: texture.view,
                            desc: TextureDesc::new_2d(
                                state.desc.format,
                                texture.width,
                                texture.height,
                            ),
                            initial: TextureState::shader_read(READERS),
                            final_state: Some(TextureState::shader_read(READERS)),
                            sampled_index: Some(texture.sampled_index),
                            storage_index: None,
                        },
                    );
                    state.handle = Some(handle);
                    state.handle_frame = frame_number;
                    handle
                }
            };
            let (rect, sampler) = composite_rect(
                placement,
                state.desc.scaling,
                (texture.width, texture.height),
                extent,
            );
            tonemap.add_composite_pass(
                fg,
                bindless,
                source,
                output,
                Composite {
                    rect: Some(rect),
                    sampler,
                    exposure: state.desc.exposure,
                    tonemap: state.desc.tonemap,
                    load: if i == 0 {
                        LoadOp::Clear(clear)
                    } else {
                        LoadOp::Load
                    },
                },
            );
        }
    }

    fn collect_retired(&mut self, allocator: &mut Allocator, frame_number: u64) {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 <= completed {
                let (_, texture) = self.retired.swap_remove(i);
                // SAFETY: the frames that used it have completed.
                unsafe { destroy_texture(&self.device, allocator, texture) };
            } else {
                i += 1;
            }
        }
    }

    /// Frees every texture.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy_all(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        for (_, texture) in self.retired.drain(..) {
            // SAFETY: guaranteed by the caller.
            unsafe { destroy_texture(&self.device, allocator, texture) };
        }
        for slot in &mut self.slots {
            if let Some(texture) = slot.state.as_mut().and_then(|s| s.texture.take()) {
                bindless.remove_sampled_image(texture.sampled_index, 0);
                // SAFETY: guaranteed by the caller.
                unsafe { destroy_texture(&self.device, allocator, texture) };
            }
        }
    }
}

/// A placement's rectangle in window pixels (rounded to whole pixels).
fn placement_pixels(placement: Placement, window: (u32, u32)) -> (u32, u32) {
    let [x, y, w, h] = placement.rect;
    let (ww, wh) = (window.0 as f32, window.1 as f32);
    let x0 = (x * ww).round();
    let y0 = (y * wh).round();
    let x1 = ((x + w) * ww).round();
    let y1 = ((y + h) * wh).round();
    ((x1 - x0).max(0.0) as u32, (y1 - y0).max(0.0) as u32)
}

/// Where a viewport of `size` lands in an `extent` output, and the sampler to use.
pub fn composite_rect(
    placement: Placement,
    scaling: Scaling,
    size: (u32, u32),
    extent: (u32, u32),
) -> (vk::Rect2D, SamplerKind) {
    let [x, y, _, _] = placement.rect;
    let (w, h) = placement_pixels(placement, extent);
    let x0 = (x * extent.0 as f32).round() as i32;
    let y0 = (y * extent.1 as f32).round() as i32;
    match scaling {
        Scaling::Stretch => (
            vk::Rect2D {
                offset: vk::Offset2D { x: x0, y: y0 },
                extent: vk::Extent2D {
                    width: w,
                    height: h,
                },
            },
            SamplerKind::LinearClamp,
        ),
        Scaling::StretchNearest => (
            vk::Rect2D {
                offset: vk::Offset2D { x: x0, y: y0 },
                extent: vk::Extent2D {
                    width: w,
                    height: h,
                },
            },
            SamplerKind::NearestClamp,
        ),
        Scaling::PixelPerfect => {
            let scale = (w / size.0.max(1)).min(h / size.1.max(1)).max(1);
            let (sw, sh) = (size.0 * scale, size.1 * scale);
            (
                vk::Rect2D {
                    offset: vk::Offset2D {
                        x: x0 + (w as i32 - sw as i32) / 2,
                        y: y0 + (h as i32 - sh as i32) / 2,
                    },
                    extent: vk::Extent2D {
                        width: sw,
                        height: sh,
                    },
                },
                SamplerKind::NearestClamp,
            )
        }
    }
}

fn create_texture(
    device: &Device,
    allocator: &mut Allocator,
    bindless: &Bindless,
    format: vk::Format,
    width: u32,
    height: u32,
) -> Result<ViewportTexture, RhiError> {
    let (image, allocation) = allocator.create_image(
        &vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .initial_layout(vk::ImageLayout::UNDEFINED),
        MemoryLocation::Device,
    )?;
    let d = device.raw();
    // SAFETY: a view of the image just created.
    let view = unsafe {
        d.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                }),
            None,
        )
    };
    let view = match view {
        Ok(view) => view,
        Err(e) => {
            // SAFETY: never used.
            unsafe { allocator.destroy_image(image, allocation) };
            return Err(RhiError::vk("vkCreateImageView")(e));
        }
    };
    let Some(sampled_index) = bindless.add_sampled_image(view) else {
        // SAFETY: never used.
        unsafe {
            d.destroy_image_view(view, None);
            allocator.destroy_image(image, allocation);
        }
        return Err(RhiError::vk("bindless sampled-image slots")(
            vk::Result::ERROR_OUT_OF_POOL_MEMORY,
        ));
    };
    Ok(ViewportTexture {
        image,
        view,
        allocation,
        sampled_index,
        width,
        height,
    })
}

/// # Safety
///
/// The GPU no longer uses the texture.
unsafe fn destroy_texture(
    device: &engine_rhi::ash::Device,
    allocator: &mut Allocator,
    texture: ViewportTexture,
) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        device.destroy_image_view(texture.view, None);
        allocator.destroy_image(texture.image, texture.allocation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_perfect_uses_the_largest_integer_scale_centered() {
        let placement = Placement {
            rect: [0.5, 0.0, 0.5, 1.0],
            order: 0,
        };
        // Right half of 128 × 128 = 64 × 128; a 16 × 16 viewport scales by 4 (64 × 64),
        // centered vertically.
        let (rect, sampler) =
            composite_rect(placement, Scaling::PixelPerfect, (16, 16), (128, 128));
        assert_eq!(
            (
                rect.offset.x,
                rect.offset.y,
                rect.extent.width,
                rect.extent.height
            ),
            (64, 32, 64, 64)
        );
        assert_eq!(sampler, SamplerKind::NearestClamp);
        // 320 × 180 in 1920 × 1080: exactly 6×.
        let (rect, _) = composite_rect(
            Placement::FULL,
            Scaling::PixelPerfect,
            (320, 180),
            (1920, 1080),
        );
        assert_eq!(
            (rect.extent.width, rect.extent.height, rect.offset.x),
            (1920, 1080, 0)
        );
        // Too small a window still shows it at 1× (cropped by the scissor).
        let (rect, _) = composite_rect(
            Placement::FULL,
            Scaling::PixelPerfect,
            (320, 180),
            (200, 100),
        );
        assert_eq!((rect.extent.width, rect.extent.height), (320, 180));
    }

    #[test]
    fn stretch_nearest_fills_the_placement_with_nearest_filtering() {
        // A 1080p window showing a 0.5 render scale: 960 × 540 scaled to 1920 × 1080.
        let (rect, sampler) = composite_rect(
            Placement::FULL,
            Scaling::StretchNearest,
            (960, 540),
            (1920, 1080),
        );
        assert_eq!((rect.extent.width, rect.extent.height), (1920, 1080));
        assert_eq!(sampler, SamplerKind::NearestClamp);
    }

    #[test]
    fn stretch_fills_the_placement() {
        let placement = Placement {
            rect: [0.0, 0.5, 0.5, 0.5],
            order: 0,
        };
        let (rect, sampler) = composite_rect(placement, Scaling::Stretch, (10, 10), (1280, 720));
        assert_eq!(
            (
                rect.offset.x,
                rect.offset.y,
                rect.extent.width,
                rect.extent.height
            ),
            (0, 360, 640, 360)
        );
        assert_eq!(sampler, SamplerKind::LinearClamp);
    }

    #[test]
    fn update_modes() {
        let state = |update, initialized| ViewportState {
            desc: ViewportDesc {
                update,
                ..ViewportDesc::default()
            },
            texture: None,
            initialized,
            update_requested: false,
            handle: None,
            handle_frame: 0,
        };
        let due = |s: &ViewportState| (1..=6).filter(|&f| Viewports::is_due(s, f)).count();
        assert_eq!(due(&state(UpdateMode::Always, true)), 6);
        assert_eq!(due(&state(UpdateMode::Every { n: 3, offset: 1 }, true)), 2);
        assert_eq!(due(&state(UpdateMode::OnRequest, true)), 0);
        // Never rendered: renders once regardless, unless disabled.
        assert_eq!(due(&state(UpdateMode::OnRequest, false)), 6);
        assert_eq!(due(&state(UpdateMode::Disabled, false)), 0);
        let mut requested = state(UpdateMode::Disabled, true);
        requested.update_requested = true;
        assert!(Viewports::is_due(&requested, 1));
    }
}
