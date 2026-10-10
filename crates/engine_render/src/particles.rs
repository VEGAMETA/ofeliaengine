//! GPU particles (ADR 0081): a pool that lives on the GPU, emitted, simulated, sorted back
//! to front and drawn as camera-facing quads, without the CPU reading anything back. The
//! CPU only says, per frame, what each emitter spawns and where it is.
//!
//! ```text
//! let mut particles = ParticleSystem::new(&device, &mut allocator, &bindless, 65_536, HDR_FORMAT)?;
//! let fountain = particles.add_emitter(ParticleEmitter { rate: 2_000.0, ..ParticleEmitter::default() });
//!
//! // Each frame, before the frame renderer's view (the ring takes the emitters), with the
//! // camera the particles are sorted for:
//! let frame = particles.prepare(&mut ring, dt, camera.position, forward, camera.far);
//! // In `FrameRenderer::add_view_with`'s hook, with the scene's targets:
//! if let Some(frame) = frame {
//!     particles.add_passes(fg, &bindless, &frame, &ParticleTarget::of(targets));
//! }
//! ```
//!
//! - **Emitters** spawn `rate` particles per second (the fraction carried to the next
//!   frame) and bursts, inside a ball around their transform, moving into a cone. Each
//!   particle falls with the emitter's acceleration, slows with its drag, spins, and
//!   passes from the birth size and color to the death ones over its life.
//! - **Determinism:** a particle's random numbers come from its birth number and its
//!   emitter's seed: the same emitters and steps make the same particles, wherever they
//!   land in the pool. Equal depths sort in birth order.
//! - **Drawing:** blended premultiplied (an emitter's `additive` share adds instead of
//!   covering), unlit, textured or a soft disc, faded where it meets the scene (soft
//!   particles), into the reactive mask when the view has one (temporal antialiasing
//!   trusts its history less there).

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::forward::{Camera, REACTIVE_FORMAT};
use crate::graph::{
    Access, BufferDesc, BufferState, FrameGraph, ImportedBuffer, LoadOp, ShaderStages, StoreOp,
    Texture,
};
use crate::sort::{GpuSort, SortCount, SortJob};
use crate::upload::UploadRing;
use engine_core::math::{Affine3A, Mat4, Vec3, Vec4};
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, Blend, Device, GraphicsPipelineDesc, LazyPipeline,
    MemoryLocation, PipelineDesc, RhiError, SamplerKind, ShaderStageDesc, spirv_words,
};

const INIT: &[u8] = include_bytes!("../shaders/particles.particlesInit.spv");
const BEGIN: &[u8] = include_bytes!("../shaders/particles.particlesBegin.spv");
const EMIT: &[u8] = include_bytes!("../shaders/particles.particlesEmit.spv");
const SIMULATE_ARGS: &[u8] = include_bytes!("../shaders/particles.particlesSimulateArgs.spv");
const SIMULATE: &[u8] = include_bytes!("../shaders/particles.particlesSimulate.spv");
const FINISH: &[u8] = include_bytes!("../shaders/particles.particlesFinish.spv");
const VERTEX: &[u8] = include_bytes!("../shaders/particles.particlesVertex.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/particles.particlesFragment.spv");
const FRAGMENT_REACTIVE: &[u8] =
    include_bytes!("../shaders/particles.particlesFragmentReactive.spv");

/// `NO_TEXTURE` in `particles.slang`: the soft disc.
const NO_TEXTURE: u32 = u32::MAX;
const PARTICLE_BYTES: u64 = 48;
const COUNTERS_BYTES: u64 = 64;
/// Byte offsets in `Counters` of the alive count and the dispatch and draw arguments.
const ALIVE: u64 = 0;
const EMIT_ARGS: u64 = 16;
const SIMULATE_ARGS_AT: u64 = 32;
const DRAW_ARGS: u64 = 48;

/// What the pool's buffers were last used for, between frames.
const PARTICLE_ACCESS: BufferState = BufferState {
    stages: vk::PipelineStageFlags2::from_raw(
        vk::PipelineStageFlags2::COMPUTE_SHADER.as_raw()
            | vk::PipelineStageFlags2::VERTEX_SHADER.as_raw()
            | vk::PipelineStageFlags2::DRAW_INDIRECT.as_raw(),
    ),
    access: vk::AccessFlags2::from_raw(
        vk::AccessFlags2::SHADER_STORAGE_READ.as_raw()
            | vk::AccessFlags2::SHADER_STORAGE_WRITE.as_raw()
            | vk::AccessFlags2::INDIRECT_COMMAND_READ.as_raw(),
    ),
};

/// An emitter's description (see the module's documentation). Units are the scene's
/// (meters) and seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParticleEmitter {
    /// World from emitter: particles spawn around its origin, into its cone.
    pub transform: Affine3A,
    /// Particles per second.
    pub rate: f32,
    /// Seconds a particle lives: uniform between the two.
    pub lifetime: [f32; 2],
    /// Particles spawn uniformly in a ball of this radius (0: at the origin).
    pub radius: f32,
    /// The cone's axis, in the emitter's space.
    pub direction: Vec3,
    /// The cone's half angle in radians (0: along the axis; π: every direction).
    pub spread: f32,
    /// Speed at birth: uniform between the two.
    pub speed: [f32; 2],
    /// World acceleration (gravity).
    pub acceleration: Vec3,
    /// Share of the velocity lost per second.
    pub drag: f32,
    /// Size (width and height) at birth and at death.
    pub size: [f32; 2],
    /// Linear color and opacity at birth and at death.
    pub color: [Vec4; 2],
    /// 0: blended over what is behind; 1: added to it (fire, sparks); between: both.
    pub additive: f32,
    /// Spin at birth in radians per second: uniform between the two.
    pub spin: [f32; 2],
    /// A bindless sampled texture, or `None` for a soft disc.
    pub texture: Option<u32>,
    /// Particles fade out over this distance in front of the scene (0: a hard edge).
    pub soft_distance: f32,
    /// Seed of the emitter's random numbers.
    pub seed: u32,
}

impl Default for ParticleEmitter {
    fn default() -> Self {
        Self {
            transform: Affine3A::IDENTITY,
            rate: 100.0,
            lifetime: [1.0, 2.0],
            radius: 0.0,
            direction: Vec3::Y,
            spread: 0.3,
            speed: [1.0, 2.0],
            acceleration: Vec3::new(0.0, -9.81, 0.0),
            drag: 0.0,
            size: [0.1, 0.1],
            color: [Vec4::ONE, Vec4::new(1.0, 1.0, 1.0, 0.0)],
            additive: 0.0,
            spin: [0.0, 0.0],
            texture: None,
            soft_distance: 0.2,
            seed: 0,
        }
    }
}

/// An emitter of a [`ParticleSystem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EmitterId(u32);

/// `GpuEmitter` / `Emitter` in `particles.slang` (176 bytes).
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct GpuEmitter {
    rows: [[f32; 4]; 3],
    color_start: [f32; 4],
    color_end: [f32; 4],
    accel_drag: [f32; 4],
    direction_spread: [f32; 4],
    shape: [f32; 4],
    ranges: [f32; 4],
    spin_additive: [f32; 4],
    spawn: [u32; 4],
}

impl GpuEmitter {
    fn new(e: &ParticleEmitter, first: u32, count: u32) -> Self {
        let m = e.transform.matrix3;
        let t = e.transform.translation;
        let direction = e.direction.normalize_or(Vec3::Y);
        Self {
            rows: [
                [m.x_axis.x, m.y_axis.x, m.z_axis.x, t.x],
                [m.x_axis.y, m.y_axis.y, m.z_axis.y, t.y],
                [m.x_axis.z, m.y_axis.z, m.z_axis.z, t.z],
            ],
            color_start: e.color[0].to_array(),
            color_end: e.color[1].to_array(),
            accel_drag: [
                e.acceleration.x,
                e.acceleration.y,
                e.acceleration.z,
                e.drag.max(0.0),
            ],
            direction_spread: [
                direction.x,
                direction.y,
                direction.z,
                e.spread.clamp(0.0, core::f32::consts::PI).cos(),
            ],
            shape: [
                e.radius.max(0.0),
                e.size[0],
                e.size[1],
                e.soft_distance.max(0.0),
            ],
            ranges: [e.lifetime[0], e.lifetime[1], e.speed[0], e.speed[1]],
            spin_additive: [e.spin[0], e.spin[1], e.additive.clamp(0.0, 1.0), 1.0],
            spawn: [first, count, e.texture.unwrap_or(NO_TEXTURE), e.seed],
        }
    }

    /// A slot without an emitter: nothing spawns, and its particles die.
    fn dead() -> Self {
        bytemuck::Zeroable::zeroed()
    }
}

/// `ParticlePush` in `particles.slang` (112 bytes).
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct ParticlePush {
    particles: u64,
    alive_in: u64,
    alive_out: u64,
    dead: u64,
    counters: u64,
    emitters: u64,
    keys: u64,
    values: u64,
    eye_dt: [f32; 4],
    forward_far: [f32; 4],
    requested: u32,
    serial: u32,
    capacity: u32,
    emitters_count: u32,
}

/// `DrawPush` in `particles.slang` (128 bytes).
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct DrawPush {
    particles: u64,
    order: u64,
    emitters: u64,
    depth: u32,
    sampler: u32,
    clip_from_world: [f32; 16],
    right_p22: [f32; 4],
    up_p32: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Slot {
    Free,
    /// Removed: one more frame as a dead entry (its particles die), then free.
    Dying,
    Live {
        emitter: ParticleEmitter,
        /// The fraction of a particle carried to the next frame.
        carry: f32,
        /// Particles to spawn at once in the next frame.
        burst: u32,
    },
}

/// One frame's step, from [`ParticleSystem::prepare`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParticleFrame {
    emitters: u64,
    emitters_count: u32,
    requested: u32,
    serial: u32,
    dt: f32,
    eye: Vec3,
    forward: Vec3,
    far: f32,
    parity: usize,
    init: bool,
}

/// Where and how a frame's particles are drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParticleTarget {
    /// The scene's HDR color, drawn over.
    pub color: Texture,
    /// The scene's depth (0 near, 1 far): particles behind it are hidden, those just in
    /// front fade.
    pub depth: Texture,
    /// The view's reactive mask, when it has one ([`crate::forward::MotionOutput`]).
    pub reactive: Option<Texture>,
    /// Clip from world, jitter included: the scene's (`MotionOutput::clip_from_world`
    /// with temporal antialiasing, the camera's without).
    pub clip_from_world: Mat4,
    /// The camera without jitter (its right and up vectors, its depth terms).
    pub camera: Camera,
}

/// A pool of GPU particles and its emitters.
pub struct ParticleSystem {
    capacity: u32,
    particles: PoolBuffer,
    alive: [PoolBuffer; 2],
    dead: PoolBuffer,
    counters: PoolBuffer,
    slots: Vec<Slot>,
    /// This frame's emitter table (kept to avoid allocating every frame).
    table: Vec<GpuEmitter>,
    serial: u32,
    parity: usize,
    initialized: bool,
    sort: GpuSort,
    init: ComputeShader<ParticlePush>,
    begin: ComputeShader<ParticlePush>,
    emit: ComputeShader<ParticlePush>,
    simulate_args: ComputeShader<ParticlePush>,
    simulate: ComputeShader<ParticlePush>,
    finish: ComputeShader<ParticlePush>,
    /// The draw: into the color, and also into the reactive mask.
    draw: [LazyPipeline; 2],
}

impl core::fmt::Debug for ParticleSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ParticleSystem")
            .field("capacity", &self.capacity)
            .field("emitters", &self.slots.len())
            .finish_non_exhaustive()
    }
}

/// A buffer of the pool, with its device address.
#[derive(Debug)]
struct PoolBuffer {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    size: u64,
}

impl PoolBuffer {
    fn new(device: &Device, allocator: &mut Allocator, size: u64) -> Result<Self, RhiError> {
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(size).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::INDIRECT_BUFFER
                    | vk::BufferUsageFlags::TRANSFER_SRC
                    | vk::BufferUsageFlags::TRANSFER_DST
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
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
            buffer,
            allocation: Some(allocation),
            address,
            size,
        })
    }

    /// # Safety
    ///
    /// The GPU no longer uses it.
    unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

impl ParticleSystem {
    /// A pool of `capacity` particles, drawn into targets of `color_format` (the scene's
    /// HDR color). Its memory: 60 bytes per particle, and as much again for a frame's
    /// sort.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        capacity: u32,
        color_format: vk::Format,
    ) -> Result<Self, RhiError> {
        let capacity = capacity.max(1);
        let list = u64::from(capacity) * 4;
        let source = |spirv, entry| ComputeSource::<ParticlePush>::new(spirv, entry, &[]);
        let [init, begin, emit, simulate_args, simulate, finish] = ComputeSource::create_all(
            device,
            bindless,
            [
                source(INIT, c"particlesInit")?,
                source(BEGIN, c"particlesBegin")?,
                source(EMIT, c"particlesEmit")?,
                source(SIMULATE_ARGS, c"particlesSimulateArgs")?,
                source(SIMULATE, c"particlesSimulate")?,
                source(FINISH, c"particlesFinish")?,
            ],
        )?;
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let vertex = spirv_words(VERTEX).ok_or_else(invalid)?;
        let fragment = spirv_words(FRAGMENT).ok_or_else(invalid)?;
        let fragment_reactive = spirv_words(FRAGMENT_REACTIVE).ok_or_else(invalid)?;
        let formats = [color_format];
        let reactive_formats = [color_format, REACTIVE_FORMAT];
        let draw = |fragment, entry, color_formats| {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                color_formats,
                cull_mode: vk::CullModeFlags::NONE,
                depth_test: false,
                depth_write: false,
                blend: Blend::Premultiplied,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &vertex,
                        entry: c"particlesVertex",
                    },
                    Some(ShaderStageDesc {
                        spirv: fragment,
                        entry,
                    }),
                )
            })
        };
        let described = LazyPipeline::describe_many(
            device,
            bindless.shared_pipeline_layout(),
            &[
                draw(&fragment, c"particlesFragment", &formats[..]),
                draw(
                    &fragment_reactive,
                    c"particlesFragmentReactive",
                    &reactive_formats[..],
                ),
            ],
        );
        let [plain, reactive]: [LazyPipeline; 2] =
            described.try_into().map_err(|_| RhiError::Vulkan {
                call: "particle pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })?;
        let mut buffers = Vec::with_capacity(5);
        for size in [
            u64::from(capacity) * PARTICLE_BYTES,
            list,
            list,
            list,
            COUNTERS_BYTES,
        ] {
            match PoolBuffer::new(device, allocator, size) {
                Ok(b) => buffers.push(b),
                Err(e) => {
                    for mut b in buffers {
                        // SAFETY: never used by the GPU.
                        unsafe { b.destroy(allocator) };
                    }
                    return Err(e);
                }
            }
        }
        let mut buffers = buffers.into_iter();
        let mut next = || {
            buffers.next().ok_or(RhiError::Vulkan {
                call: "particle buffers",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        Ok(Self {
            capacity,
            particles: next()?,
            alive: [next()?, next()?],
            dead: next()?,
            counters: next()?,
            slots: Vec::new(),
            table: Vec::new(),
            serial: 0,
            parity: 0,
            initialized: false,
            sort: GpuSort::new(device, bindless)?,
            init,
            begin,
            emit,
            simulate_args,
            simulate,
            finish,
            draw: [plain, reactive],
        })
    }

    /// Particles the pool holds at most.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// The pool's buffers, for tools and tests that read particles back after a frame:
    /// the particles (48 bytes each, `Particle` in `particles.slang`), the counters
    /// (`Counters`: alive, dead, ...), and the alive list the last declared frame wrote.
    pub fn debug_buffers(&self) -> [vk::Buffer; 3] {
        [
            self.particles.buffer,
            self.counters.buffer,
            self.alive[self.parity].buffer,
        ]
    }

    /// Adds an emitter; it spawns from the next frame on.
    pub fn add_emitter(&mut self, emitter: ParticleEmitter) -> EmitterId {
        let live = Slot::Live {
            emitter,
            carry: 0.0,
            burst: 0,
        };
        match self.slots.iter().position(|s| *s == Slot::Free) {
            Some(i) => {
                self.slots[i] = live;
                EmitterId(i as u32)
            }
            None => {
                self.slots.push(live);
                EmitterId((self.slots.len() - 1) as u32)
            }
        }
    }

    /// The emitter `id`, to change (moved with its object, its rate turned down...).
    pub fn emitter_mut(&mut self, id: EmitterId) -> Option<&mut ParticleEmitter> {
        match self.slots.get_mut(id.0 as usize) {
            Some(Slot::Live { emitter, .. }) => Some(emitter),
            _ => None,
        }
    }

    /// Spawns `count` more particles from `id` in the next frame (an explosion).
    pub fn burst(&mut self, id: EmitterId, count: u32) {
        if let Some(Slot::Live { burst, .. }) = self.slots.get_mut(id.0 as usize) {
            *burst = burst.saturating_add(count);
        }
    }

    /// Removes the emitter `id`; its particles die at the next step. (To let them live
    /// on, set its rate to 0 and remove it later.)
    pub fn remove_emitter(&mut self, id: EmitterId) {
        if let Some(slot) = self.slots.get_mut(id.0 as usize)
            && matches!(slot, Slot::Live { .. })
        {
            *slot = Slot::Dying;
        }
    }

    /// Kills every particle at the next step (a level change).
    pub fn reset(&mut self) {
        self.initialized = false;
    }

    /// Steps the emitters by `dt` seconds and uploads them into `ring`, for this frame's
    /// [`Self::add_passes`]. The particles are sorted for a camera at `eye` looking along
    /// `forward` (unit), whose depths end at `far`. `None` when the ring had no room
    /// (nothing changed).
    pub fn prepare(
        &mut self,
        ring: &mut UploadRing,
        dt: f32,
        eye: Vec3,
        forward: Vec3,
        far: f32,
    ) -> Option<ParticleFrame> {
        let dt = dt.max(0.0);
        self.table.clear();
        let mut first = 0u32;
        for slot in &self.slots {
            self.table.push(match slot {
                Slot::Live {
                    emitter,
                    carry,
                    burst,
                } => {
                    let count = (carry + emitter.rate.max(0.0) * dt).floor() as u32 + burst;
                    let entry = GpuEmitter::new(emitter, first, count);
                    first = first.saturating_add(count);
                    entry
                }
                Slot::Dying | Slot::Free => GpuEmitter::dead(),
            });
        }
        if self.table.is_empty() {
            self.table.push(GpuEmitter::dead());
        }
        let emitters = ring.push(&self.table)?;
        // Committed: the emitters' carries, bursts and slots move on.
        for slot in &mut self.slots {
            match slot {
                Slot::Live {
                    emitter,
                    carry,
                    burst,
                } => {
                    let total = *carry + emitter.rate.max(0.0) * dt;
                    *carry = total - total.floor();
                    *burst = 0;
                }
                Slot::Dying => *slot = Slot::Free,
                Slot::Free => {}
            }
        }
        let far = if far.is_finite() && far > 0.0 {
            far
        } else {
            1000.0
        };
        let frame = ParticleFrame {
            emitters,
            emitters_count: self.table.len() as u32,
            requested: first,
            serial: self.serial,
            dt,
            eye,
            forward: forward.normalize_or(Vec3::NEG_Z),
            far,
            parity: self.parity,
            init: !self.initialized,
        };
        self.serial = self.serial.wrapping_add(first);
        self.parity ^= 1;
        self.initialized = true;
        Some(frame)
    }

    /// Declares `frame`'s passes: emission and simulation, the sort back to front, and
    /// the draw into `target`'s color (and reactive mask).
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        frame: &ParticleFrame,
        target: &ParticleTarget,
    ) {
        let initial = if frame.init {
            BufferState::idle()
        } else {
            PARTICLE_ACCESS
        };
        let import = |fg: &mut FrameGraph<'g>, name, b: &PoolBuffer| {
            fg.import_buffer(
                name,
                ImportedBuffer {
                    buffer: b.buffer,
                    size: b.size,
                    address: b.address,
                    initial,
                    final_state: Some(PARTICLE_ACCESS),
                },
            )
        };
        let particles = import(fg, "particles", &self.particles);
        let alive = [
            import(fg, "particles alive", &self.alive[0]),
            import(fg, "particles alive (other)", &self.alive[1]),
        ];
        let dead = import(fg, "particles dead", &self.dead);
        let counters = import(fg, "particle counters", &self.counters);
        let list = u64::from(self.capacity) * 4;
        let keys = fg.create_buffer("particle keys", BufferDesc { size: list });
        let order = fg.create_buffer("particle order", BufferDesc { size: list });
        let (alive_in, alive_out) = (alive[frame.parity], alive[1 - frame.parity]);
        let frame = *frame;
        let rw = Access::StorageReadWrite(ShaderStages::COMPUTE);
        fg.add_compute("particles simulate", bindless)
            .write(particles, rw)
            .write(alive_in, rw)
            .write(alive_out, rw)
            .write(dead, rw)
            .write(counters, rw)
            .read(counters, Access::Indirect)
            .write(keys, rw)
            .write(order, rw)
            .record(move |rec| {
                let ctx = rec.context();
                let push = ParticlePush {
                    particles: ctx.address(particles),
                    alive_in: ctx.address(alive_in),
                    alive_out: ctx.address(alive_out),
                    dead: ctx.address(dead),
                    counters: ctx.address(counters),
                    emitters: frame.emitters,
                    keys: ctx.address(keys),
                    values: ctx.address(order),
                    eye_dt: [frame.eye.x, frame.eye.y, frame.eye.z, frame.dt],
                    forward_far: [frame.forward.x, frame.forward.y, frame.forward.z, frame.far],
                    requested: frame.requested,
                    serial: frame.serial,
                    capacity: self.capacity,
                    emitters_count: frame.emitters_count,
                };
                let args = ctx.buffer(counters);
                if frame.init {
                    rec.dispatch(&self.init, Dispatch::Threads([self.capacity, 1, 1]), &push);
                    rec.barrier();
                }
                rec.dispatch(&self.begin, Dispatch::Groups([1, 1, 1]), &push);
                rec.barrier_for_indirect();
                rec.dispatch_indirect(&self.emit, args, EMIT_ARGS, &push);
                rec.barrier();
                rec.dispatch(&self.simulate_args, Dispatch::Groups([1, 1, 1]), &push);
                rec.barrier_for_indirect();
                rec.dispatch_indirect(&self.simulate, args, SIMULATE_ARGS_AT, &push);
                rec.barrier();
                rec.dispatch(&self.finish, Dispatch::Groups([1, 1, 1]), &push);
            });
        // Back to front, equal depths in birth order: 16 bits of depth, 16 of birth.
        self.sort.add_pass(
            fg,
            bindless,
            "particles sort",
            SortJob {
                keys,
                values: Some(order),
                capacity: self.capacity,
                count: SortCount::Gpu {
                    buffer: counters,
                    offset: ALIVE,
                },
                key_bits: 32,
            },
        );

        let Some(view) = target.camera.view_space() else {
            // Orthographic views: not drawn yet (ADR 0081).
            return;
        };
        let right = view.view_from_world.row(0).truncate();
        let up = view.view_from_world.row(1).truncate();
        let pipeline = &self.draw[usize::from(target.reactive.is_some())];
        let (color, depth, reactive) = (target.color, target.depth, target.reactive);
        let clip_from_world = target.clip_from_world.to_cols_array();
        let vertex = Access::StorageRead(ShaderStages::VERTEX);
        let mut pass = fg
            .add_pass("particles")
            .color(color, LoadOp::Load, StoreOp::Store);
        if let Some(reactive) = reactive {
            pass = pass.color(reactive, LoadOp::Load, StoreOp::Store);
        }
        pass.read(depth, Access::Sampled(ShaderStages::FRAGMENT))
            .read(particles, vertex)
            .read(order, vertex)
            .read(counters, Access::Indirect)
            .execute(move |ctx| {
                let Some(pipeline) = pipeline.get_or_create().ok() else {
                    return;
                };
                let Some(depth_index) = ctx.sampled_index(depth) else {
                    return;
                };
                let push = DrawPush {
                    particles: ctx.address(particles),
                    order: ctx.address(order),
                    emitters: frame.emitters,
                    depth: depth_index,
                    sampler: Bindless::sampler(SamplerKind::LinearClamp),
                    clip_from_world,
                    right_p22: [right.x, right.y, right.z, view.p22],
                    up_p32: [up.x, up.y, up.z, view.p32],
                };
                let area = ctx.render_area();
                // SAFETY: recording inside the pass's rendering scope; the pass reads the
                // pool, the order and the draw arguments it declared.
                unsafe {
                    pipeline.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    ctx.device.cmd_set_viewport(
                        ctx.cmd,
                        0,
                        &[vk::Viewport {
                            x: area.offset.x as f32,
                            y: area.offset.y as f32,
                            width: area.extent.width as f32,
                            height: area.extent.height as f32,
                            min_depth: 0.0,
                            max_depth: 1.0,
                        }],
                    );
                    ctx.device.cmd_set_scissor(ctx.cmd, 0, &[area]);
                    bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
                    ctx.device
                        .cmd_draw_indirect(ctx.cmd, ctx.buffer(counters), DRAW_ARGS, 1, 16);
                }
            });
    }

    /// Frees the pool's memory.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.particles.destroy(allocator);
            self.alive[0].destroy(allocator);
            self.alive[1].destroy(allocator);
            self.dead.destroy(allocator);
            self.counters.destroy(allocator);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_the_shader() {
        assert_eq!(core::mem::size_of::<GpuEmitter>(), 176);
        assert_eq!(core::mem::size_of::<ParticlePush>(), 112);
        assert_eq!(core::mem::size_of::<DrawPush>(), 128);
    }

    #[test]
    fn an_emitter_row_major_transform_places_its_origin() {
        let e = ParticleEmitter {
            transform: Affine3A::from_translation(Vec3::new(1.0, 2.0, 3.0)),
            ..ParticleEmitter::default()
        };
        let g = GpuEmitter::new(&e, 5, 7);
        assert_eq!([g.rows[0][3], g.rows[1][3], g.rows[2][3]], [1.0, 2.0, 3.0]);
        assert_eq!(g.spawn[0..2], [5, 7]);
        assert_eq!(g.spawn[2], NO_TEXTURE);
    }
}
