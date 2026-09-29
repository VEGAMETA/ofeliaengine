//! Stress scene (CLAUDE.md §5, §17: a measured demo scene per milestone).
//!
//! Everything the engine has so far, at once, with a per-subsystem frame-time breakdown
//! against the 240 FPS target (4.17 ms per frame):
//! - **Luau:** `npcs` agents wander between random targets. The script updates them in
//!   one call per frame and writes their positions into a `buffer`.
//! - **Jolt:** `bodies` mixed shapes in a pile, kicked upward every 2 s so they stay
//!   awake.
//! - **ECS:**
//!   - `decor` entities in 100 spinning hierarchies (roots rotate every frame, so every
//!     world transform is recomputed);
//!   - plus one entity per body and per NPC.
//! - **Rendering:** every entity as a PBR cube with shadows, at 1920×1080 into HDR, then
//!   tonemapped, presented with `immediate` (uncapped). One draw per object: there is no
//!   GPU-driven path yet.
//!
//! ```text
//! cargo run --release -p engine_stress -- [frames] [decor] [bodies] [npcs] [--headless]
//! ```
//!
//! `--headless` runs the simulation parts only (any OS). The report is Markdown on stdout.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Quat, Vec3};
use engine_ecs::{Entity, GlobalTransform, Transform, TransformPropagation, World, pod_component};
use engine_physics::{BodyId, PhysicsBackend, PhysicsSettings, scenes};
use engine_physics_jolt::JoltBackend;
use engine_script::{ScriptSettings, ScriptVm, mlua};
use std::time::Instant;

/// Simulated time per frame: one 240 Hz tick, headless and windowed alike, so every frame
/// does the same work whatever the frame rate.
const TICK: f32 = 1.0 / 240.0;

/// How a cube is rendered: its paint (an index into `Sim::paints`) and scale.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Look {
    pub paint: u32,
    pub scale: f32,
}
pod_component!(Look);

/// A cube's surface: one material each.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Paint {
    pub color: [f32; 3],
    pub metallic: f32,
    pub roughness: f32,
}

const NPC_SCRIPT: &str = r#"
local state = ...
local M = {}

-- Deterministic pseudo-random numbers (LCG), no allocation.
local seed = 12345
local function rand(): number
    seed = (seed * 1103515245 + 12345) % 2147483648
    return seed / 2147483648
end

local function random_target(): vector
    return vector.create(rand() * 60 - 30, 0.5, rand() * 60 - 30)
end

function M.init(n: number)
    state.n = n
    state.pos = table.create(n)
    state.vel = table.create(n)
    state.target = table.create(n)
    for i = 1, n do
        state.pos[i] = random_target()
        state.vel[i] = vector.zero
        state.target[i] = random_target()
    end
end

-- Steer toward the target, slow down near it, pick a new one on arrival, keep out of
-- the central pile; write positions to `out` (3 × f32 per NPC).
function M.update(dt: number, out: buffer)
    local pos, vel, target = state.pos, state.vel, state.target
    for i = 1, state.n do
        local p = pos[i]
        local to_target = target[i] - p
        local dist = vector.magnitude(to_target)
        if dist < 1 then
            target[i] = random_target()
        else
            local speed = math.min(dist, 4)
            local desired = to_target / dist * speed
            local away = p - vector.create(0, p.y, 0)
            local d_center = vector.magnitude(away)
            if d_center < 8 and d_center > 0.001 then
                desired += away / d_center * (8 - d_center)
            end
            local v = vel[i] + (desired - vel[i]) * math.min(dt * 4, 1)
            vel[i] = v
            p += v * dt
            pos[i] = p
        end
        local o = (i - 1) * 12
        buffer.writef32(out, o, p.x)
        buffer.writef32(out, o + 4, p.y)
        buffer.writef32(out, o + 8, p.z)
    end
end

return M
"#;

/// Times of one frame's stages, in microseconds.
#[derive(Default, Clone, Copy)]
struct Stages {
    script: f64,
    physics: f64,
    sync: f64,
    transforms: f64,
    draw_list: f64,
    record: f64,
    gpu: f64,
    frame: f64,
}

struct Sim {
    world: World,
    physics: JoltBackend,
    bodies: Vec<BodyId>,
    body_entities: Vec<Entity>,
    npc_entities: Vec<Entity>,
    roots: Vec<Entity>,
    vm: ScriptVm,
    npc_out: mlua::Buffer,
    npc_bytes: Vec<u8>,
    body_poses: Vec<(Vec3, Quat)>,
    propagation: TransformPropagation,
    /// The distinct surfaces of the cubes.
    paints: Vec<Paint>,
    frame: u64,
}

impl Sim {
    fn new(decor: usize, bodies: u32, npcs: usize) -> Self {
        let mut world = World::new();
        let spawn = |world: &mut World, t: Transform, look: Look| {
            world.spawn((t, GlobalTransform::IDENTITY, look))
        };
        let mut paints: Vec<Paint> = Vec::new();
        let mut look = |color: [f32; 3], metallic: f32, roughness: f32, scale: f32| {
            let paint = Paint {
                color,
                metallic,
                roughness,
            };
            let index = paints.iter().position(|p| *p == paint).unwrap_or_else(|| {
                paints.push(paint);
                paints.len() - 1
            });
            Look {
                paint: index as u32,
                scale,
            }
        };
        // Decor: 100 hierarchies on a ring, each a root with children in a helix.
        let per_root = (decor / 100).max(1);
        let mut roots = Vec::new();
        for r in 0..100 {
            let angle = r as f32 / 100.0 * core::f32::consts::TAU;
            let root = spawn(
                &mut world,
                Transform::from_xyz(angle.cos() * 40.0, 1.0, angle.sin() * 40.0),
                look([0.8, 0.8, 0.85], 1.0, 0.3, 0.6),
            );
            roots.push(root);
            for c in 1..per_root {
                let a = c as f32 * 0.5;
                let child = spawn(
                    &mut world,
                    Transform::from_xyz(a.cos() * 1.5, c as f32 * 0.12, a.sin() * 1.5),
                    look(
                        [0.2 + (r % 5) as f32 * 0.15, 0.4, 0.9 - (c % 7) as f32 * 0.1],
                        0.0,
                        0.5,
                        0.25,
                    ),
                );
                world.set_parent(child, Some(root)).unwrap();
            }
        }
        let mut physics = JoltBackend::new(&PhysicsSettings::default()).unwrap();
        let bodies_ids = scenes::pile(&mut physics, bodies).unwrap();
        let body_entities = (0..bodies_ids.len())
            .map(|i| {
                spawn(
                    &mut world,
                    Transform::IDENTITY,
                    look([0.9, 0.5 + (i % 3) as f32 * 0.15, 0.2], 0.0, 0.6, 0.8),
                )
            })
            .collect();
        let npc_entities = (0..npcs)
            .map(|_| {
                spawn(
                    &mut world,
                    Transform::IDENTITY,
                    look([0.2, 0.9, 0.3], 0.0, 0.4, 0.5),
                )
            })
            .collect();
        let mut vm = ScriptVm::new(ScriptSettings::default()).unwrap();
        vm.load_module("npc", NPC_SCRIPT).unwrap();
        vm.call::<_, ()>("npc", "init", npcs).unwrap();
        let npc_out = vm.lua().create_buffer(vec![0u8; npcs * 12]).unwrap();
        let propagation = TransformPropagation::new(&mut world);
        Self {
            world,
            physics,
            body_poses: vec![(Vec3::ZERO, Quat::IDENTITY); bodies_ids.len()],
            bodies: bodies_ids,
            body_entities,
            npc_entities,
            roots,
            vm,
            npc_out,
            npc_bytes: vec![0; npcs * 12],
            propagation,
            paints,
            frame: 0,
        }
    }

    /// Runs the simulation stages of one frame and records their times.
    fn update(&mut self, dt: f32, t: f32, stages: &mut Stages) {
        let us = |start: Instant| start.elapsed().as_secs_f64() * 1e6;

        let start = Instant::now();
        self.vm
            .call::<_, ()>("npc", "update", (dt, self.npc_out.clone()))
            .unwrap();
        stages.script = us(start);

        let start = Instant::now();
        if self.frame.is_multiple_of(480) {
            for (i, &b) in self.bodies.iter().enumerate() {
                let side = if i % 2 == 0 { 1.0 } else { -1.0 };
                self.physics
                    .add_impulse(b, Vec3::new(side * 0.5, 6.0, 0.3 * side));
            }
        }
        self.physics.step(dt, 1).unwrap();
        stages.physics = us(start);

        let start = Instant::now();
        self.physics.transforms(&self.bodies, &mut self.body_poses);
        for (&e, &(p, r)) in self.body_entities.iter().zip(&self.body_poses) {
            if let Some(tr) = self.world.get_mut::<Transform>(e) {
                tr.translation = p;
                tr.rotation = r;
            }
        }
        // One copy out of the script's buffer per frame (no per-NPC calls).
        let bytes = self.npc_out.to_vec();
        self.npc_bytes.copy_from_slice(&bytes);
        for (i, &e) in self.npc_entities.iter().enumerate() {
            let f = |k: usize| {
                let o = i * 12 + k * 4;
                f32::from_le_bytes([
                    self.npc_bytes[o],
                    self.npc_bytes[o + 1],
                    self.npc_bytes[o + 2],
                    self.npc_bytes[o + 3],
                ])
            };
            let p = Vec3::new(f(0), f(1), f(2));
            if let Some(tr) = self.world.get_mut::<Transform>(e) {
                tr.translation = p;
            }
        }
        for (k, &root) in self.roots.iter().enumerate() {
            if let Some(tr) = self.world.get_mut::<Transform>(root) {
                tr.rotation = Quat::from_rotation_y(t * (0.5 + (k % 7) as f32 * 0.1));
            }
        }
        stages.sync = us(start);

        let start = Instant::now();
        self.propagation.run(&mut self.world);
        stages.transforms = us(start);
        self.frame += 1;
    }
}

/// Reads one stage's time from a sample.
type Stage = fn(&Stages) -> f64;

fn percentile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn report(samples: &[Stages], title: &str, extra: &str) {
    const BUDGET_US: f64 = 1e6 / 240.0;
    let column = |f: fn(&Stages) -> f64| -> (f64, f64) {
        let mut v: Vec<f64> = samples.iter().map(f).collect();
        (percentile(&mut v, 0.5), percentile(&mut v, 0.99))
    };
    let rows: [(&str, Stage); 8] = [
        ("Luau NPC update (one call)", |s| s.script),
        ("Jolt step", |s| s.physics),
        ("physics + script → ECS sync", |s| s.sync),
        ("transform propagation", |s| s.transforms),
        ("draw list from ECS", |s| s.draw_list),
        ("render graph record + submit (CPU)", |s| s.record),
        ("GPU (all passes)", |s| s.gpu),
        ("whole frame (wall clock)", |s| s.frame),
    ];
    println!("### Stress scene: {title}\n");
    println!("{extra}\n");
    println!("| stage | median µs | p99 µs | % of 240 FPS budget (median) |");
    println!("|---|---:|---:|---:|");
    for (name, f) in rows {
        let (m, p) = column(f);
        if m == 0.0 && p == 0.0 {
            continue;
        }
        println!(
            "| {name} | {m:.0} | {p:.0} | {:.0}% |",
            m / BUDGET_US * 100.0
        );
    }
    let (frame_median, frame_p99) = column(|s| s.frame);
    if frame_median > 0.0 {
        println!(
            "\n**{:.0} FPS median, {:.0} FPS at p99** (target 240: {:.2} ms per frame).\n",
            1e6 / frame_median,
            1e6 / frame_p99,
            BUDGET_US / 1000.0
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let headless = args.iter().any(|a| a == "--headless");
    let nums: Vec<usize> = args.iter().filter_map(|a| a.parse().ok()).collect();
    let frames = nums.first().copied().unwrap_or(1500);
    let decor = nums.get(1).copied().unwrap_or(10_000);
    let bodies = nums.get(2).copied().unwrap_or(1_000) as u32;
    let npcs = nums.get(3).copied().unwrap_or(1_000);
    let mut sim = Sim::new(decor, bodies, npcs);
    let entities = sim.world.len();
    let extra = format!(
        "{entities} ECS entities ({decor} decor in 100 hierarchies, {bodies} Jolt bodies, {npcs} Luau NPCs), {} threads",
        std::thread::available_parallelism().map_or(1, |n| n.get())
    );
    if headless || !cfg!(windows) {
        let mut samples = Vec::with_capacity(frames);
        for f in 0..frames + 120 {
            let mut s = Stages::default();
            let start = Instant::now();
            sim.update(TICK, f as f32 * TICK, &mut s);
            s.frame = start.elapsed().as_secs_f64() * 1e6;
            if f >= 120 {
                samples.push(s);
            }
        }
        report(&samples, "simulation only (headless)", &extra);
    } else {
        #[cfg(windows)]
        windowed::run(sim, frames, &extra);
    }
}

#[cfg(windows)]
mod windowed {
    use super::*;
    use engine_core::math::{Mat4, look_at};
    use engine_ecs::QueryState;
    use engine_platform::window::{WindowDesc, WindowEventKind, WindowSystem};
    use engine_render::forward::{
        Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
    };
    use engine_render::graph::{ImportedTexture, RenderGraph, TextureDesc, TextureState};
    use engine_render::material::{MaterialDesc, MaterialId, MaterialTable};
    use engine_render::mesh::{GeometryCapacity, GeometryPool, cube, perspective};
    use engine_render::post::{HDR_FORMAT, Tonemap};
    use engine_render::texture_pool::TexturePool;
    use engine_render::upload::UploadRing;
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Bindless, Device, Frames, Instance, InstanceDesc,
        OutputColor, PresentMode, RhiError, Surface, Swapchain, SwapchainDesc,
    };
    use std::time::Duration;

    pub(crate) fn run(mut sim: Sim, frames_to_run: usize, extra: &str) {
        let mut windows = WindowSystem::start(4096).unwrap();
        let window = windows
            .create_window(&WindowDesc {
                title: String::from("engine stress"),
                width: 1920,
                height: 1080,
                resizable: false,
                visible: true,
            })
            .unwrap();
        let instance = Instance::new(&InstanceDesc::default()).unwrap();
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference).unwrap();
        let adapter_name = adapter.summary();
        let device = Device::new(&instance, &adapter).unwrap();
        let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap();
        let in_flight = 2;
        let mut swapchain = Swapchain::new(
            &device,
            &surface,
            &SwapchainDesc {
                width: 1920,
                height: 1080,
                present_mode: PresentMode::Immediate,
                srgb: true,
                color: OutputColor::Sdr,
                low_latency: false,
            },
        )
        .unwrap();
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let depth_format = vk::Format::D32_SFLOAT;
        let renderer = ForwardRenderer::new(
            &device,
            &bindless,
            HDR_FORMAT,
            depth_format,
            ShadowSettings::default(),
        )
        .unwrap();
        let tonemap = Tonemap::new(&device, &bindless, swapchain.format()).unwrap();
        let mut geometry = GeometryPool::new(
            &device,
            &mut allocator,
            GeometryCapacity::default(),
            in_flight,
        )
        .unwrap();
        let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
        let mut textures = TexturePool::new(&device, 64 << 20, in_flight);
        let mut materials = MaterialTable::new(in_flight);
        let paints: Vec<MaterialId> = sim
            .paints
            .iter()
            .map(|p| materials.add(MaterialDesc::solid(p.color, p.metallic, p.roughness)))
            .collect();
        let ground = materials.default_material();
        let mut ring = UploadRing::new(&device, &mut allocator, in_flight, 32 << 20).unwrap();
        let mut frames = Frames::new(&device, device.graphics.family, in_flight).unwrap();
        let mut graph = RenderGraph::new(&device, in_flight);
        let mut query = QueryState::<(&GlobalTransform, &Look)>::new(&mut sim.world);
        let mut draws: Vec<DrawItem> = Vec::with_capacity(sim.world.len() + 1);
        let mut samples: Vec<Stages> = Vec::with_capacity(frames_to_run);
        let start = Instant::now();
        let mut total = 0usize;
        let mut extent_seen = (0, 0);
        'main: loop {
            let mut close = false;
            windows.drain_events(|e| {
                if matches!(e.kind, WindowEventKind::CloseRequested) {
                    close = true;
                }
            });
            if close || start.elapsed() > Duration::from_secs(300) {
                break 'main;
            }
            let frame_start = Instant::now();
            // One 240 Hz tick per frame, in simulated time like the headless run: with wall
            // time, 1,500 frames at ~1,000 FPS cover 1.5 s, the pile is kicked at most once
            // and falls asleep, and the Jolt step measures a sleeping world.
            let dt = TICK;
            let mut s = Stages::default();
            let t = total as f32 * TICK;
            sim.update(dt, t, &mut s);

            let frame = frames.begin().unwrap();
            let acquired = match swapchain.acquire(frame.image_available, u64::MAX) {
                Ok(a) => a,
                Err(RhiError::Vulkan {
                    result: vk::Result::ERROR_OUT_OF_DATE_KHR,
                    ..
                }) => {
                    frames.wait_idle().unwrap();
                    let e = swapchain.extent();
                    swapchain
                        .recreate(&device, &surface, e.width, e.height)
                        .unwrap();
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            let index = acquired.index as usize;
            let extent = swapchain.extent();
            extent_seen = (extent.width, extent.height);

            let dl = Instant::now();
            draws.clear();
            draws.push(DrawItem {
                mesh,
                material: ground,
                world_from_object: Mat4::from_scale_rotation_translation(
                    Vec3::new(120.0, 0.2, 120.0),
                    Quat::IDENTITY,
                    Vec3::new(0.0, -0.1, 0.0),
                ),
            });
            query.for_each(&mut sim.world, |(g, look)| {
                draws.push(DrawItem {
                    mesh,
                    material: paints[look.paint as usize],
                    world_from_object: Mat4::from(g.0) * Mat4::from_scale(Vec3::splat(look.scale)),
                });
            });
            s.draw_list = dl.elapsed().as_secs_f64() * 1e6;

            let eye = Vec3::new(55.0 * (t * 0.1).cos(), 28.0, 55.0 * (t * 0.1).sin());
            let clip_from_world = perspective(
                55f32.to_radians(),
                extent.width as f32 / extent.height.max(1) as f32,
                0.1,
                400.0,
            ) * look_at(eye, Vec3::ZERO, Vec3::Y);
            let light = DirectionalLight {
                direction: Vec3::new(-0.4, -1.0, -0.3),
                color: Vec3::splat(3.0),
                ambient: Vec3::splat(0.08),
            };
            let rec = Instant::now();
            {
                let mut fg = graph.begin(&frame);
                let geo = geometry
                    .begin_frame(&mut fg, &device, &mut allocator, frame.number)
                    .unwrap();
                textures
                    .begin_frame(&mut fg, &mut allocator, &bindless, frame.number)
                    .unwrap();
                let mats = materials
                    .begin_frame(&mut fg, &device, &mut allocator, &textures, frame.number)
                    .unwrap();
                let desc = TextureDesc::new_2d(swapchain.format(), extent.width, extent.height);
                let backbuffer = fg.import_texture(
                    "backbuffer",
                    ImportedTexture {
                        image: swapchain.images()[index],
                        view: swapchain.views()[index],
                        desc,
                        initial: TextureState::acquired_swapchain_image(),
                        final_state: Some(TextureState::present()),
                        sampled_index: None,
                        storage_index: None,
                    },
                );
                let depth = fg.create_texture(
                    "depth",
                    TextureDesc::new_2d(depth_format, extent.width, extent.height),
                );
                let hdr = fg.create_texture(
                    "hdr",
                    TextureDesc::new_2d(HDR_FORMAT, extent.width, extent.height),
                );
                ring.begin(&frame);
                renderer.add_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    &draws,
                    &ForwardView {
                        camera: Camera {
                            clip_from_world,
                            position: eye,
                        },
                        light,
                        color: hdr,
                        depth,
                        clear: Some([0.02, 0.02, 0.05, 1.0]),
                        occlusion: None,
                        temporal: None,
                        lights: &[],
                    },
                );
                tonemap.add_pass(&mut fg, &bindless, hdr, backbuffer, 1.0);
                fg.execute_with_bindless(&mut allocator, Some(&bindless))
                    .unwrap();
            }
            frames
                .submit(
                    device.graphics,
                    frame,
                    true,
                    Some(swapchain.render_finished(acquired.index)),
                )
                .unwrap();
            let _ = swapchain.present(device.graphics, acquired.index);
            s.record = rec.elapsed().as_secs_f64() * 1e6;
            // GPU times of this graph's latest completed frame.
            s.gpu = graph
                .gpu_pass_times()
                .iter()
                .map(|p| f64::from(p.gpu_us))
                .sum();
            s.frame = frame_start.elapsed().as_secs_f64() * 1e6;
            engine_profiler::frame_mark();
            total += 1;
            if total > 120 {
                samples.push(s);
            }
            if samples.len() >= frames_to_run {
                break 'main;
            }
        }
        frames.wait_idle().unwrap();
        let passes: Vec<String> = graph
            .gpu_pass_times()
            .iter()
            .map(|p| format!("{} {:.0} µs", p.name, p.gpu_us))
            .collect();
        let validation = instance.validation_report();
        // The draw list borrows the mesh: release it before the mesh goes.
        let draw_count = draws.capacity();
        drop(draws);
        // SAFETY: the device is idle.
        unsafe {
            graph.destroy(&mut allocator, Some(&bindless));
            ring.destroy(&mut allocator);
            geometry.destroy(&mut allocator);
            materials.destroy(&mut allocator);
            textures.destroy(&mut allocator, &bindless);
        }
        drop(frames);
        drop(renderer);
        drop(tonemap);
        drop(bindless);
        drop(allocator);
        drop(swapchain);
        drop(surface);
        windows.destroy_window(window.id).unwrap();
        if samples.is_empty() {
            println!("no frames measured (window never became visible?)");
            std::process::exit(2);
        }
        let extra = format!(
            "{extra}; {} draws; {}×{} immediate; adapter: {adapter_name}; last GPU passes: {}; validation errors: {}",
            draw_count,
            extent_seen.0,
            extent_seen.1,
            passes.join(", "),
            validation.errors
        );
        report(&samples, "1080p, uncapped", &extra);
    }
}
