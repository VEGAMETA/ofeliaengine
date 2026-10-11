//! Global illumination from a volume of probes (ADR 0088, phase 1 of ADR 0084): a grid of
//! probes over the scene, each holding its irradiance over all directions and the
//! distances its rays reach, traced against the scene's acceleration structures
//! ([`crate::rays`]) and converged over a few updates at load (or after
//! [`ProbeGi::refresh`]), then kept: no tracing in later frames. A pixel's diffuse light
//! comes from the eight probes around its surface ([`ProbeGi::add_resolve_pass`]), with
//! Chebyshev visibility against their distances so light does not pass through walls.
//!
//! - **Updates** (`gi.slang`): each probe casts [`GiSettings::rays`] rays (spherical
//!   Fibonacci directions, turned at random every update); a hit is shaded with its
//!   material's albedo and emission, the sun (with a shadow ray) and, after the first
//!   update, the volume's own light (one more bounce per update); a miss sees the sky. The
//!   first [`GiSettings::bounces`] updates replace the probes' maps (the light gains a
//!   bounce each time, and the first two move probes out of geometry, the third turns off
//!   those that stay inside); the next [`GiSettings::samples`] are averaged.
//! - **Maps**: irradiance in 6 × 6 octahedral texels per probe (RGBA16F), the mean and
//!   mean square of hit distances in 14 × 14 (RG16F), each inside a one-texel border for
//!   bilinear filtering; a probe's offset from its rest position and its state in a
//!   buffer.
//! - A GPU without ray queries cannot have one: views keep the sky's light.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::environment::{Image, ImageKind};
use crate::gpu_scene::SceneFrame;
use crate::graph::{
    Access, Buffer, BufferDesc, BufferState, FrameGraph, ImportedBuffer, ShaderStages, Texture,
    TextureDesc,
};
use crate::material::MaterialFrame;
use crate::mesh::GeometryFrame;
use crate::rays::{RAY_MASK_MASKED, RAY_MASK_OPAQUE, RayFrame};
use crate::upload::UploadRing;
use core::cell::Cell;
use engine_core::math::{Mat3, Mat4, Quat, Vec3};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterKind, Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError, SamplerKind,
};

const TRACE: &[u8] = include_bytes!("../shaders/gi.giTrace.spv");
const IRRADIANCE: &[u8] = include_bytes!("../shaders/gi.giIrradiance.spv");
const MOMENTS: &[u8] = include_bytes!("../shaders/gi.giMoments.spv");
const RELOCATE: &[u8] = include_bytes!("../shaders/gi.giRelocate.spv");
const RESOLVE: &[u8] = include_bytes!("../shaders/gi.giResolve.spv");
const SAMPLE: &[u8] = include_bytes!("../shaders/gi.giSample.spv");

/// Texels per probe in the irradiance atlas (6 × 6 inside a border of one).
const IRRADIANCE_TILE: u32 = 8;
/// Texels per probe in the moments atlas (14 × 14 inside a border of one).
const MOMENTS_TILE: u32 = 16;
/// The most rays per probe in one update (`MAX_RAYS` in `gi.slang`).
pub const MAX_RAYS: u32 = 256;
/// Atlas sides are at most this many texels: probes along x times along y, and along z,
/// are at most this over [`MOMENTS_TILE`].
const MAX_ATLAS: u32 = 16_384;
/// The most probes in a volume.
pub const MAX_PROBES: u32 = 32_768;
/// Updates that move probes out of geometry (the next one turns off those still inside).
const RELOCATING_UPDATES: u32 = 2;

/// The format of the irradiance a resolve pass writes.
pub const GI_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// The probes' grid: probe `(x, y, z)` rests at `origin + (x, y, z) × spacing`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeGrid {
    /// The rest position of probe `(0, 0, 0)`.
    pub origin: Vec3,
    /// Distance between neighbouring probes along each axis.
    pub spacing: Vec3,
    /// Probes along each axis (at least 2).
    pub counts: [u32; 3],
}

impl ProbeGrid {
    /// Probes over the box `min..max`, at most `spacing` apart (the outer ones on its
    /// faces, at least two per axis), and at most `max_probes` of them (capped at
    /// [`MAX_PROBES`]; the spacing grows to fit).
    pub fn covering(min: Vec3, max: Vec3, spacing: f32, max_probes: u32) -> Self {
        let size = (max - min).max(Vec3::splat(1e-3));
        let max_probes = max_probes.clamp(8, MAX_PROBES);
        let mut step = spacing.max(1e-3);
        loop {
            let counts = ((size / step).ceil() + 1.0).max(Vec3::splat(2.0));
            let counts = [counts.x as u32, counts.y as u32, counts.z as u32];
            let fits = counts[0] * counts[1] * counts[2] <= max_probes
                && counts[0] * counts[1] * MOMENTS_TILE <= MAX_ATLAS
                && counts[2] * MOMENTS_TILE <= MAX_ATLAS;
            if fits {
                let spaces = Vec3::new(
                    (counts[0] - 1) as f32,
                    (counts[1] - 1) as f32,
                    (counts[2] - 1) as f32,
                );
                return Self {
                    origin: min,
                    spacing: size / spaces,
                    counts,
                };
            }
            step *= 1.1;
        }
    }

    /// Probes in the grid.
    pub fn probe_count(&self) -> u32 {
        self.counts[0] * self.counts[1] * self.counts[2]
    }

    /// The far corner: the rest position of the last probe.
    pub fn max(&self) -> Vec3 {
        self.origin
            + self.spacing
                * Vec3::new(
                    (self.counts[0] - 1) as f32,
                    (self.counts[1] - 1) as f32,
                    (self.counts[2] - 1) as f32,
                )
    }
}

/// How a [`ProbeGi`] converges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GiSettings {
    /// Rays per probe in each update (32 to [`MAX_RAYS`]).
    pub rays: u32,
    /// Updates whose result replaces the maps: each adds a bounce of light; the first two
    /// also move probes out of geometry (at least 3).
    pub bounces: u32,
    /// Updates averaged after them (more: less noise).
    pub samples: u32,
    /// Updates declared per frame while converging.
    pub updates_per_frame: u32,
    /// How far a surface point is moved before the probes are looked up, along its normal
    /// and towards the eye, as fractions of the smallest spacing.
    pub normal_bias: f32,
    /// See [`Self::normal_bias`].
    pub view_bias: f32,
    /// Pixels per resolved sample along each axis (1, 2 or 4): the resolve pass evaluates
    /// the probes once per `scale` × `scale` pixels and the shading takes each pixel's
    /// light from the samples around it whose surfaces are at its distance (diffuse light
    /// is smooth; the probes' cost is in their texture reads).
    pub resolve_scale: u32,
}

impl Default for GiSettings {
    fn default() -> Self {
        Self {
            rays: 128,
            bounces: 3,
            samples: 8,
            updates_per_frame: 1,
            normal_bias: 0.06,
            view_bias: 0.24,
            resolve_scale: 2,
        }
    }
}

impl GiSettings {
    /// The defaults for `device`: a resolve scale of 4 on integrated GPUs (on a two-CU
    /// Radeon, Sponza's resolve at 1920 × 1080 takes 4.2 ms at a scale of 2 and 1.7 ms
    /// at 4), 2 on discrete ones (an RTX 4060 Ti: 93 µs).
    pub fn for_device(device: &Device) -> Self {
        let resolve_scale = match device.adapter().kind {
            AdapterKind::Integrated | AdapterKind::Cpu => 4,
            _ => 2,
        };
        Self {
            resolve_scale,
            ..Self::default()
        }
    }

    fn updates(&self) -> u32 {
        self.bounces.max(3) + self.samples
    }
}

/// What a volume's rays see where they hit nothing.
#[derive(Debug, Clone, Copy)]
pub enum GiSky {
    /// The same radiance in every direction.
    Flat(Vec3),
    /// A radiance cube (in the frame's graph, its bindless index) times `intensity`, and
    /// the address of its nine irradiance harmonics (9 × 16 bytes).
    Cube {
        /// The cube.
        texture: Texture,
        /// Its bindless index.
        index: u32,
        /// Radiance multiplier.
        intensity: f32,
        /// Its irradiance harmonics.
        sh: vk::DeviceAddress,
    },
}

/// `GiVolume` in `gi.slang` (96 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct GiVolumeData {
    origin: [f32; 4],
    spacing: [f32; 4],
    inv_spacing: [f32; 4],
    counts: [u32; 4],
    atlas: [f32; 4],
    irradiance: u32,
    moments: u32,
    sampler: u32,
    view_bias: f32,
}

/// `TraceFrame` in `gi.slang` (224 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct TraceFrame {
    volume: GiVolumeData,
    rotation: [[f32; 4]; 3],
    sun_direction: [f32; 4],
    sun_color: [f32; 4],
    sky: [f32; 4],
    accel: u64,
    sky_cube: u32,
    rays: u32,
    mask: u32,
    bounce: u32,
    linear_sampler: u32,
    _pad0: u32,
}

/// `TracePush` in `gi.slang` (72 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct TracePush {
    frame: u64,
    probes: u64,
    results: u64,
    instances: u64,
    batches: u64,
    positions: u64,
    attributes: u64,
    indices: u64,
    materials: u64,
}

/// `BlendPush` in `gi.slang` (40 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct BlendPush {
    frame: u64,
    results: u64,
    probes: u64,
    alpha: f32,
    target: u32,
    classify: u32,
    _pad: u32,
}

/// `ResolveFrame` in `gi.slang` (208 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct ResolveFrame {
    volume: GiVolumeData,
    world_from_clip: [f32; 16],
    eye: [f32; 4],
    ndc_per_pixel: [f32; 2],
    size: [u32; 2],
    intensity: f32,
    scale: u32,
    low_size: [u32; 2],
}

/// `ResolvePush` in `gi.slang` (32 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct ResolvePush {
    frame: u64,
    probes: u64,
    sh: u64,
    depth: u32,
    target: u32,
}

/// `SamplePush` in `gi.slang` (40 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct SamplePush {
    frame: u64,
    probes: u64,
    points: u64,
    out: u64,
    count: u32,
    _pad: u32,
}

const _: () = assert!(size_of::<GiVolumeData>() == 96);
const _: () = assert!(size_of::<TraceFrame>() == 224);
const _: () = assert!(size_of::<ResolveFrame>() == 208);

/// A buffer of the volume's, kept across frames.
#[derive(Debug)]
struct Persistent {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    size: u64,
    state: Cell<BufferState>,
}

impl Persistent {
    fn new(device: &Device, allocator: &mut Allocator, size: u64) -> Result<Self, RhiError> {
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(size).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                    | vk::BufferUsageFlags::TRANSFER_DST,
            ),
            MemoryLocation::Device,
        )?;
        // SAFETY: created with the device address usage.
        let address = unsafe {
            device
                .raw()
                .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
        };
        Ok(Self {
            buffer,
            allocation: Some(allocation),
            address,
            size,
            state: Cell::new(BufferState::idle()),
        })
    }

    /// Imports it into `fg`, left readable by compute shaders.
    fn import<'g>(&self, fg: &mut FrameGraph<'g>, name: &'static str) -> Buffer {
        let final_state = BufferState {
            stages: vk::PipelineStageFlags2::COMPUTE_SHADER,
            access: vk::AccessFlags2::SHADER_STORAGE_READ,
        };
        let buffer = fg.import_buffer(
            name,
            ImportedBuffer {
                buffer: self.buffer,
                size: self.size,
                address: self.address,
                initial: self.state.get(),
                final_state: Some(final_state),
            },
        );
        self.state.set(final_state);
        buffer
    }

    /// # Safety
    ///
    /// The GPU no longer uses the buffer.
    unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

/// A volume of probes and its passes (see the module documentation).
pub struct ProbeGi {
    grid: ProbeGrid,
    settings: GiSettings,
    trace: ComputeShader<TracePush>,
    irradiance_pass: ComputeShader<BlendPush>,
    moments_pass: ComputeShader<BlendPush>,
    relocate: ComputeShader<BlendPush>,
    resolve: ComputeShader<ResolvePush>,
    sample: ComputeShader<SamplePush>,
    irradiance: Image,
    moments: Image,
    /// Per probe: offset from its rest position (xyz) and state (w: 0 on, 1 off).
    probes: Persistent,
    /// Whether the probes were reset (the first update resets them).
    cleared: Cell<bool>,
    /// Whether the maps have a layout (a frame's graph gave them one).
    defined: Cell<bool>,
    /// Updates done since the volume was made or refreshed.
    updates: Cell<u32>,
    /// Updates ever done: the random rotations' sequence.
    serial: Cell<u64>,
}

impl core::fmt::Debug for ProbeGi {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProbeGi")
            .field("grid", &self.grid)
            .field("settings", &self.settings)
            .field("updates", &self.updates.get())
            .finish_non_exhaustive()
    }
}

/// splitmix64: the random numbers of the rotations.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A uniformly random rotation (Shoemake 1992) for update `serial`, as matrix rows.
fn rotation(serial: u64) -> [[f32; 4]; 3] {
    let unit =
        |k: u64| (mix(serial.wrapping_mul(3).wrapping_add(k)) >> 40) as f32 / (1u64 << 24) as f32;
    let (u1, u2, u3) = (unit(0), unit(1), unit(2));
    let tau = core::f32::consts::TAU;
    let q = Quat::from_xyzw(
        (1.0 - u1).sqrt() * (tau * u2).sin(),
        (1.0 - u1).sqrt() * (tau * u2).cos(),
        u1.sqrt() * (tau * u3).sin(),
        u1.sqrt() * (tau * u3).cos(),
    )
    .normalize();
    let m = Mat3::from_quat(q);
    let row = |i: usize| {
        let r = m.row(i);
        [r.x, r.y, r.z, 0.0]
    };
    [row(0), row(1), row(2)]
}

impl ProbeGi {
    /// Makes the volume's maps and buffers; nothing is traced before
    /// [`Self::add_update_passes`]. Fails on a device without ray queries.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        grid: ProbeGrid,
        settings: GiSettings,
    ) -> Result<Self, RhiError> {
        if device.ray_query().is_none() {
            return Err(RhiError::Vulkan {
                call: "probe GI (ray queries are not enabled)",
                result: vk::Result::ERROR_FEATURE_NOT_PRESENT,
            });
        }
        let settings = GiSettings {
            rays: settings.rays.clamp(32, MAX_RAYS),
            bounces: settings.bounces.max(3),
            updates_per_frame: settings.updates_per_frame.max(1),
            resolve_scale: match settings.resolve_scale {
                0 | 1 => 1,
                2 | 3 => 2,
                _ => 4,
            },
            ..settings
        };
        let [trace] = ComputeSource::create_all(
            device,
            bindless,
            [ComputeSource::new(TRACE, c"giTrace", &[])?],
        )?;
        let [irradiance_pass, moments_pass, relocate] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(IRRADIANCE, c"giIrradiance", &[])?,
                ComputeSource::new(MOMENTS, c"giMoments", &[])?,
                ComputeSource::new(RELOCATE, c"giRelocate", &[])?,
            ],
        )?;
        let [resolve] = ComputeSource::create_all(
            device,
            bindless,
            [ComputeSource::new(RESOLVE, c"giResolve", &[])?],
        )?;
        let [sample] = ComputeSource::create_all(
            device,
            bindless,
            [ComputeSource::new(SAMPLE, c"giSample", &[])?],
        )?;
        let [x, y, z] = grid.counts;
        let irradiance = Image::new(
            device,
            allocator,
            bindless,
            vk::Format::R16G16B16A16_SFLOAT,
            x * y * IRRADIANCE_TILE,
            z * IRRADIANCE_TILE,
            1,
            ImageKind::Flat,
        )?;
        let moments = Image::new(
            device,
            allocator,
            bindless,
            vk::Format::R16G16_SFLOAT,
            x * y * MOMENTS_TILE,
            z * MOMENTS_TILE,
            1,
            ImageKind::Flat,
        )?;
        let count = u64::from(grid.probe_count());
        let probes = Persistent::new(device, allocator, count * 16)?;
        Ok(Self {
            grid,
            settings,
            trace,
            irradiance_pass,
            moments_pass,
            relocate,
            resolve,
            sample,
            irradiance,
            moments,
            probes,
            cleared: Cell::new(false),
            defined: Cell::new(false),
            updates: Cell::new(0),
            serial: Cell::new(0),
        })
    }

    /// The grid.
    pub fn grid(&self) -> ProbeGrid {
        self.grid
    }

    /// The settings (as clamped when made).
    pub fn settings(&self) -> GiSettings {
        self.settings
    }

    /// Whether the volume has converged: no updates are left.
    pub fn is_converged(&self) -> bool {
        self.updates.get() >= self.settings.updates()
    }

    /// Updates done since it was made or refreshed.
    pub fn updates_done(&self) -> u32 {
        self.updates.get()
    }

    /// Whether any update was declared (in this frame's graph or an earlier one): until
    /// then its maps hold nothing to read.
    pub fn has_light(&self) -> bool {
        self.serial.get() > 0
    }

    /// Converges again from the next frame on (the light or the scene changed): the
    /// probes are traced afresh, from where they were moved.
    pub fn refresh(&self) {
        self.updates.set(0);
    }

    /// Bytes of the maps and the probes' buffer, kept between frames. (The rays of an
    /// update live in the frame's graph, only in frames that update the volume:
    /// [`Self::rays_bytes`].)
    pub fn memory_bytes(&self) -> u64 {
        let [x, y, z] = self.grid.counts;
        let texels = |tile: u32| u64::from(x * y * tile) * u64::from(z * tile);
        texels(IRRADIANCE_TILE) * 8 + texels(MOMENTS_TILE) * 4 + self.probes.size
    }

    /// Bytes of an update's rays: a transient buffer of the frames that update the volume.
    pub fn rays_bytes(&self) -> u64 {
        u64::from(self.grid.probe_count()) * u64::from(self.settings.rays) * 8
    }

    /// Imports the volume's maps and buffers into this frame's graph, once: every pass of
    /// the frame that updates or reads the volume goes through the returned frame.
    pub fn begin_frame<'g>(&'g self, fg: &mut FrameGraph<'g>) -> GiFrame<'g> {
        let frame = GiFrame {
            gi: self,
            irradiance: self.irradiance.import(fg, "gi irradiance atlas"),
            moments: self.moments.import(fg, "gi moments atlas"),
            probes: self.probes.import(fg, "gi probes"),
        };
        // The first frame gives the maps a layout (their contents stay undefined until
        // the first update replaces them): an import no pass uses would leave them
        // undefined while the next import takes them as readable.
        if !self.defined.replace(true) {
            fg.add_pass("gi maps layout")
                .write(
                    frame.irradiance,
                    Access::StorageWrite(ShaderStages::COMPUTE),
                )
                .write(frame.moments, Access::StorageWrite(ShaderStages::COMPUTE));
        }
        frame
    }

    fn volume_data(&self) -> GiVolumeData {
        let g = &self.grid;
        let smallest = g.spacing.min_element();
        let reach = 1.5 * g.spacing.length();
        let [x, y, z] = g.counts;
        GiVolumeData {
            origin: [g.origin.x, g.origin.y, g.origin.z, reach],
            spacing: [g.spacing.x, g.spacing.y, g.spacing.z, smallest],
            inv_spacing: [
                1.0 / g.spacing.x,
                1.0 / g.spacing.y,
                1.0 / g.spacing.z,
                self.settings.normal_bias * smallest,
            ],
            counts: [x, y, z, x * y * z],
            atlas: [
                1.0 / (x * y * IRRADIANCE_TILE) as f32,
                1.0 / (z * IRRADIANCE_TILE) as f32,
                1.0 / (x * y * MOMENTS_TILE) as f32,
                1.0 / (z * MOMENTS_TILE) as f32,
            ],
            irradiance: self.irradiance.sampled,
            moments: self.moments.sampled,
            sampler: Bindless::sampler(SamplerKind::LinearClamp),
            view_bias: self.settings.view_bias * smallest,
        }
    }

    /// Frees the maps and buffers.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.irradiance
                .destroy(device.raw(), allocator, bindless, 0);
            self.moments.destroy(device.raw(), allocator, bindless, 0);
            self.probes.destroy(allocator);
        }
    }
}

/// A volume in one frame's graph ([`ProbeGi::begin_frame`]).
#[derive(Debug, Clone, Copy)]
pub struct GiFrame<'g> {
    gi: &'g ProbeGi,
    /// The irradiance atlas.
    pub irradiance: Texture,
    /// The moments atlas.
    pub moments: Texture,
    /// The probes' offsets and states.
    pub probes: Buffer,
}

impl<'g> GiFrame<'g> {
    /// Whether the volume has converged ([`ProbeGi::is_converged`]).
    pub fn is_converged(&self) -> bool {
        self.gi.is_converged()
    }

    /// Whether its maps hold light ([`ProbeGi::has_light`]).
    pub fn has_light(&self) -> bool {
        self.gi.has_light()
    }

    /// Pixels per resolved sample along each axis ([`GiSettings::resolve_scale`]).
    pub fn resolve_scale(&self) -> u32 {
        self.gi.settings.resolve_scale
    }

    /// Declares this frame's updates while the volume converges (none once it has):
    /// rays traced against `rays`' top level, their hits shaded from the GPU scene's
    /// instances, the geometry pool and the materials, lit by the sun (a unit vector
    /// towards it and its color, or `None`) and the volume's own light, misses lit by
    /// `sky`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_update_passes(
        &self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        rays: &RayFrame<'g>,
        scene: &SceneFrame<'g>,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        sun: Option<(Vec3, Vec3)>,
        sky: GiSky,
    ) -> Result<(), RhiError> {
        let gi = self.gi;
        if gi.is_converged() {
            return Ok(());
        }
        engine_profiler::zone!("render::GiFrame::add_update_passes");
        let (irradiance, moments, probes) = (self.irradiance, self.moments, self.probes);
        // Each update's rays: radiance and distance as four halves per ray, read by the
        // blend passes of the same update.
        let results = fg.create_buffer(
            "gi rays",
            BufferDesc {
                size: gi.rays_bytes(),
            },
        );
        if !gi.cleared.get() {
            gi.cleared.set(true);
            // Every probe on, at rest. (The maps need no clearing: the first update
            // replaces them without reading them.)
            fg.add_pass("gi probes reset")
                .write(probes, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; the graph made the
                    // buffer a transfer destination.
                    unsafe {
                        ctx.device.cmd_fill_buffer(
                            ctx.cmd,
                            ctx.buffer(probes),
                            0,
                            vk::WHOLE_SIZE,
                            0,
                        );
                    }
                });
        }
        let count = gi.grid.probe_count();
        let volume = gi.volume_data();
        let (sun_direction, sun_color) = sun.map_or(([0.0; 4], [0.0; 4]), |(to_sun, color)| {
            let d = to_sun.normalize_or(Vec3::Y);
            ([d.x, d.y, d.z, 1.0], [color.x, color.y, color.z, 0.0])
        });
        let (sky_rgb, sky_cube, sky_texture) = match sky {
            GiSky::Flat(radiance) => ([radiance.x, radiance.y, radiance.z, 0.0], u32::MAX, None),
            GiSky::Cube {
                texture,
                index,
                intensity,
                ..
            } => ([0.0, 0.0, 0.0, intensity], index, Some(texture)),
        };
        let total = gi.settings.updates();
        for _ in 0..gi.settings.updates_per_frame {
            let update = gi.updates.get();
            if update >= total {
                break;
            }
            gi.updates.set(update + 1);
            let serial = gi.serial.get();
            gi.serial.set(serial + 1);
            let frame = TraceFrame {
                volume,
                rotation: rotation(serial),
                sun_direction,
                sun_color,
                sky: sky_rgb,
                accel: rays.address,
                sky_cube,
                rays: gi.settings.rays,
                mask: RAY_MASK_OPAQUE | RAY_MASK_MASKED,
                bounce: u32::from(update > 0),
                linear_sampler: Bindless::sampler(SamplerKind::LinearClamp),
                _pad0: 0,
            };
            let frame = ring.push(&[frame]).ok_or(RhiError::Vulkan {
                call: "probe GI's update constants (upload ring full)",
                result: vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
            })?;
            let batches = rays.batches;
            let current = scene.current;
            let positions = geometry.position_address();
            let (attributes, indices) = (geometry.attributes, geometry.indices);
            let material_table = materials.buffer;
            let mut trace = fg
                .add_compute("gi trace", bindless)
                .read(
                    rays.tlas,
                    Access::AccelerationStructureRead(ShaderStages::COMPUTE),
                )
                .read(current, Access::StorageRead(ShaderStages::COMPUTE))
                .read(attributes, Access::StorageRead(ShaderStages::COMPUTE))
                .read(indices, Access::StorageRead(ShaderStages::COMPUTE))
                .read(material_table, Access::StorageRead(ShaderStages::COMPUTE))
                .read(probes, Access::StorageRead(ShaderStages::COMPUTE))
                .read(irradiance, Access::Sampled(ShaderStages::COMPUTE))
                .read(moments, Access::Sampled(ShaderStages::COMPUTE))
                .write(results, Access::StorageWrite(ShaderStages::COMPUTE));
            if let Some(texture) = sky_texture {
                trace = trace.read(texture, Access::Sampled(ShaderStages::COMPUTE));
            }
            trace.dispatch(
                &gi.trace,
                Dispatch::Threads([count * gi.settings.rays, 1, 1]),
                move |ctx| {
                    Some(TracePush {
                        frame,
                        probes: ctx.address(probes),
                        results: ctx.address(results),
                        instances: ctx.address(current),
                        batches,
                        positions,
                        attributes: ctx.address(attributes),
                        indices: ctx.address(indices),
                        materials: ctx.address(material_table),
                    })
                },
            );
            // The first updates replace the maps (the light gains a bounce each time);
            // the rest are averaged.
            let alpha = if update < gi.settings.bounces {
                1.0
            } else {
                1.0 / (update - gi.settings.bounces + 1) as f32
            };
            let blend = |target: u32| BlendPush {
                frame,
                alpha,
                target,
                ..BlendPush::default()
            };
            let irradiance_push = blend(gi.irradiance.storage[0]);
            fg.add_compute("gi irradiance", bindless)
                .read(results, Access::StorageRead(ShaderStages::COMPUTE))
                .read(irradiance, Access::StorageReadWrite(ShaderStages::COMPUTE))
                .dispatch(
                    &gi.irradiance_pass,
                    Dispatch::Groups([count, 1, 1]),
                    move |ctx| {
                        Some(BlendPush {
                            results: ctx.address(results),
                            probes: ctx.address(probes),
                            ..irradiance_push
                        })
                    },
                );
            let moments_push = blend(gi.moments.storage[0]);
            fg.add_compute("gi moments", bindless)
                .read(results, Access::StorageRead(ShaderStages::COMPUTE))
                .read(moments, Access::StorageReadWrite(ShaderStages::COMPUTE))
                .dispatch(
                    &gi.moments_pass,
                    Dispatch::Groups([count, 1, 1]),
                    move |ctx| {
                        Some(BlendPush {
                            results: ctx.address(results),
                            probes: ctx.address(probes),
                            ..moments_push
                        })
                    },
                );
            if update <= RELOCATING_UPDATES {
                let relocate_push = BlendPush {
                    classify: u32::from(update == RELOCATING_UPDATES),
                    ..blend(0)
                };
                fg.add_compute("gi relocate", bindless)
                    .read(results, Access::StorageRead(ShaderStages::COMPUTE))
                    .read(probes, Access::StorageReadWrite(ShaderStages::COMPUTE))
                    .dispatch(&gi.relocate, Dispatch::Threads([count, 1, 1]), move |ctx| {
                        Some(BlendPush {
                            results: ctx.address(results),
                            probes: ctx.address(probes),
                            ..relocate_push
                        })
                    });
            }
        }
        Ok(())
    }

    /// Declares a pass writing the diffuse irradiance of one sample per
    /// [`GiSettings::resolve_scale`]² pixels ([`GI_FORMAT`]: rgb, and in a the distance
    /// from `eye` of the surface it was resolved at, 0 for the sky) from `depth` (the
    /// view's, `size` pixels, drawn by `clip_from_world` from `eye`), the sky's
    /// irradiance harmonics (`sh`, 9 × 16 bytes, times `intensity`) outside the volume.
    /// Returns the samples (`size` divided by the scale, rounded up), which the GI
    /// shading reads four at a time.
    #[allow(clippy::too_many_arguments)]
    pub fn add_resolve_pass(
        &self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        depth: Texture,
        size: [u32; 2],
        clip_from_world: Mat4,
        eye: Vec3,
        sh: vk::DeviceAddress,
        intensity: f32,
    ) -> Result<Texture, RhiError> {
        let gi = self.gi;
        let (irradiance, moments, probes) = (self.irradiance, self.moments, self.probes);
        let scale = gi.settings.resolve_scale;
        let low_size = [size[0].div_ceil(scale), size[1].div_ceil(scale)];
        let frame = ResolveFrame {
            volume: gi.volume_data(),
            world_from_clip: clip_from_world.inverse().to_cols_array(),
            eye: [eye.x, eye.y, eye.z, 0.0],
            ndc_per_pixel: [2.0 / size[0] as f32, 2.0 / size[1] as f32],
            size,
            intensity,
            scale,
            low_size,
        };
        let frame = ring.push(&[frame]).ok_or(RhiError::Vulkan {
            call: "probe GI's resolve constants (upload ring full)",
            result: vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        })?;
        let target = fg.create_texture(
            "gi samples",
            TextureDesc::new_2d(GI_FORMAT, low_size[0], low_size[1]),
        );
        fg.add_compute("gi resolve", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(irradiance, Access::Sampled(ShaderStages::COMPUTE))
            .read(moments, Access::Sampled(ShaderStages::COMPUTE))
            .read(probes, Access::StorageRead(ShaderStages::COMPUTE))
            .write(target, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                &gi.resolve,
                Dispatch::Threads([low_size[0], low_size[1], 1]),
                move |ctx| {
                    Some(ResolvePush {
                        frame,
                        probes: ctx.address(probes),
                        sh,
                        depth: ctx.sampled_index(depth)?,
                        target: ctx.storage_index(target)?,
                    })
                },
            );
        Ok(target)
    }

    /// Declares a pass writing the irradiance at `count` points (`points`: per point two
    /// `[f32; 4]`, its position then its normal, written before the frame) into `out`
    /// (per point rgb irradiance and a coverage): tests.
    pub fn add_sample_pass(
        &self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        points: vk::DeviceAddress,
        count: u32,
        out: Buffer,
    ) -> Result<(), RhiError> {
        let gi = self.gi;
        let (irradiance, moments, probes) = (self.irradiance, self.moments, self.probes);
        let frame = TraceFrame {
            volume: gi.volume_data(),
            ..TraceFrame::default()
        };
        let frame = ring.push(&[frame]).ok_or(RhiError::Vulkan {
            call: "probe GI's sample constants (upload ring full)",
            result: vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        })?;
        fg.add_compute("gi sample", bindless)
            .read(irradiance, Access::Sampled(ShaderStages::COMPUTE))
            .read(moments, Access::Sampled(ShaderStages::COMPUTE))
            .read(probes, Access::StorageRead(ShaderStages::COMPUTE))
            .write(out, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(&gi.sample, Dispatch::Threads([count, 1, 1]), move |ctx| {
                Some(SamplePush {
                    frame,
                    probes: ctx.address(probes),
                    points,
                    out: ctx.address(out),
                    count,
                    _pad: 0,
                })
            });
        Ok(())
    }
}
