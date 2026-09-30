//! Post-processing before tone mapping (ADR 0047, docs/research/post-processing.md): bloom
//! and automatic exposure, as compute passes of the frame graph whose results the tone
//! mapper reads ([`crate::post::Composite::bloom`], [`crate::post::Composite::auto_exposure`]).
//!
//! - [`Bloom`] halves the HDR image several times (a 13-tap filter, Karis' average on the
//!   first step) and enlarges it back with a 3 × 3 tent, each level mixing in the smaller
//!   ones (after Jimenez, SIGGRAPH 2014). Every filter is normalized, and the tone mapper
//!   mixes the result in by `intensity`: energy is kept.
//! - [`AutoExposure`] builds a histogram of log2 luminance, averages it between two
//!   percentiles, and adapts an exposure towards middle grey over time, all on the GPU.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::graph::{
    Access, Buffer, BufferDesc, BufferState, FrameGraph, ImportedBuffer, ShaderStages, Texture,
    TextureDesc,
};
use core::cell::Cell;
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError, SamplerKind};

const DOWNSAMPLE: &[u8] = include_bytes!("../shaders/postfx.bloomDownsample.spv");
const UPSAMPLE: &[u8] = include_bytes!("../shaders/postfx.bloomUpsample.spv");
const HISTOGRAM: &[u8] = include_bytes!("../shaders/postfx.exposureHistogram.spv");
const AVERAGE: &[u8] = include_bytes!("../shaders/postfx.exposureAverage.spv");

/// Format of the bloom levels.
const BLOOM_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// Most bloom levels (the first is half the image's size).
pub const MAX_BLOOM_LEVELS: u32 = 8;

/// `Push` in `postfx.slang` (96 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    histogram: u64,
    exposure: u64,
    source: u32,
    target: u32,
    sampler: u32,
    karis: u32,
    size: [u32; 2],
    scatter: f32,
    lower: u32,
    min_log: f32,
    log_range: f32,
    low_percent: f32,
    high_percent: f32,
    rate_up: f32,
    rate_down: f32,
    key: f32,
    min_exposure: f32,
    max_exposure: f32,
    reset: u32,
    step: u32,
    _pad: u32,
}

/// Bloom settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BloomSettings {
    /// How much of the image the bloom replaces when mixed in (0 none, 1 all): a lens's
    /// scatter is a few percent.
    pub intensity: f32,
    /// How much each level takes from the smaller ones (0..1): larger is wider.
    pub scatter: f32,
    /// Levels, the first half the image's size (at most [`MAX_BLOOM_LEVELS`]).
    pub levels: u32,
    /// Weigh the first downsample's 2 × 2 groups by 1 / (1 + luma) (Karis' average):
    /// single bright pixels (specular sparkles) stop flickering in the bloom, at the cost
    /// of blooming small bright sources less than their energy.
    pub karis_average: bool,
}

impl Default for BloomSettings {
    fn default() -> Self {
        Self {
            intensity: 0.04,
            scatter: 0.7,
            levels: 6,
            karis_average: true,
        }
    }
}

/// What [`Bloom::add_passes`] produced, for [`crate::post::Composite::bloom`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BloomOutput {
    /// The bloom, half the image's size.
    pub texture: Texture,
    /// How much of the image it replaces.
    pub intensity: f32,
}

/// Bloom (see the module documentation).
#[derive(Debug)]
pub struct Bloom {
    downsample: ComputeShader<Push>,
    upsample: ComputeShader<Push>,
    settings: Cell<BloomSettings>,
}

impl Bloom {
    /// Creates the pipelines.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        settings: BloomSettings,
    ) -> Result<Self, RhiError> {
        let [downsample, upsample] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(DOWNSAMPLE, c"bloomDownsample", &[])?,
                ComputeSource::new(UPSAMPLE, c"bloomUpsample", &[])?,
            ],
        )?;
        Ok(Self {
            downsample,
            upsample,
            settings: Cell::new(settings),
        })
    }

    /// Changes the settings.
    pub fn set_settings(&self, settings: BloomSettings) {
        self.settings.set(settings);
    }

    /// The settings.
    pub fn settings(&self) -> BloomSettings {
        self.settings.get()
    }

    /// Declares the bloom of `hdr`. The frame must execute with `bindless`.
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        hdr: Texture,
    ) -> BloomOutput {
        engine_profiler::zone!("render::Bloom::add_passes");
        let settings = self.settings.get();
        let desc = fg.texture_desc(hdr);
        let sampler = Bindless::sampler(SamplerKind::LinearClamp);
        let scatter = settings.scatter.clamp(0.0, 1.0);
        // Level sizes: halved until either side would drop below 2 pixels.
        let mut sizes = [[0u32; 2]; MAX_BLOOM_LEVELS as usize];
        let mut levels = 0usize;
        let (mut w, mut h) = (desc.width, desc.height);
        while levels < settings.levels.clamp(1, MAX_BLOOM_LEVELS) as usize && w >= 4 && h >= 4 {
            (w, h) = (w.div_ceil(2), h.div_ceil(2));
            sizes[levels] = [w, h];
            levels += 1;
        }
        const NAMES: [&str; MAX_BLOOM_LEVELS as usize] = [
            "bloom 1", "bloom 2", "bloom 3", "bloom 4", "bloom 5", "bloom 6", "bloom 7", "bloom 8",
        ];
        let mut down = [hdr; MAX_BLOOM_LEVELS as usize];
        let mut previous = hdr;
        for level in 0..levels.max(1) {
            let [w, h] = if levels == 0 {
                [
                    desc.width.div_ceil(2).max(1),
                    desc.height.div_ceil(2).max(1),
                ]
            } else {
                sizes[level]
            };
            let target = fg.create_texture(NAMES[level], TextureDesc::new_2d(BLOOM_FORMAT, w, h));
            let source = previous;
            fg.add_compute("bloom downsample", bindless)
                .read(source, Access::Sampled(ShaderStages::COMPUTE))
                .write(target, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(&self.downsample, Dispatch::Threads([w, h, 1]), move |ctx| {
                    Some(Push {
                        source: ctx.sampled_index(source)?,
                        target: ctx.storage_index(target)?,
                        sampler,
                        karis: u32::from(level == 0 && settings.karis_average),
                        size: [w, h],
                        ..Push::default()
                    })
                });
            down[level] = target;
            previous = target;
        }
        // Back up: each level mixes its own downsample with the enlarged smaller levels.
        let mut up = previous;
        for level in (0..levels.saturating_sub(1)).rev() {
            let [w, h] = sizes[level];
            let target = fg.create_texture("bloom up", TextureDesc::new_2d(BLOOM_FORMAT, w, h));
            let (own, lower) = (down[level], up);
            fg.add_compute("bloom upsample", bindless)
                .read(own, Access::Sampled(ShaderStages::COMPUTE))
                .read(lower, Access::Sampled(ShaderStages::COMPUTE))
                .write(target, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(&self.upsample, Dispatch::Threads([w, h, 1]), move |ctx| {
                    Some(Push {
                        source: ctx.sampled_index(own)?,
                        lower: ctx.sampled_index(lower)?,
                        target: ctx.storage_index(target)?,
                        sampler,
                        size: [w, h],
                        scatter,
                        ..Push::default()
                    })
                });
            up = target;
        }
        BloomOutput {
            texture: up,
            intensity: settings.intensity.clamp(0.0, 1.0),
        }
    }
}

/// Automatic exposure settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExposureSettings {
    /// Added to the automatic exposure, in EV (+1 twice as bright).
    pub compensation: f32,
    /// The average is taken over the pixels between these fractions of the histogram:
    /// the darkest and brightest pixels (the sun, black borders) do not move it.
    pub low_percent: f32,
    /// See `low_percent`.
    pub high_percent: f32,
    /// How fast the exposure rises (entering the dark), in EV per second.
    pub speed_up: f32,
    /// How fast it falls (entering the light), in EV per second.
    pub speed_down: f32,
    /// The exposure's range, in EV (log2 of the multiplier).
    pub min_exposure: f32,
    /// See `min_exposure`.
    pub max_exposure: f32,
    /// Luminance range of the histogram, log2 (below counts as the darkest bin, above
    /// as the brightest).
    pub min_log_luminance: f32,
    /// See `min_log_luminance`.
    pub max_log_luminance: f32,
}

impl Default for ExposureSettings {
    fn default() -> Self {
        Self {
            compensation: 0.0,
            low_percent: 0.1,
            high_percent: 0.9,
            speed_up: 1.5,
            speed_down: 3.0,
            min_exposure: -10.0,
            max_exposure: 10.0,
            min_log_luminance: -12.0,
            max_log_luminance: 12.0,
        }
    }
}

/// The exposure multiplier's target: the average luminance goes to middle grey.
const KEY: f32 = 0.18;
/// The histogram reads every `HISTOGRAM_STEP`-th pixel along each axis.
const HISTOGRAM_STEP: u32 = 2;

/// Automatic exposure (see the module documentation). One per view: it keeps the view's
/// adapted exposure.
pub struct AutoExposure {
    histogram: ComputeShader<Push>,
    average: ComputeShader<Push>,
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    state: Cell<BufferState>,
    settings: Cell<ExposureSettings>,
    reset: Cell<bool>,
}

impl core::fmt::Debug for AutoExposure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AutoExposure")
            .field("settings", &self.settings.get())
            .finish_non_exhaustive()
    }
}

impl AutoExposure {
    /// Creates the pipelines and the exposure's buffer.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        settings: ExposureSettings,
    ) -> Result<Self, RhiError> {
        let [histogram, average] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(HISTOGRAM, c"exposureHistogram", &[])?,
                ComputeSource::new(AVERAGE, c"exposureAverage", &[])?,
            ],
        )?;
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(16).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                    | vk::BufferUsageFlags::TRANSFER_SRC,
            ),
            MemoryLocation::Device,
        )?;
        // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
        let address = unsafe {
            device
                .raw()
                .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
        };
        Ok(Self {
            histogram,
            average,
            buffer,
            allocation: Some(allocation),
            address,
            state: Cell::new(BufferState::idle()),
            settings: Cell::new(settings),
            reset: Cell::new(true),
        })
    }

    /// Changes the settings.
    pub fn set_settings(&self, settings: ExposureSettings) {
        self.settings.set(settings);
    }

    /// The settings.
    pub fn settings(&self) -> ExposureSettings {
        self.settings.get()
    }

    /// The next frame jumps to its exposure instead of adapting (after a camera cut).
    pub fn reset(&self) {
        self.reset.set(true);
    }

    /// Declares the passes that measure `hdr` and adapt the exposure by `dt` seconds;
    /// returns the exposure's buffer for [`crate::post::Composite::auto_exposure`]. The
    /// frame must execute with `bindless`.
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        hdr: Texture,
        dt: f32,
    ) -> Buffer {
        engine_profiler::zone!("render::AutoExposure::add_passes");
        let settings = self.settings.get();
        let desc = fg.texture_desc(hdr);
        let read = BufferState {
            stages: vk::PipelineStageFlags2::FRAGMENT_SHADER
                | vk::PipelineStageFlags2::COMPUTE_SHADER,
            access: vk::AccessFlags2::SHADER_STORAGE_READ,
        };
        let exposure = fg.import_buffer(
            "exposure",
            ImportedBuffer {
                buffer: self.buffer,
                size: 16,
                address: self.address,
                initial: self.state.get(),
                final_state: Some(read),
            },
        );
        self.state.set(read);
        let histogram = fg.create_buffer("exposure histogram", BufferDesc { size: 256 * 4 });
        fg.add_pass("exposure histogram clear")
            .write(histogram, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording outside a rendering scope.
                unsafe {
                    ctx.device.cmd_fill_buffer(
                        ctx.cmd,
                        ctx.buffer(histogram),
                        0,
                        vk::WHOLE_SIZE,
                        0,
                    );
                }
            });
        let base = Push {
            size: [desc.width, desc.height],
            min_log: settings.min_log_luminance,
            log_range: (settings.max_log_luminance - settings.min_log_luminance).max(1e-3),
            low_percent: settings.low_percent.clamp(0.0, 1.0),
            high_percent: settings.high_percent.clamp(settings.low_percent, 1.0),
            rate_up: 1.0 - (-dt.max(0.0) * settings.speed_up).exp(),
            rate_down: 1.0 - (-dt.max(0.0) * settings.speed_down).exp(),
            key: KEY * settings.compensation.exp2(),
            min_exposure: settings.min_exposure,
            max_exposure: settings.max_exposure,
            reset: u32::from(self.reset.replace(false)),
            step: HISTOGRAM_STEP,
            ..Push::default()
        };
        // One thread per `HISTOGRAM_STEP` pixels along each axis.
        let threads = [
            desc.width.div_ceil(HISTOGRAM_STEP),
            desc.height.div_ceil(HISTOGRAM_STEP),
            1,
        ];
        fg.add_compute("exposure histogram", bindless)
            .read(hdr, Access::Sampled(ShaderStages::COMPUTE))
            .read(histogram, Access::StorageReadWrite(ShaderStages::COMPUTE))
            .dispatch(&self.histogram, Dispatch::Threads(threads), move |ctx| {
                Some(Push {
                    histogram: ctx.address(histogram),
                    source: ctx.sampled_index(hdr)?,
                    ..base
                })
            });
        fg.add_compute("exposure average", bindless)
            .read(histogram, Access::StorageRead(ShaderStages::COMPUTE))
            .read(exposure, Access::StorageReadWrite(ShaderStages::COMPUTE))
            .dispatch(&self.average, Dispatch::Groups([1, 1, 1]), move |ctx| {
                Some(Push {
                    histogram: ctx.address(histogram),
                    exposure: ctx.address(exposure),
                    ..base
                })
            });
        exposure
    }

    /// Frees the buffer.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}
