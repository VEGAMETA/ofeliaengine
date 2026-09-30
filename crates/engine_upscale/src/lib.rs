//! Upscaling and temporal antialiasing (CLAUDE.md §6, ADR 0043).
//!
//! [`Taau`] is the engine's own temporal antialiasing and upsampling: it accumulates the
//! forward renderer's jittered frames, following their motion vectors, into an image at
//! output resolution (the same as the render resolution for plain TAA, larger for
//! upsampling). It is always available: the quality reference and the fallback for the
//! vendor upscalers that come later behind the same inputs (color, depth, motion vectors,
//! jitter).
//!
//! ```text
//! let temporal = Temporal::new(&device, &mut allocator, 100_000)?;
//! temporal.set_jitter_phases(Taau::jitter_phases(render_size, output_size));
//! // every frame:
//! let passes = forward.add_passes(.., &ForwardView { temporal: Some(&temporal), .. })?;
//! let resolved = taau.add_pass(&mut fg, &device, &mut allocator, &bindless, TaauInput {
//!     color: hdr, depth, motion: passes.motion.unwrap() }, output_size, frame.number)?;
//! tonemap.add_pass(&mut fg, &bindless, resolved, backbuffer, exposure);
//! ```

use engine_render::compute::{ComputeShader, Dispatch};
use engine_render::forward::MotionOutput;
use engine_render::graph::{
    Access, FrameGraph, ImportedTexture, ShaderStages, Texture, TextureDesc, TextureState,
};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError, SamplerKind};

const RESOLVE: &[u8] = include_bytes!("../shaders/taau.resolve.spv");

/// Format of the history and of the resolved image (HDR, accumulated weight in alpha).
pub const HISTORY_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// What [`Taau::add_pass`] resolves: the forward renderer's jittered color and depth at
/// render resolution, and its motion output.
#[derive(Debug, Clone, Copy)]
pub struct TaauInput {
    /// HDR color, jittered.
    pub color: Texture,
    /// Depth, the size of `color`.
    pub depth: Texture,
    /// Motion vectors and jitter (`ForwardPasses::motion`).
    pub motion: MotionOutput,
}

/// `Push` in `taau.slang` (56 bytes).
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    color: u32,
    depth: u32,
    velocity: u32,
    history: u32,
    output: u32,
    linear_sampler: u32,
    input_size: [u32; 2],
    output_size: [u32; 2],
    jitter: [f32; 2],
    reset: u32,
    /// The reactive mask's bindless index, or [`NO_REACTIVE`].
    reactive: u32,
}

/// `NO_REACTIVE` in `taau.slang`.
const NO_REACTIVE: u32 = u32::MAX;

#[derive(Debug)]
struct HistoryImage {
    image: vk::Image,
    view: vk::ImageView,
    allocation: Option<Allocation>,
    sampled: u32,
    storage: u32,
    state: TextureState,
}

#[derive(Debug)]
struct History {
    images: [HistoryImage; 2],
    width: u32,
    height: u32,
    /// The image written last frame.
    last: usize,
    /// Whether `images[last]` holds anything.
    valid: bool,
}

/// How [`Taau`] fetches last frame's result (ADR 0043).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryFilter {
    /// One bilinear fetch: the fastest (the resolve about 15% cheaper), a little softer in
    /// motion (about 1.2 dB lower PSNR on a moving camera).
    #[default]
    Bilinear,
    /// A 5-tap Catmull-Rom filter (5 bilinear fetches): keeps moving images sharper.
    CatmullRom,
}

/// Temporal antialiasing and upsampling (see the crate documentation). One per view: it
/// keeps the view's history.
pub struct Taau {
    device: engine_rhi::ash::Device,
    /// Resolve pipelines by [`HistoryFilter`]: bilinear, Catmull-Rom.
    shaders: [ComputeShader<Push>; 2],
    filter: HistoryFilter,
    history: Option<History>,
    /// Histories replaced by a resize, freed once their frames are done.
    retired: Vec<(u64, History)>,
    frames_in_flight: u64,
}

impl core::fmt::Debug for Taau {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Taau")
            .field(
                "history",
                &self.history.as_ref().map(|h| (h.width, h.height)),
            )
            .finish_non_exhaustive()
    }
}

impl Taau {
    /// Creates the resolve pipeline. The history is created at the first
    /// [`Self::add_pass`], at its output size.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        frames_in_flight: usize,
    ) -> Result<Self, RhiError> {
        let shader = |catmull_rom: u32| {
            ComputeShader::specialized(device, bindless, RESOLVE, c"resolve", &[catmull_rom])
        };
        Ok(Self {
            device: device.raw().clone(),
            shaders: [shader(0)?, shader(1)?],
            filter: HistoryFilter::default(),
            history: None,
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
        })
    }

    /// Jitter phases for upsampling `input` to `output` (Halton(2, 3), FSR 2's rule):
    /// ⌈8 · s²⌉ for a scale `s` per axis, at least 8. Pass it to
    /// `Temporal::set_jitter_phases`.
    pub fn jitter_phases(input: (u32, u32), output: (u32, u32)) -> u32 {
        let scale = output.0 as f32 / input.0.max(1) as f32;
        ((8.0 * scale * scale).ceil() as u32).max(8)
    }

    /// Chooses how the history is fetched (default [`HistoryFilter::Bilinear`]); takes
    /// effect with the next [`Self::add_pass`].
    pub fn set_history_filter(&mut self, filter: HistoryFilter) {
        self.filter = filter;
    }

    /// The current [`HistoryFilter`].
    pub fn history_filter(&self) -> HistoryFilter {
        self.filter
    }

    /// Forgets the history: the next frame starts accumulating anew (after a camera cut,
    /// with `Temporal::reset`, which does the same through [`MotionOutput::reset`]).
    pub fn reset(&mut self) {
        if let Some(history) = &mut self.history {
            history.valid = false;
        }
    }

    /// Declares the resolve into an image of `output_size` and returns it: HDR, ready for
    /// tonemapping (read it before the next frame's resolve overwrites it). The frame must
    /// execute with `bindless`. `frame_number` bounds how long a resized history is kept.
    #[allow(clippy::too_many_arguments)]
    pub fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &'g Bindless,
        input: TaauInput,
        output_size: (u32, u32),
        frame_number: u64,
    ) -> Result<Texture, RhiError> {
        engine_profiler::zone!("upscale::Taau::add_pass");
        self.collect(allocator, bindless, frame_number);
        let (width, height) = (output_size.0.max(1), output_size.1.max(1));
        if self
            .history
            .as_ref()
            .is_none_or(|h| (h.width, h.height) != (width, height))
        {
            let new = History::new(device, allocator, bindless, width, height)?;
            if let Some(old) = self.history.replace(new) {
                self.retired.push((frame_number, old));
            }
        }
        let Some(history) = self.history.as_mut() else {
            unreachable!("created above");
        };
        let reset = input.motion.reset || !history.valid;
        let (previous, next) = (history.last, 1 - history.last);
        let desc = TextureDesc::new_2d(HISTORY_FORMAT, width, height);
        let final_state = TextureState::shader_read(ShaderStages::ALL);
        let mut import = |fg: &mut FrameGraph<'g>, k: usize, name| {
            let image = &mut history.images[k];
            let texture = fg.import_texture(
                name,
                ImportedTexture {
                    image: image.image,
                    view: image.view,
                    desc,
                    initial: image.state,
                    final_state: Some(final_state),
                    sampled_index: Some(image.sampled),
                    storage_index: Some(image.storage),
                },
            );
            image.state = final_state;
            texture
        };
        let history_texture = import(fg, previous, "taau history");
        let output = import(fg, next, "taau output");
        history.last = next;
        history.valid = true;

        let input_desc = fg.texture_desc(input.color);
        let shader = &self.shaders[match self.filter {
            HistoryFilter::Bilinear => 0,
            HistoryFilter::CatmullRom => 1,
        }];
        let jitter = input.motion.jitter;
        let velocity = input.motion.velocity;
        let reactive = input.motion.reactive;
        let mut pass = fg.add_compute("taau", bindless);
        if let Some(mask) = reactive {
            pass = pass.read(mask, Access::Sampled(ShaderStages::COMPUTE));
        }
        pass.read(input.color, Access::Sampled(ShaderStages::COMPUTE))
            .read(input.depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(velocity, Access::Sampled(ShaderStages::COMPUTE))
            .read(history_texture, Access::Sampled(ShaderStages::COMPUTE))
            .write(output, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(shader, Dispatch::Threads([width, height, 1]), move |ctx| {
                Some(Push {
                    color: ctx.sampled_index(input.color)?,
                    depth: ctx.sampled_index(input.depth)?,
                    velocity: ctx.sampled_index(velocity)?,
                    history: ctx.sampled_index(history_texture)?,
                    output: ctx.storage_index(output)?,
                    linear_sampler: Bindless::sampler(SamplerKind::LinearClamp),
                    input_size: [input_desc.width, input_desc.height],
                    output_size: [width, height],
                    jitter,
                    reset: u32::from(reset),
                    reactive: match reactive {
                        Some(mask) => ctx.sampled_index(mask)?,
                        None => NO_REACTIVE,
                    },
                })
            });
        Ok(output)
    }

    fn collect(&mut self, allocator: &mut Allocator, bindless: &Bindless, frame_number: u64) {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            let (frame, history) = self.retired.swap_remove(i);
            // SAFETY: the frames that used it have finished.
            unsafe { history.destroy(&self.device, allocator, bindless, frame) };
        }
    }

    /// Frees the history.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        for (frame, history) in self
            .retired
            .drain(..)
            .chain(self.history.take().map(|h| (0, h)))
        {
            // SAFETY: guaranteed by the caller.
            unsafe { history.destroy(&self.device, allocator, bindless, frame) };
        }
    }
}

impl History {
    fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        width: u32,
        height: u32,
    ) -> Result<Self, RhiError> {
        let mut image = || -> Result<HistoryImage, RhiError> {
            let (image, allocation) = allocator.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(HISTORY_FORMAT)
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
                        vk::ImageUsageFlags::SAMPLED
                            | vk::ImageUsageFlags::STORAGE
                            | vk::ImageUsageFlags::TRANSFER_SRC,
                    )
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                MemoryLocation::Device,
            )?;
            // SAFETY: a view of the image just created.
            let view = unsafe {
                device.raw().create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(HISTORY_FORMAT)
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
            let full = || RhiError::Vulkan {
                call: "bindless set full",
                result: vk::Result::ERROR_OUT_OF_POOL_MEMORY,
            };
            Ok(HistoryImage {
                image,
                view,
                allocation: Some(allocation),
                sampled: bindless.add_sampled_image(view).ok_or_else(full)?,
                storage: bindless.add_storage_image(view).ok_or_else(full)?,
                state: TextureState::undefined(),
            })
        };
        Ok(Self {
            images: [image()?, image()?],
            width,
            height,
            last: 0,
            valid: false,
        })
    }

    /// # Safety
    ///
    /// The GPU no longer uses the images after `frame`.
    unsafe fn destroy(
        self,
        device: &engine_rhi::ash::Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame: u64,
    ) {
        for mut image in self.images {
            bindless.remove_sampled_image(image.sampled, frame);
            bindless.remove_storage_image(image.storage, frame);
            // SAFETY: guaranteed by the caller.
            unsafe {
                device.destroy_image_view(image.view, None);
                if let Some(allocation) = image.allocation.take() {
                    allocator.destroy_image(image.image, allocation);
                }
            }
        }
    }
}
