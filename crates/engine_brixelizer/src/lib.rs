//! AMD FidelityFX Brixelizer GI in the engine (ADR 0090): the reference our probe GI is held
//! to (ADR 0084), built from the FidelityFX SDK by `engine_ffx`.
//!
//! - [`Brixelizer`]: the scene's sparse distance field, built on the GPU from the
//!   instances' triangles (cascades of 64³ voxels around the camera, one cascade updated
//!   per frame); one per scene.
//! - [`BrixelizerGi`]: one view's GI over it, a [`ScreenGi`] for the forward renderer
//!   ([`ViewGi::Screen`](engine_render::forward::ViewGi::Screen)): diffuse light from screen
//!   probes traced through the distance field and a radiance cache fed by last frame's lit
//!   image, for the GI shading of ADR 0088. The SDK's diffuse light is irradiance: an open
//!   floor under a uniform sky of radiance 1 gets 3.19 to 3.21 on both GPUs at 512 × 512
//!   (π; `tests/brixelizer/gi.rs`).
//!
//! The GI waits for the distance field: its first updates build every cascade once (one a
//! frame), and a view's GI runs from then on. Rays that leave the built cascades miss
//! everything and bring the sky into the GI's caches, whence it fades by half every 48
//! frames or so: a closed room under a sky of radiance 1 kept a quarter of its light 64
//! frames after the start.
//!
//! What the SDK needs that the forward renderer does not make is made for it each frame
//! (`shaders/brixelizer.slang`): the depth inverted, normals from the depth (as probe GI's
//! resolve takes them), the history of both, last frame's lit image, blue noise, and the
//! camera's motion from the depth when the view has no motion vectors (no temporal
//! antialiasing; moving objects then show no motion of their own).
//!
//! ```text
//! // Once per scene: the distance field and its instances (static).
//! let mut sdf = Brixelizer::new(&device, &mut allocator, SdfSettings::default())?;
//! sdf.set_instances(&geometry_pool, &instances)?;
//! // Once per view (and again when its size changes).
//! let gi = BrixelizerGi::new(&device, &mut allocator, &bindless, &sdf, width, height,
//!     GiSettings::for_device(&device))?;
//! // Every frame, before the view's scene passes:
//! let sdf_frame = sdf.add_update_pass(&mut fg, &geometry_frame, camera_position);
//! gi.begin_frame(&sdf_frame);
//! let view = ForwardView { gi: Some(ViewGi::Screen(&gi)), .. };
//! // ... the forward passes, then the lit image for next frame:
//! gi.add_capture_pass(&mut fg, hdr);
//! ```

use core::cell::Cell;
use core::fmt;
use core::ptr::NonNull;
use engine_core::math::{Mat4, Vec3};
use engine_ffx::brixelizer as ffi;
use engine_render::compute::{ComputeShader, Dispatch};
use engine_render::forward::{Camera, ScreenGi, ScreenGiInput};
use engine_render::graph::{
    Access, Buffer, BufferState, FrameGraph, ImportedBuffer, ImportedTexture, PassContext,
    ShaderStages, Texture, TextureDesc, TextureState,
};
use engine_render::mesh::{GeometryFrame, GeometryPool, MeshId};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterKind, Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError};

pub use engine_ffx::brixelizer::{InternalResolution, Stats};

const INPUTS_SPV: &[u8] = include_bytes!("../shaders/brixelizer.brixelizerInputs.spv");
const IRRADIANCE_SPV: &[u8] = include_bytes!("../shaders/brixelizer.brixelizerIrradiance.spv");
/// The blue noise tiles (`examples/blue_noise.rs`): 128 × 128 texels, R and G.
const BLUE_NOISE: &[u8] = include_bytes!("../data/blue_noise_128.bin");
/// The noise's size, and the frames before it repeats (one image each, as AMD's sample).
const NOISE_SIZE: u32 = 128;
const NOISE_FRAMES: usize = 16;

/// Formats of the images made for the SDK.
const DEPTH_FORMAT: vk::Format = vk::Format::R32_SFLOAT;
const NORMAL_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const LIT_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const NOISE_FORMAT: vk::Format = vk::Format::R8G8_UNORM;
/// The SDK's outputs and the GI shading's irradiance.
const GI_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// An error from the FidelityFX SDK or from Vulkan.
#[derive(Debug)]
pub enum BrixelizerError {
    /// An SDK call returned an `FfxErrorCode` (`ffx_error.h`).
    Ffx {
        /// What was being done.
        call: &'static str,
        /// The error code.
        code: i32,
    },
    /// A Vulkan call failed.
    Rhi(RhiError),
    /// More instances than a context holds ([`ffi::MAX_INSTANCES`]).
    TooManyInstances,
}

impl fmt::Display for BrixelizerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ffx { call, code } => write!(f, "{call} failed: FfxErrorCode {code:#x}"),
            Self::Rhi(e) => write!(f, "{e}"),
            Self::TooManyInstances => write!(f, "more than {} instances", ffi::MAX_INSTANCES),
        }
    }
}

impl std::error::Error for BrixelizerError {}

impl From<RhiError> for BrixelizerError {
    fn from(e: RhiError) -> Self {
        Self::Rhi(e)
    }
}

fn ffx(call: &'static str, code: i32) -> Result<(), BrixelizerError> {
    if code == 0 {
        Ok(())
    } else {
        Err(BrixelizerError::Ffx { call, code })
    }
}

/// A distance field's cascades and its build's limits ([`Brixelizer::new`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SdfSettings {
    /// Cascades (at most 8 with `dynamic`, 24 without). AMD's sample: 8.
    pub cascades: u32,
    /// Cascade 0's voxel (brick) edge, world units; each cascade spans 64 of its voxels.
    /// AMD's sample: 0.2.
    pub voxel_size: f32,
    /// Each cascade's voxel size over the previous one's. AMD's sample: 2.
    pub cascade_ratio: f32,
    /// Static and dynamic cascades, merged into the traced ones, as AMD's sample (else
    /// static ones only: no merge, a third of the cascade memory).
    pub dynamic: bool,
    /// Triangle references one update may store.
    pub max_references: u32,
    /// Bytes of triangle swap space one update may use.
    pub triangle_swap_size: u32,
    /// Bricks one update may build.
    pub max_bricks_per_bake: u32,
    /// Reads the build's statistics back ([`Brixelizer::stats`]): a few copies a frame.
    pub readback: bool,
    /// Views ([`BrixelizerGi`]) the distance field serves at once.
    pub max_views: u32,
}

impl Default for SdfSettings {
    fn default() -> Self {
        Self {
            cascades: 8,
            voxel_size: 0.2,
            cascade_ratio: 2.0,
            dynamic: true,
            max_references: 1 << 22,
            triangle_swap_size: 1 << 26,
            max_bricks_per_bake: 1 << 14,
            readback: false,
            max_views: 2,
        }
    }
}

/// The resources an update writes and the GI reads are the SDK's own: a small buffer of
/// ours stands for them in the frame's graph, so the GI's passes come after the update.
#[derive(Debug)]
struct Token {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    used: Cell<bool>,
}

/// What the GPU reads the registered geometry from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Registered {
    positions: vk::Buffer,
    indices: vk::Buffer,
    position_index: u32,
    index_index: u32,
}

/// The scene's sparse distance field (see the crate documentation).
pub struct Brixelizer {
    raw: NonNull<ffi::Context>,
    settings: SdfSettings,
    registered: Option<Registered>,
    instances: Vec<u32>,
    token: Token,
    /// Updates since the instances were set: the first build every cascade once.
    updates: Cell<u32>,
    failures: Cell<u32>,
}

// SAFETY: the SDK's context is not tied to the thread that created it; the shim sets the
// device's functions for the calling thread on every call. `Brixelizer` is used from one
// thread at a time (`&mut self`, or `&self` within a frame's recording).
unsafe impl Send for Brixelizer {}

impl fmt::Debug for Brixelizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Brixelizer")
            .field("settings", &self.settings)
            .field("instances", &self.instances.len())
            .finish_non_exhaustive()
    }
}

/// What [`Brixelizer::add_update_pass_with_debug`] shows (the SDK's
/// `FfxBrixelizerTraceDebugModes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SdfDebugView {
    /// The distance to the hit: near blue, far green.
    Distance = 0,
    /// The hit's coordinates in its brick.
    Uvw = 1,
    /// A heat map of the steps taken.
    Iterations = 2,
    /// The distance field's normals at the hit.
    Normals = 3,
    /// Each brick in its own color.
    Bricks = 4,
    /// Each cascade in its own color.
    Cascades = 5,
}

/// A [`Brixelizer`] in one frame's graph ([`Brixelizer::add_update_pass`]): what its views
/// read after the update.
#[derive(Debug, Clone, Copy)]
pub struct SdfFrame {
    token: Buffer,
    serial: u64,
    /// Every cascade was built (the GI may trace it).
    built: bool,
}

impl SdfFrame {
    /// Whether every cascade of the distance field was built by this frame: until then a
    /// view's GI does not run.
    pub fn built(&self) -> bool {
        self.built
    }
}

impl Brixelizer {
    /// Creates the distance field's context on `device` (its 512³ atlas, bounds, cascade
    /// buffers and build scratch: 128 MiB and up; its pipelines are compiled now).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        settings: SdfSettings,
    ) -> Result<Self, BrixelizerError> {
        engine_profiler::zone!("brixelizer::Brixelizer::new");
        engine_ffx::register_modules();
        let desc = ffi::CreateDesc {
            device: engine_ffx::Device::new(device),
            center: [0.0; 3],
            cascade_count: settings.cascades,
            voxel_size: settings.voxel_size,
            cascade_ratio: settings.cascade_ratio,
            dynamic: u32::from(settings.dynamic),
            max_references: settings.max_references,
            triangle_swap_size: settings.triangle_swap_size,
            max_bricks_per_bake: settings.max_bricks_per_bake,
            readback: u32::from(settings.readback),
            max_gi_contexts: settings.max_views,
        };
        let mut raw = core::ptr::null_mut();
        // SAFETY: the device is alive; the modules were handed over.
        let code = unsafe { ffi::engine_brixelizer_create(&desc, &mut raw) };
        ffx("ffxBrixelizerContextCreate", code)?;
        let Some(raw) = NonNull::new(raw) else {
            return Err(BrixelizerError::Ffx {
                call: "ffxBrixelizerContextCreate",
                code,
            });
        };
        let token = match allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(16)
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
            MemoryLocation::Device,
        ) {
            Ok((buffer, allocation)) => Token {
                buffer,
                allocation: Some(allocation),
                used: Cell::new(false),
            },
            Err(e) => {
                // SAFETY: never used on the GPU.
                unsafe { ffi::engine_brixelizer_destroy(raw.as_ptr()) };
                return Err(e.into());
            }
        };
        Ok(Self {
            raw,
            settings,
            registered: None,
            instances: Vec::new(),
            token,
            updates: Cell::new(0),
            failures: Cell::new(0),
        })
    }

    /// The settings it was created with.
    pub fn settings(&self) -> &SdfSettings {
        &self.settings
    }

    /// Places the scene's static instances: each a mesh of `geometry` and its world
    /// transform; replaces the previous ones. Every cascade is built again over the next
    /// [`SdfSettings::cascades`] updates, before the GI runs again. Call it again after
    /// the pool grows (its buffers change). Instances added after the first update need no
    /// frame using this distance field in flight (the SDK uploads each in a submission of
    /// its own, recycling per-call views).
    pub fn set_instances(
        &mut self,
        geometry: &GeometryPool,
        instances: &[(MeshId, Mat4)],
    ) -> Result<(), BrixelizerError> {
        engine_profiler::zone!("brixelizer::Brixelizer::set_instances");
        if instances.len() > ffi::MAX_INSTANCES as usize {
            return Err(BrixelizerError::TooManyInstances);
        }
        let raw = self.raw.as_ptr();
        if !self.instances.is_empty() {
            // SAFETY: a live context; ids it gave.
            let code = unsafe {
                ffi::engine_brixelizer_delete_instances(
                    raw,
                    self.instances.as_ptr(),
                    self.instances.len() as u32,
                )
            };
            ffx("ffxBrixelizerDeleteInstances", code)?;
            self.instances.clear();
        }
        let buffers = geometry.raw_buffers();
        if self
            .registered
            .is_none_or(|r| (r.positions, r.indices) != (buffers.positions, buffers.indices))
        {
            if let Some(old) = self.registered.take() {
                for index in [old.position_index, old.index_index] {
                    // SAFETY: a live context; an index it gave.
                    let code = unsafe { ffi::engine_brixelizer_unregister_buffer(raw, index) };
                    ffx("ffxBrixelizerUnregisterBuffers", code)?;
                }
            }
            let register = |buffer: vk::Buffer, size: u64| -> Result<u32, BrixelizerError> {
                let mut index = 0;
                // SAFETY: a live context and a live buffer with storage usage.
                let code = unsafe {
                    ffi::engine_brixelizer_register_buffer(
                        raw,
                        buffer,
                        size.min(u64::from(u32::MAX)) as u32,
                        &mut index,
                    )
                };
                ffx("ffxBrixelizerRegisterBuffers", code)?;
                Ok(index)
            };
            self.registered = Some(Registered {
                positions: buffers.positions,
                indices: buffers.indices,
                position_index: register(buffers.positions, buffers.positions_size)?,
                index_index: register(buffers.indices, buffers.indices_size)?,
            });
        }
        let Some(registered) = self.registered else {
            unreachable!("registered above");
        };
        let mut descs = Vec::with_capacity(instances.len());
        for &(mesh, transform) in instances {
            let Some(info) = geometry.mesh(mesh) else {
                continue;
            };
            let (min, max) = world_bounds(transform, info.min, info.max);
            let c = transform.to_cols_array_2d();
            descs.push(ffi::Instance {
                aabb_min: min.to_array(),
                aabb_max: max.to_array(),
                // Rows of the affine part: column-major `c[column][row]`.
                transform: [
                    c[0][0], c[1][0], c[2][0], c[3][0], c[0][1], c[1][1], c[2][1], c[3][1],
                    c[0][2], c[1][2], c[2][2], c[3][2],
                ],
                index_buffer: registered.index_index,
                index_offset: info.first_index * 4,
                triangle_count: info.index_count / 3,
                vertex_buffer: registered.position_index,
                vertex_offset: info.first_vertex * 12,
                vertex_stride: 12,
                vertex_count: info.vertex_count,
                dynamic: 0,
            });
        }
        self.instances.resize(descs.len(), 0);
        // SAFETY: a live context; as many ids as instances.
        let code = unsafe {
            ffi::engine_brixelizer_create_instances(
                raw,
                descs.as_ptr(),
                descs.len() as u32,
                self.instances.as_mut_ptr(),
            )
        };
        // Built again from the next update, every cascade once before the GI traces it.
        self.updates.set(0);
        ffx("ffxBrixelizerCreateInstances", code)
    }

    /// Declares this frame's update (one cascade, around `center`: the camera), reading
    /// the geometry pool. Its views read the distance field after it
    /// ([`BrixelizerGi::begin_frame`]). The first [`SdfSettings::cascades`] updates after
    /// the instances are set build every cascade once, the coarsest last
    /// ([`SdfFrame::built`]); later ones follow the SDK's schedule (the first cascade every
    /// other frame, the next every fourth, and so on).
    pub fn add_update_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        geometry: &GeometryFrame<'g>,
        center: Vec3,
    ) -> SdfFrame {
        self.update(fg, geometry, center, None).0
    }

    /// [`Self::add_update_pass`] with the SDK's debug view of the distance field traced
    /// from `camera` (RGBA16F, `size`).
    pub fn add_update_pass_with_debug<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        geometry: &GeometryFrame<'g>,
        center: Vec3,
        camera: Camera,
        view: SdfDebugView,
        size: (u32, u32),
    ) -> (SdfFrame, Option<Texture>) {
        self.update(fg, geometry, center, Some((camera, view, size)))
    }

    fn update<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        geometry: &GeometryFrame<'g>,
        center: Vec3,
        debug: Option<(Camera, SdfDebugView, (u32, u32))>,
    ) -> (SdfFrame, Option<Texture>) {
        engine_profiler::zone!("brixelizer::Brixelizer::add_update_pass");
        let state = BufferState {
            stages: vk::PipelineStageFlags2::COMPUTE_SHADER,
            access: vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
        };
        let initial = if self.token.used.replace(true) {
            state
        } else {
            BufferState::idle()
        };
        let token = fg.import_buffer(
            "brixelizer distance field",
            ImportedBuffer {
                buffer: self.token.buffer,
                size: 16,
                address: 0,
                initial,
                final_state: Some(state),
            },
        );
        // The SDK updates the cascade of the frame index's lowest set bit, the coarsest at 0
        // (`ffxBrixelizerRawGetCascadeToUpdate`): the first updates take the indices that
        // build each cascade once, the coarsest last, then the count goes on.
        let update = self.updates.get();
        self.updates.set(update.saturating_add(1));
        let cascades = self.settings.cascades;
        let frame = match update {
            k if k + 1 < cascades => 1 << k,
            k if k + 1 == cascades => 0,
            k => k,
        };
        let built = update + 1 >= cascades;
        let raw = self.raw;
        let failures = &self.failures;
        // The debug view: the camera's matrices as the SDK takes them, and its image.
        let debug = debug.and_then(|(camera, view, (width, height))| {
            let (v, p) = sdk_view_projection(camera)?;
            let texture = fg.create_texture(
                "brixelizer sdf debug",
                TextureDesc::new_2d(GI_FORMAT, width, height),
            );
            Some((
                v.inverse(),
                p.inverse(),
                view as u32 + 1,
                texture,
                width,
                height,
            ))
        });
        let mut pass = fg
            .add_pass("brixelizer update")
            .read(
                geometry.positions,
                Access::StorageRead(ShaderStages::COMPUTE),
            )
            .read(geometry.indices, Access::StorageRead(ShaderStages::COMPUTE))
            .write(token, Access::StorageWrite(ShaderStages::COMPUTE));
        if let Some((.., texture, _, _)) = debug {
            pass = pass.write(texture, Access::StorageWrite(ShaderStages::COMPUTE));
        }
        pass.execute(move |ctx| {
            let (debug_mode, inverse_view, inverse_projection, debug_output) = match debug {
                Some((v, p, mode, texture, width, height)) => (
                    mode,
                    v.to_cols_array(),
                    p.to_cols_array(),
                    engine_ffx::Image {
                        image: ctx.image(texture),
                        format: GI_FORMAT,
                        width,
                        height,
                    },
                ),
                None => (0, [0.0; 16], [0.0; 16], engine_ffx::Image::default()),
            };
            let desc = ffi::UpdateDesc {
                command_buffer: ctx.cmd,
                center: center.to_array(),
                frame_index: frame,
                debug_mode,
                inverse_view,
                inverse_projection,
                pad: 0,
                debug_output,
            };
            // SAFETY: a live context; the geometry is alive for the frame and synchronized
            // for compute reads by the graph.
            if unsafe { ffi::engine_brixelizer_update(raw.as_ptr(), &desc) } != 0 {
                failures.set(failures.get() + 1);
            }
        });
        (
            SdfFrame {
                token,
                serial: fg.serial(),
                built,
            },
            debug.map(|(.., texture, _, _)| texture),
        )
    }

    /// Updates the SDK refused (scratch too small for an update, invalid inputs).
    pub fn failures(&self) -> u32 {
        self.failures.get()
    }

    /// The build's statistics some frames ago (zeros without [`SdfSettings::readback`]).
    pub fn stats(&self) -> Stats {
        let mut stats = Stats::default();
        // SAFETY: a live context.
        unsafe { ffi::engine_brixelizer_stats(self.raw.as_ptr(), &mut stats) };
        stats
    }

    /// Bytes of GPU memory the distance field holds: the SDK's resources, the atlas,
    /// bounds, cascades and the build's scratch.
    pub fn gpu_memory(&self) -> u64 {
        // SAFETY: a live context.
        unsafe { ffi::engine_brixelizer_gpu_memory(self.raw.as_ptr()) }
    }

    /// Destroys the context and its resources.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them, and every [`BrixelizerGi`] over this distance field is
    /// destroyed.
    pub unsafe fn destroy(&mut self, device: &Device, allocator: &mut Allocator) {
        // SAFETY: guaranteed by the caller.
        unsafe { ffi::engine_brixelizer_destroy(self.raw.as_ptr()) };
        self.raw = NonNull::dangling();
        if let Some(allocation) = self.token.allocation.take() {
            // SAFETY: guaranteed by the caller.
            unsafe { device.raw().destroy_buffer(self.token.buffer, None) };
            allocator.free(allocation);
        }
    }
}

/// The world bounds of an object-space box under `transform`.
fn world_bounds(transform: Mat4, min: Vec3, max: Vec3) -> (Vec3, Vec3) {
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for corner in 0..8u32 {
        let p = Vec3::new(
            if corner & 1 == 0 { min.x } else { max.x },
            if corner & 2 == 0 { min.y } else { max.y },
            if corner & 4 == 0 { min.z } else { max.z },
        );
        let w = transform.transform_point3(p);
        lo = lo.min(w);
        hi = hi.max(w);
    }
    (lo, hi)
}

/// A view's GI settings ([`BrixelizerGi::new`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GiSettings {
    /// The GI's internal size: probe GI's resolve scale (ADR 0088) is 50 % on discrete
    /// GPUs and 25 % on integrated ones ([`Self::for_device`]).
    pub internal_resolution: InternalResolution,
    /// Specular GI traced too (its output is not used by the forward renderer yet).
    pub specular: bool,
    /// AMD's sample: rays start 0.25 off the surface, the distance field is solved to 0.5
    /// of a voxel, rays reach 10 km.
    pub ray_pushoff: f32,
    /// See [`Self::ray_pushoff`].
    pub sdf_solve_eps: f32,
    /// See [`Self::ray_pushoff`].
    pub t_min: f32,
    /// See [`Self::ray_pushoff`].
    pub t_max: f32,
}

impl Default for GiSettings {
    fn default() -> Self {
        Self {
            internal_resolution: InternalResolution::Percent50,
            specular: false,
            ray_pushoff: 0.25,
            sdf_solve_eps: 0.5,
            t_min: 0.0,
            t_max: 10_000.0,
        }
    }
}

impl GiSettings {
    /// The defaults with probe GI's resolve scale for `device` (ADR 0088): 25 % on
    /// integrated GPUs.
    pub fn for_device(device: &Device) -> Self {
        let integrated = matches!(
            device.adapter().kind,
            AdapterKind::Integrated | AdapterKind::Cpu
        );
        Self {
            internal_resolution: if integrated {
                InternalResolution::Percent25
            } else {
                InternalResolution::Percent50
            },
            ..Self::default()
        }
    }
}

/// An image kept across frames, in the bindless set.
#[derive(Debug)]
struct Kept {
    image: vk::Image,
    view: vk::ImageView,
    allocation: Option<Allocation>,
    desc: TextureDesc,
    sampled: u32,
    storage: Option<u32>,
    state: Cell<TextureState>,
}

impl Kept {
    fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        format: vk::Format,
        width: u32,
        height: u32,
        usage: vk::ImageUsageFlags,
    ) -> Result<Self, RhiError> {
        let storage = usage.contains(vk::ImageUsageFlags::STORAGE);
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
                .usage(usage | vk::ImageUsageFlags::SAMPLED),
            MemoryLocation::Device,
        )?;
        // SAFETY: a view of the image just created.
        let view = unsafe {
            device.raw().create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(color_range()),
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
        let sampled = bindless.add_sampled_image(view).ok_or_else(full)?;
        let storage = if storage {
            Some(bindless.add_storage_image(view).ok_or_else(full)?)
        } else {
            None
        };
        Ok(Self {
            image,
            view,
            allocation: Some(allocation),
            desc: TextureDesc::new_2d(format, width, height),
            sampled,
            storage,
            state: Cell::new(TextureState::undefined()),
        })
    }

    /// Imports the image into `fg`; it is left readable by compute shaders.
    fn import(&self, fg: &mut FrameGraph<'_>, name: &'static str) -> Texture {
        let final_state = TextureState::shader_read(ShaderStages::COMPUTE);
        let texture = fg.import_texture(
            name,
            ImportedTexture {
                image: self.image,
                view: self.view,
                desc: self.desc,
                initial: self.state.get(),
                final_state: Some(final_state),
                sampled_index: Some(self.sampled),
                storage_index: self.storage,
            },
        );
        self.state.set(final_state);
        texture
    }

    /// # Safety
    ///
    /// The GPU no longer uses the image after `frame`.
    unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame: u64,
    ) {
        bindless.remove_sampled_image(self.sampled, frame);
        if let Some(storage) = self.storage {
            bindless.remove_storage_image(storage, frame);
        }
        // SAFETY: guaranteed by the caller.
        unsafe {
            device.raw().destroy_image_view(self.view, None);
            if let Some(allocation) = self.allocation.take() {
                allocator.destroy_image(self.image, allocation);
            }
        }
    }

    fn bytes(&self) -> u64 {
        self.allocation.as_ref().map_or(0, Allocation::size)
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: vk::REMAINING_MIP_LEVELS,
        base_array_layer: 0,
        layer_count: vk::REMAINING_ARRAY_LAYERS,
    }
}

/// `ScreenFrame` in `brixelizer.slang` (112 bytes).
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct ScreenFrame {
    world_from_clip: [f32; 16],
    previous_clip_from_world: [f32; 16],
    eye: [f32; 4],
    size: [u32; 2],
    ndc_per_pixel: [f32; 2],
    motion: u32,
    _pad: [u32; 3],
}

const _: () = assert!(size_of::<ScreenFrame>() == 176);

/// `Push` in `brixelizer.slang` (32 bytes).
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    frame: u64,
    depth: u32,
    source: u32,
    target0: u32,
    target1: u32,
    target2: u32,
    _pad: u32,
}

/// What [`BrixelizerGi::add_debug_pass`] shows at each pixel's surface (the SDK's debug
/// view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugView {
    /// The radiance the cache holds (last frames' lit images, on the distance field's
    /// bricks).
    RadianceCache,
    /// The irradiance of the bricks' harmonics.
    IrradianceCache,
}

/// What the frame's GI made, for its debug view.
#[derive(Debug, Clone, Copy)]
struct LastFrame {
    serial: u64,
    depth: Texture,
    normal: Texture,
    diffuse: Texture,
    view: Mat4,
    projection: Mat4,
}

/// What a view's frame imported: its inputs this frame and last frame's.
#[derive(Debug, Clone, Copy)]
struct ViewImages {
    depth: Texture,
    normal: Texture,
    previous_depth: Texture,
    previous_normal: Texture,
    lit: Texture,
    noise: Texture,
}

/// One view's Brixelizer GI (see the crate documentation): a [`ScreenGi`].
pub struct BrixelizerGi {
    raw: NonNull<ffi::GiContext>,
    cascades: u32,
    width: u32,
    height: u32,
    settings: GiSettings,
    depth: [Kept; 2],
    normal: [Kept; 2],
    /// The camera's motion, for views without motion vectors.
    motion: Kept,
    lit: Kept,
    noise: Vec<Kept>,
    noise_upload: Option<(vk::Buffer, Allocation)>,
    noise_ready: Cell<bool>,
    lit_ready: Cell<bool>,
    inputs: ComputeShader<Push>,
    irradiance: ComputeShader<Push>,
    /// Which of the pairs holds this frame's inputs.
    parity: Cell<usize>,
    /// Whether the other pair holds last frame's.
    history: Cell<bool>,
    /// Last frame's view and projection, as the SDK takes them.
    previous: Cell<Option<(Mat4, Mat4)>>,
    sdf: Cell<Option<SdfFrame>>,
    last: Cell<Option<LastFrame>>,
    frame: Cell<u64>,
    failures: Cell<u32>,
}

// SAFETY: as for `Brixelizer`.
unsafe impl Send for BrixelizerGi {}

impl fmt::Debug for BrixelizerGi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrixelizerGi")
            .field("size", &(self.width, self.height))
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl BrixelizerGi {
    /// Creates a view's GI of `width` × `height` over `sdf` (its pipelines are compiled
    /// now; its caches take about 100 MiB whatever the size).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        sdf: &Brixelizer,
        width: u32,
        height: u32,
        settings: GiSettings,
    ) -> Result<Self, BrixelizerError> {
        engine_profiler::zone!("brixelizer::BrixelizerGi::new");
        let (width, height) = (width.max(1), height.max(1));
        let inputs = ComputeShader::<Push>::new(device, bindless, INPUTS_SPV, c"brixelizerInputs")?;
        let irradiance =
            ComputeShader::<Push>::new(device, bindless, IRRADIANCE_SPV, c"brixelizerIrradiance")?;
        let storage = vk::ImageUsageFlags::STORAGE;
        let transfer = vk::ImageUsageFlags::TRANSFER_DST;
        let depth = [
            Kept::new(
                device,
                allocator,
                bindless,
                DEPTH_FORMAT,
                width,
                height,
                storage,
            )?,
            Kept::new(
                device,
                allocator,
                bindless,
                DEPTH_FORMAT,
                width,
                height,
                storage,
            )?,
        ];
        let normal = [
            Kept::new(
                device,
                allocator,
                bindless,
                NORMAL_FORMAT,
                width,
                height,
                storage,
            )?,
            Kept::new(
                device,
                allocator,
                bindless,
                NORMAL_FORMAT,
                width,
                height,
                storage,
            )?,
        ];
        let motion = Kept::new(
            device,
            allocator,
            bindless,
            NORMAL_FORMAT,
            width,
            height,
            storage,
        )?;
        // Last frame's lit image at the GI's internal size: the SDK samples it by texture
        // coordinates and downsamples it to that size itself.
        let (lit_width, lit_height) = internal_size(settings.internal_resolution, width, height);
        let lit = Kept::new(
            device, allocator, bindless, LIT_FORMAT, lit_width, lit_height, transfer,
        )?;
        let mut noise = Vec::with_capacity(NOISE_FRAMES);
        for _ in 0..NOISE_FRAMES {
            noise.push(Kept::new(
                device,
                allocator,
                bindless,
                NOISE_FORMAT,
                NOISE_SIZE,
                NOISE_SIZE,
                transfer,
            )?);
        }
        let noise_upload = noise_staging(allocator)?;
        let desc = ffi::GiCreateDesc {
            width,
            height,
            internal_resolution: settings.internal_resolution,
            specular: u32::from(settings.specular),
        };
        let mut raw = core::ptr::null_mut();
        // SAFETY: a live distance field context.
        let code = unsafe { ffi::engine_brixelizer_gi_create(sdf.raw.as_ptr(), &desc, &mut raw) };
        ffx("ffxBrixelizerGIContextCreate", code)?;
        let Some(raw) = NonNull::new(raw) else {
            return Err(BrixelizerError::Ffx {
                call: "ffxBrixelizerGIContextCreate",
                code,
            });
        };
        Ok(Self {
            raw,
            cascades: sdf.settings.cascades,
            width,
            height,
            settings,
            depth,
            normal,
            motion,
            lit,
            noise,
            noise_upload: Some(noise_upload),
            noise_ready: Cell::new(false),
            lit_ready: Cell::new(false),
            inputs,
            irradiance,
            parity: Cell::new(0),
            history: Cell::new(false),
            previous: Cell::new(None),
            sdf: Cell::new(None),
            last: Cell::new(None),
            frame: Cell::new(0),
            failures: Cell::new(0),
        })
    }

    /// The view's size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The settings it was created with.
    pub fn settings(&self) -> &GiSettings {
        &self.settings
    }

    /// Sets the distance field this frame's GI reads ([`Brixelizer::add_update_pass`]);
    /// without it, the frame has no GI.
    pub fn begin_frame(&self, sdf: &SdfFrame) {
        self.sdf.set(Some(*sdf));
    }

    /// Forgets the history (a camera cut): the next frame starts anew.
    pub fn reset(&self) {
        self.history.set(false);
        self.previous.set(None);
    }

    /// Declares the copy of the view's lit image (`lit`, HDR, after its scene passes) for
    /// next frame's GI, scaled to the GI's internal size.
    pub fn add_capture_pass<'g>(&'g self, fg: &mut FrameGraph<'g>, lit: Texture) {
        let source = fg.texture_desc(lit);
        let target = self.lit.import(fg, "brixelizer lit image");
        let (width, height) = (self.lit.desc.width, self.lit.desc.height);
        fg.add_pass("brixelizer lit copy")
            .read(lit, Access::TransferRead)
            .write(target, Access::TransferWrite)
            .execute(move |ctx| {
                let region = vk::ImageBlit2::default()
                    .src_subresource(color_layers())
                    .src_offsets([
                        vk::Offset3D::default(),
                        vk::Offset3D {
                            x: source.width as i32,
                            y: source.height as i32,
                            z: 1,
                        },
                    ])
                    .dst_subresource(color_layers())
                    .dst_offsets([
                        vk::Offset3D::default(),
                        vk::Offset3D {
                            x: width as i32,
                            y: height as i32,
                            z: 1,
                        },
                    ]);
                // SAFETY: recording outside a rendering scope; the graph made the images a
                // transfer source and destination.
                unsafe {
                    ctx.device.cmd_blit_image2(
                        ctx.cmd,
                        &vk::BlitImageInfo2::default()
                            .src_image(ctx.image(lit))
                            .src_image_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                            .dst_image(ctx.image(target))
                            .dst_image_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                            .regions(core::slice::from_ref(&region))
                            .filter(vk::Filter::LINEAR),
                    );
                }
            });
        self.lit_ready.set(true);
    }

    /// Declares the SDK's debug view of the GI's caches at each pixel's surface, after this
    /// frame's GI (RGBA16F, the view's size); `None` if the frame had no GI.
    pub fn add_debug_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        view: DebugView,
    ) -> Option<Texture> {
        let last = self.last.get().filter(|l| l.serial == fg.serial())?;
        let output = fg.create_texture(
            "brixelizer debug",
            TextureDesc::new_2d(GI_FORMAT, self.width, self.height),
        );
        let raw = self.raw;
        let failures = &self.failures;
        let descs = [fg.texture_desc(last.depth), fg.texture_desc(last.normal)];
        let (width, height) = (self.width, self.height);
        fg.add_pass("brixelizer debug")
            .read(last.diffuse, Access::Sampled(ShaderStages::COMPUTE))
            .read(last.depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(last.normal, Access::Sampled(ShaderStages::COMPUTE))
            .write(output, Access::StorageWrite(ShaderStages::COMPUTE))
            .execute(move |ctx| {
                let inputs = [ctx.image(last.depth), ctx.image(last.normal)];
                // SAFETY: as in the GI's pass.
                unsafe {
                    transition(
                        ctx,
                        &inputs,
                        (
                            vk::PipelineStageFlags2::ALL_COMMANDS,
                            vk::AccessFlags2::MEMORY_WRITE,
                        ),
                        vk::ImageLayout::READ_ONLY_OPTIMAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    );
                }
                let image = |texture: Texture, desc: TextureDesc| engine_ffx::Image {
                    image: ctx.image(texture),
                    format: desc.format,
                    width: desc.width,
                    height: desc.height,
                };
                let desc = ffi::GiDebugDesc {
                    command_buffer: ctx.cmd,
                    view: last.view.to_cols_array(),
                    projection: last.projection.to_cols_array(),
                    mode: match view {
                        DebugView::RadianceCache => 0,
                        DebugView::IrradianceCache => 1,
                    },
                    normals_unpack_mul: 1.0,
                    normals_unpack_add: 0.0,
                    pad: 0,
                    depth: image(last.depth, descs[0]),
                    normal: image(last.normal, descs[1]),
                    output: engine_ffx::Image {
                        image: ctx.image(output),
                        format: GI_FORMAT,
                        width,
                        height,
                    },
                };
                // SAFETY: a live context; the images are alive for the frame and in the
                // layouts the SDK expects.
                if unsafe { ffi::engine_brixelizer_gi_debug(raw.as_ptr(), &desc) } != 0 {
                    failures.set(failures.get() + 1);
                }
                // SAFETY: as above, after the SDK's compute reads.
                unsafe {
                    transition(
                        ctx,
                        &inputs,
                        (
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::NONE,
                        ),
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::READ_ONLY_OPTIMAL,
                    );
                }
            });
        Some(output)
    }

    /// GI dispatches the SDK refused.
    pub fn failures(&self) -> u32 {
        self.failures.get()
    }

    /// Bytes of GPU memory the view's GI holds: the SDK's caches and screen-sized images,
    /// and the inputs made for it.
    pub fn gpu_memory(&self) -> u64 {
        // SAFETY: a live context.
        let sdk = unsafe { ffi::engine_brixelizer_gi_gpu_memory(self.raw.as_ptr()) };
        let ours = self
            .depth
            .iter()
            .chain(self.normal.iter())
            .chain(core::iter::once(&self.lit))
            .chain(self.noise.iter())
            .map(Kept::bytes)
            .sum::<u64>();
        sdk + ours
    }

    /// Destroys the view's GI.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it after `frame`; its distance field is still alive.
    pub unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame: u64,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            ffi::engine_brixelizer_gi_destroy(self.raw.as_ptr());
            for kept in self
                .depth
                .iter_mut()
                .chain(self.normal.iter_mut())
                .chain([&mut self.motion, &mut self.lit])
                .chain(self.noise.iter_mut())
            {
                kept.destroy(device, allocator, bindless, frame);
            }
            if let Some((buffer, allocation)) = self.noise_upload.take() {
                device.raw().destroy_buffer(buffer, None);
                allocator.free(allocation);
            }
        }
        self.raw = NonNull::dangling();
    }

    /// Imports this frame's images; uploads the noise and clears the lit image the first
    /// time.
    fn import<'g>(&'g self, fg: &mut FrameGraph<'g>) -> ViewImages {
        let k = self.parity.get();
        let depth = self.depth[k].import(fg, "brixelizer depth");
        let normal = self.normal[k].import(fg, "brixelizer normals");
        let (previous_depth, previous_normal) = if self.history.get() {
            (
                self.depth[1 - k].import(fg, "brixelizer last depth"),
                self.normal[1 - k].import(fg, "brixelizer last normals"),
            )
        } else {
            (depth, normal)
        };
        let lit = self.lit.import(fg, "brixelizer lit image");
        if !self.lit_ready.replace(true) {
            fg.add_pass("brixelizer lit clear")
                .write(lit, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; the graph made the image
                    // a transfer destination.
                    unsafe {
                        ctx.device.cmd_clear_color_image(
                            ctx.cmd,
                            ctx.image(lit),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &vk::ClearColorValue { float32: [0.0; 4] },
                            &[color_range()],
                        );
                    }
                });
        }
        let frame = self.frame.get();
        let noise_index = (frame % NOISE_FRAMES as u64) as usize;
        let noise = self.noise[noise_index].import(fg, "brixelizer blue noise");
        if !self.noise_ready.replace(true)
            && let Some((upload, _)) = self.noise_upload.as_ref()
        {
            let upload = *upload;
            let targets: Vec<Texture> = (0..NOISE_FRAMES)
                .map(|i| {
                    if i == noise_index {
                        noise
                    } else {
                        self.noise[i].import(fg, "brixelizer blue noise")
                    }
                })
                .collect();
            let mut pass = fg.add_pass("brixelizer noise upload");
            for &t in &targets {
                pass = pass.write(t, Access::TransferWrite);
            }
            pass.execute(move |ctx| {
                for (i, &t) in targets.iter().enumerate() {
                    let region = vk::BufferImageCopy {
                        buffer_offset: (i * BLUE_NOISE.len()) as u64,
                        buffer_row_length: 0,
                        buffer_image_height: 0,
                        image_subresource: color_layers(),
                        image_offset: vk::Offset3D::default(),
                        image_extent: vk::Extent3D {
                            width: NOISE_SIZE,
                            height: NOISE_SIZE,
                            depth: 1,
                        },
                    };
                    // SAFETY: recording outside a rendering scope; the graph made the
                    // image a transfer destination; the staging buffer holds every tile.
                    unsafe {
                        ctx.device.cmd_copy_buffer_to_image(
                            ctx.cmd,
                            upload,
                            ctx.image(t),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[region],
                        );
                    }
                }
            });
        }
        ViewImages {
            depth,
            normal,
            previous_depth,
            previous_normal,
            lit,
            noise,
        }
    }
}

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// The GI's internal size for a view of `width` × `height` (`ffx_brixelizergi.cpp`).
fn internal_size(resolution: InternalResolution, width: u32, height: u32) -> (u32, u32) {
    let scale = match resolution {
        InternalResolution::Native => 1.0,
        InternalResolution::Percent75 => 0.75,
        InternalResolution::Percent50 => 0.5,
        InternalResolution::Percent25 => 0.25,
    };
    (
        ((width as f32 * scale) as u32).max(1),
        ((height as f32 * scale) as u32).max(1),
    )
}

/// The 16 noise tiles, each the base tile shifted by the R2 sequence's next step (a
/// Cranley-Patterson rotation per frame), in a staging buffer.
fn noise_staging(allocator: &mut Allocator) -> Result<(vk::Buffer, Allocation), RhiError> {
    let size = (BLUE_NOISE.len() * NOISE_FRAMES) as u64;
    let (buffer, mut allocation) = allocator.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC),
        MemoryLocation::Upload,
    )?;
    if let Some(bytes) = allocation.mapped_slice_mut() {
        // R2: 1 / φ₂ and 1 / φ₂² with φ₂ the plastic number.
        let steps = [0.754_877_7_f32, 0.569_840_3];
        for frame in 0..NOISE_FRAMES {
            let tile = &mut bytes[frame * BLUE_NOISE.len()..(frame + 1) * BLUE_NOISE.len()];
            for (i, (out, &value)) in tile.iter_mut().zip(BLUE_NOISE).enumerate() {
                let shift = (steps[i & 1] * frame as f32).fract() * 256.0;
                *out = ((f32::from(value) + shift) % 256.0) as u8;
            }
        }
    }
    Ok((buffer, allocation))
}

/// Layout changes of the SDK's sampled inputs (the graph leaves them in
/// `READ_ONLY_OPTIMAL`; the SDK reads them in `SHADER_READ_ONLY_OPTIMAL` and leaves them so),
/// after the accesses in `after`.
///
/// # Safety
///
/// Recording outside a rendering scope; the images are in `from`.
unsafe fn transition(
    ctx: &PassContext<'_>,
    images: &[vk::Image],
    after: (vk::PipelineStageFlags2, vk::AccessFlags2),
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let mut barriers = [vk::ImageMemoryBarrier2::default(); 10];
    let mut count = 0;
    for &image in images {
        if image == vk::Image::null() || count == barriers.len() {
            continue;
        }
        barriers[count] = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(after.0)
            .src_access_mask(after.1)
            .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(from)
            .new_layout(to)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(color_range());
        count += 1;
    }
    // SAFETY: guaranteed by the caller.
    unsafe {
        ctx.device.cmd_pipeline_barrier2(
            ctx.cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&barriers[..count]),
        );
    }
}

/// The SDK's view and projection for `camera`: row-vector matrices (glam's column-major
/// arrays), the projection with Direct3D's clip space (y up) and depth inverted (ADR 0090).
fn sdk_view_projection(camera: Camera) -> Option<(Mat4, Mat4)> {
    let view_space = camera.view_space()?;
    let view = view_space.view_from_world;
    let projection = camera.clip_from_world * view.inverse();
    // clip' = (x, −y, w − z, w).
    let convention = Mat4::from_cols_array(&[
        1.0, 0.0, 0.0, 0.0, //
        0.0, -1.0, 0.0, 0.0, //
        0.0, 0.0, -1.0, 0.0, //
        0.0, 0.0, 1.0, 1.0,
    ]);
    Some((view, convention * projection))
}

impl ScreenGi for BrixelizerGi {
    fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        input: &ScreenGiInput,
    ) -> Option<Texture> {
        engine_profiler::zone!("brixelizer::BrixelizerGi::add_passes");
        // Not before every cascade is built: rays that leave the built ones bring the sky
        // into the caches, which fades over hundreds of frames.
        let sdf = self
            .sdf
            .get()
            .filter(|s| s.serial == fg.serial() && s.built)?;
        let (environment, intensity) = input.environment?;
        let depth_desc = fg.texture_desc(input.depth);
        if (depth_desc.width, depth_desc.height) != (self.width, self.height) {
            return None;
        }
        let (view, projection) = sdk_view_projection(input.camera)?;
        if input.reset {
            self.reset();
        }
        let previous = self.previous.get().unwrap_or((view, projection));
        let images = self.import(fg);
        // The view's motion vectors, or the camera's motion made from the depth.
        let own_motion = input.motion.is_none();
        let motion = match input.motion {
            Some(motion) => motion,
            None => self.motion.import(fg, "brixelizer camera motion"),
        };
        let frame = ScreenFrame {
            world_from_clip: input.camera.clip_from_world.inverse().to_cols_array(),
            previous_clip_from_world: input.previous_clip_from_world.to_cols_array(),
            eye: input.camera.position.extend(1.0).to_array(),
            size: [self.width, self.height],
            ndc_per_pixel: [2.0 / self.width as f32, 2.0 / self.height as f32],
            motion: u32::from(own_motion),
            _pad: [0; 3],
        };
        let frame = ring.push(&[frame])?;
        let size = [self.width, self.height, 1];
        // 1. The inputs the forward renderer lacks.
        let depth = input.depth;
        let mut pass = fg
            .add_compute("brixelizer inputs", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .write(images.depth, Access::StorageWrite(ShaderStages::COMPUTE))
            .write(images.normal, Access::StorageWrite(ShaderStages::COMPUTE));
        if own_motion {
            pass = pass.write(motion, Access::StorageWrite(ShaderStages::COMPUTE));
        }
        pass.dispatch(&self.inputs, Dispatch::Threads(size), move |ctx| {
            Some(Push {
                frame,
                depth: ctx.sampled_index(depth)?,
                source: 0,
                target0: ctx.storage_index(images.depth)?,
                target1: ctx.storage_index(images.normal)?,
                target2: if own_motion {
                    ctx.storage_index(motion)?
                } else {
                    0
                },
                _pad: 0,
            })
        });
        // 2. The SDK's GI.
        let desc = TextureDesc::new_2d(GI_FORMAT, self.width, self.height);
        let diffuse = fg.create_texture("brixelizer diffuse", desc);
        let specular = fg.create_texture("brixelizer specular", desc);
        let environment_desc = fg.texture_desc(environment);
        let raw = self.raw;
        let failures = &self.failures;
        let settings = self.settings;
        let end_cascade = self.cascades.saturating_sub(1);
        let (width, height) = (self.width, self.height);
        let image_desc =
            |texture: Texture, desc: TextureDesc, ctx: &PassContext<'_>| engine_ffx::Image {
                image: ctx.image(texture),
                format: desc.format,
                width: desc.width,
                height: desc.height,
            };
        let input_descs = [
            fg.texture_desc(images.depth),
            fg.texture_desc(images.normal),
            fg.texture_desc(images.lit),
            fg.texture_desc(images.noise),
            fg.texture_desc(motion),
        ];
        fg.add_pass("brixelizer gi")
            .read(sdf.token, Access::StorageRead(ShaderStages::COMPUTE))
            .read(images.depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(images.normal, Access::Sampled(ShaderStages::COMPUTE))
            .read(
                images.previous_depth,
                Access::Sampled(ShaderStages::COMPUTE),
            )
            .read(
                images.previous_normal,
                Access::Sampled(ShaderStages::COMPUTE),
            )
            .read(images.lit, Access::Sampled(ShaderStages::COMPUTE))
            .read(images.noise, Access::Sampled(ShaderStages::COMPUTE))
            .read(motion, Access::Sampled(ShaderStages::COMPUTE))
            .read(environment, Access::Sampled(ShaderStages::COMPUTE))
            .write(diffuse, Access::StorageWrite(ShaderStages::COMPUTE))
            .write(specular, Access::StorageWrite(ShaderStages::COMPUTE))
            .execute(move |ctx| {
                let inputs = [
                    ctx.image(images.depth),
                    ctx.image(images.normal),
                    ctx.image(images.previous_depth),
                    ctx.image(images.previous_normal),
                    ctx.image(images.lit),
                    ctx.image(images.noise),
                    ctx.image(motion),
                    ctx.image(environment),
                ];
                // SAFETY: recording outside a rendering scope; the images are the pass's
                // declared inputs, synchronized by the graph for compute sampling. The
                // change of layout is a write: it waits for every earlier access.
                unsafe {
                    transition(
                        ctx,
                        &inputs,
                        (
                            vk::PipelineStageFlags2::ALL_COMMANDS,
                            vk::AccessFlags2::MEMORY_WRITE,
                        ),
                        vk::ImageLayout::READ_ONLY_OPTIMAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    );
                }
                let [depth_desc, normal_desc, lit_desc, noise_desc, motion_desc] = input_descs;
                let desc = ffi::GiDispatchDesc {
                    command_buffer: ctx.cmd,
                    view: view.to_cols_array(),
                    projection: projection.to_cols_array(),
                    prev_view: previous.0.to_cols_array(),
                    prev_projection: previous.1.to_cols_array(),
                    camera_position: frame_eye(&view),
                    start_cascade: 0,
                    end_cascade,
                    ray_pushoff: settings.ray_pushoff,
                    sdf_solve_eps: settings.sdf_solve_eps,
                    specular_ray_pushoff: settings.ray_pushoff,
                    specular_sdf_solve_eps: settings.sdf_solve_eps,
                    t_min: settings.t_min,
                    t_max: settings.t_max,
                    environment_intensity: intensity,
                    // The engine's vectors are now minus last frame's; the SDK adds its
                    // vectors to a position to find last frame's.
                    motion_vector_scale: [-1.0, -1.0],
                    normals_unpack_mul: 1.0,
                    normals_unpack_add: 0.0,
                    // The normals' alpha: roughness 1, no specular rays.
                    roughness_channel: 3,
                    roughness_perceptual: 0,
                    roughness_threshold: 0.9,
                    pad: 0,
                    depth: image_desc(images.depth, depth_desc, ctx),
                    history_depth: image_desc(images.previous_depth, depth_desc, ctx),
                    normal: image_desc(images.normal, normal_desc, ctx),
                    history_normal: image_desc(images.previous_normal, normal_desc, ctx),
                    roughness: image_desc(images.normal, normal_desc, ctx),
                    motion_vectors: image_desc(motion, motion_desc, ctx),
                    prev_lit: image_desc(images.lit, lit_desc, ctx),
                    noise: image_desc(images.noise, noise_desc, ctx),
                    environment: ffi::Cube {
                        image: ctx.image(environment),
                        format: environment_desc.format,
                        size: environment_desc.width,
                        pad: 0,
                    },
                    diffuse: engine_ffx::Image {
                        image: ctx.image(diffuse),
                        format: GI_FORMAT,
                        width,
                        height,
                    },
                    specular: engine_ffx::Image {
                        image: ctx.image(specular),
                        format: GI_FORMAT,
                        width,
                        height,
                    },
                };
                // SAFETY: a live context; the images are alive for the frame and in the
                // layouts the SDK expects (inputs just changed, outputs GENERAL for
                // storage); the distance field's update was recorded before (the token).
                if unsafe { ffi::engine_brixelizer_gi_dispatch(raw.as_ptr(), &desc) } != 0 {
                    failures.set(failures.get() + 1);
                }
                // SAFETY: as above, after the SDK's compute reads.
                unsafe {
                    transition(
                        ctx,
                        &inputs,
                        (
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::NONE,
                        ),
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::READ_ONLY_OPTIMAL,
                    );
                }
            });
        // 3. The GI shading's irradiance.
        let gi = fg.create_texture("brixelizer irradiance", desc);
        fg.add_compute("brixelizer irradiance", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(diffuse, Access::Sampled(ShaderStages::COMPUTE))
            .write(gi, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(&self.irradiance, Dispatch::Threads(size), move |ctx| {
                Some(Push {
                    frame,
                    depth: ctx.sampled_index(depth)?,
                    source: ctx.sampled_index(diffuse)?,
                    target0: ctx.storage_index(gi)?,
                    target1: 0,
                    target2: 0,
                    _pad: 0,
                })
            });
        self.last.set(Some(LastFrame {
            serial: fg.serial(),
            depth: images.depth,
            normal: images.normal,
            diffuse,
            view,
            projection,
        }));
        self.previous.set(Some((view, projection)));
        self.parity.set(1 - self.parity.get());
        self.history.set(true);
        self.frame.set(self.frame.get() + 1);
        Some(gi)
    }
}

/// The camera's position from a rigid view matrix.
fn frame_eye(view: &Mat4) -> [f32; 3] {
    view.inverse().w_axis.truncate().to_array()
}
