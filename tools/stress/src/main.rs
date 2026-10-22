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
//! - **Rendering:** a game on the player (`engine_app`, ADR 0099): every entity a PBR
//!   cube mirrored into the persistent GPU scene by the runtime, the frame the device
//!   class's (antialiasing, ambient occlusion, meshlet culling, cached shadows, bloom),
//!   borderless over the monitor, presented with `immediate` (uncapped, latency control
//!   off). The simulation is the game's tick, one 240 Hz tick a frame.
//!
//! ```text
//! cargo run --release -p engine_stress -- [frames] [decor] [bodies] [npcs] [--headless]
//! ```
//!
//! `--headless` runs the simulation parts only (any OS). The report is Markdown on stdout.
//! Before the player (2026-10-15) the tool drew a forward pass and a tone map from a draw
//! list or (`--gpu-scene`) the GPU scene itself: its history is that frame's.

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

/// The simulation, on a world that is not its own: its entities are spawned into it.
struct Sim {
    physics: JoltBackend,
    bodies: Vec<BodyId>,
    body_entities: Vec<Entity>,
    npc_entities: Vec<Entity>,
    roots: Vec<Entity>,
    vm: ScriptVm,
    npc_out: mlua::Buffer,
    npc_bytes: Vec<u8>,
    body_poses: Vec<(Vec3, Quat)>,
    /// The distinct surfaces of the cubes.
    paints: Vec<Paint>,
    frame: u64,
}

impl Sim {
    fn new(world: &mut World, decor: usize, bodies: u32, npcs: usize) -> Self {
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
                world,
                Transform::from_xyz(angle.cos() * 40.0, 1.0, angle.sin() * 40.0),
                look([0.8, 0.8, 0.85], 1.0, 0.3, 0.6),
            );
            roots.push(root);
            for c in 1..per_root {
                let a = c as f32 * 0.5;
                let child = spawn(
                    world,
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
                    world,
                    Transform::IDENTITY,
                    look([0.9, 0.5 + (i % 3) as f32 * 0.15, 0.2], 0.0, 0.6, 0.8),
                )
            })
            .collect();
        let npc_entities = (0..npcs)
            .map(|_| {
                spawn(
                    world,
                    Transform::IDENTITY,
                    look([0.2, 0.9, 0.3], 0.0, 0.4, 0.5),
                )
            })
            .collect();
        let mut vm = ScriptVm::new(ScriptSettings::default()).unwrap();
        vm.load_module("npc", NPC_SCRIPT).unwrap();
        vm.call::<_, ()>("npc", "init", npcs).unwrap();
        let npc_out = vm.lua().create_buffer(vec![0u8; npcs * 12]).unwrap();
        Self {
            physics,
            body_poses: vec![(Vec3::ZERO, Quat::IDENTITY); bodies_ids.len()],
            bodies: bodies_ids,
            body_entities,
            npc_entities,
            roots,
            vm,
            npc_out,
            npc_bytes: vec![0; npcs * 12],
            paints,
            frame: 0,
        }
    }

    /// Runs the simulation stages of one tick on `world` and records their times
    /// (transform propagation is the caller's: the runtime's after a game's tick).
    fn update(&mut self, world: &mut World, dt: f32, t: f32, stages: &mut Stages) {
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
            if let Some(tr) = world.get_mut::<Transform>(e) {
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
            if let Some(tr) = world.get_mut::<Transform>(e) {
                tr.translation = p;
            }
        }
        for (k, &root) in self.roots.iter().enumerate() {
            if let Some(tr) = world.get_mut::<Transform>(root) {
                tr.rotation = Quat::from_rotation_y(t * (0.5 + (k % 7) as f32 * 0.1));
            }
        }
        stages.sync = us(start);
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
        ("extraction into the GPU scene", |s| s.draw_list),
        (
            "pools, scene upload, frame graph, submit, present (CPU)",
            |s| s.record,
        ),
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
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let extra = |entities: usize| {
        format!(
            "{entities} ECS entities ({decor} decor in 100 hierarchies, {bodies} Jolt bodies, {npcs} Luau NPCs), {threads} threads"
        )
    };
    if headless || !cfg!(windows) {
        let mut world = World::new();
        let mut sim = Sim::new(&mut world, decor, bodies, npcs);
        let mut propagation = TransformPropagation::new(&mut world);
        let mut samples = Vec::with_capacity(frames);
        for f in 0..frames + 120 {
            let mut s = Stages::default();
            let start = Instant::now();
            sim.update(&mut world, TICK, f as f32 * TICK, &mut s);
            let transforms = Instant::now();
            propagation.run(&mut world);
            s.transforms = transforms.elapsed().as_secs_f64() * 1e6;
            s.frame = start.elapsed().as_secs_f64() * 1e6;
            if f >= 120 {
                samples.push(s);
            }
        }
        report(&samples, "simulation only (headless)", &extra(world.len()));
    } else {
        #[cfg(windows)]
        windowed::run(frames, decor, bodies, npcs, &extra);
    }
}

#[cfg(windows)]
mod windowed {
    use super::*;
    use engine_app::{Game, Player, PlayerSettings};
    use engine_ecs::{Children, QueryState};
    use engine_latency::LowLatency;
    use engine_render::material::{MaterialDesc, MaterialId};
    use engine_render::mesh::cube;
    use engine_runtime::{MeshRenderer, Sun, look_rotation};

    /// The game: the simulation as its tick, the camera orbiting every frame.
    struct Stress {
        sim: Sim,
        /// The last tick's stages.
        stages: Stages,
        camera: Entity,
        time: f32,
    }

    impl Game for Stress {
        fn tick(&mut self, world: &mut World, dt: f32) {
            self.sim.update(world, dt, self.time, &mut self.stages);
            self.time += dt;
        }

        fn present(&mut self, world: &mut World, _alpha: f32) {
            let t = self.time;
            let eye = Vec3::new(55.0 * (t * 0.1).cos(), 28.0, 55.0 * (t * 0.1).sin());
            if let Some(transform) = world.get_mut::<Transform>(self.camera) {
                transform.translation = eye;
                transform.rotation = look_rotation(eye, Vec3::ZERO, Vec3::Y);
            }
        }
    }

    pub(crate) fn run(
        frames_to_run: usize,
        decor: usize,
        bodies: u32,
        npcs: usize,
        extra: &dyn Fn(usize) -> String,
    ) {
        let mut player = Player::new(&PlayerSettings {
            title: String::from("engine stress"),
            // Uncapped: the frame's own cost, not a paced one.
            low_latency: LowLatency::Off,
            frame_cap: engine_app::FrameCap::Uncapped,
            tick_hz: 240.0,
            ..PlayerSettings::default()
        })
        .unwrap_or_else(|e| panic!("{e}"));
        // One 240 Hz tick a frame in simulated time, as the headless run: with wall time,
        // 1,500 frames at ~1,000 FPS cover 1.5 s, the pile is kicked at most once and falls
        // asleep, and the Jolt step measures a sleeping world.
        player.set_frame_time(Some(f64::from(TICK)));
        let mut drawn = 0;
        let sim = {
            let setup = player.setup();
            let sim = Sim::new(setup.world, decor, bodies, npcs);
            let mesh = setup.geometry.add_mesh(&cube().as_data()).unwrap();
            let paints: Vec<MaterialId> = sim
                .paints
                .iter()
                .map(|p| {
                    setup
                        .materials
                        .add(MaterialDesc::solid(p.color, p.metallic, p.roughness))
                })
                .collect();
            let ground = setup.materials.default_material();
            setup.world.spawn((
                Transform::from_xyz(0.0, -0.1, 0.0).with_scale(Vec3::new(120.0, 0.2, 120.0)),
                GlobalTransform::IDENTITY,
                MeshRenderer::new(mesh, ground),
            ));
            // A cube per entity, its scale in its local transform; a root draws its cube
            // through a child of its own, so its children keep their placement.
            let mut looks = Vec::new();
            QueryState::<(Entity, &Look, Option<&Children>)>::new(setup.world)
                .for_each(setup.world, |(e, look, children)| {
                    looks.push((e, *look, children.is_some()))
                });
            for (e, look, root) in looks {
                let renderer = MeshRenderer::new(mesh, paints[look.paint as usize]);
                let scale = Vec3::splat(look.scale);
                if root {
                    let cube = setup.world.spawn((
                        Transform::from_scale(scale),
                        GlobalTransform::IDENTITY,
                        renderer,
                    ));
                    setup.world.set_parent(cube, Some(e)).unwrap();
                } else {
                    if let Some(t) = setup.world.get_mut::<Transform>(e) {
                        t.scale = scale;
                    }
                    setup.world.insert(e, renderer);
                }
                drawn += 1;
            }
            setup.world.spawn((
                GlobalTransform(engine_core::math::Affine3A::from_rotation_translation(
                    Quat::from_rotation_arc(Vec3::NEG_Z, Vec3::new(-0.4, -1.0, -0.3).normalize()),
                    Vec3::ZERO,
                )),
                Sun {
                    color: Vec3::splat(3.0),
                    ambient: Vec3::splat(0.08),
                },
            ));
            sim
        };
        let camera = player.camera(55f32.to_radians(), 0.1, 400.0);
        let camera = player.world_mut().spawn((Transform::IDENTITY, camera));
        let entities = player.world_mut().len();
        let mut game = Stress {
            sim,
            stages: Stages::default(),
            camera,
            time: 0.0,
        };
        let mut samples: Vec<Stages> = Vec::with_capacity(frames_to_run);
        let mut total = 0usize;
        while samples.len() < frames_to_run && !player.closing() {
            let frame_start = Instant::now();
            let Some(report) = player.frame(&mut game).unwrap_or_else(|e| panic!("{e}")) else {
                continue;
            };
            let mut s = game.stages;
            s.transforms = report.propagate;
            s.draw_list = report.extract;
            s.record = report.pools
                + report.upload
                + report.compose
                + report.execute
                + report.submit
                + report.present;
            // GPU times of the graph's latest completed frame.
            s.gpu = player
                .graph()
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
        }
        let passes: Vec<String> = player
            .graph()
            .gpu_pass_times()
            .iter()
            .map(|p| format!("{} {:.0} µs", p.name, p.gpu_us))
            .collect();
        let extent = player.extent();
        let presentation = player.presentation();
        let extra = format!(
            "{}; {drawn} cubes on the player ({}, {} at {:?}); {}×{} {:?}; adapter: {}; last GPU passes: {}; validation errors: {}",
            extra(entities),
            player.antialiasing(),
            match player.defaults().ambient_occlusion {
                Some(ao) => format!("AO {:?}", ao.quality),
                None => String::from("no AO"),
            },
            player.quality(),
            extent.width,
            extent.height,
            presentation.present_mode,
            player.device().adapter().summary(),
            passes.join(", "),
            player.validation_report().errors
        );
        drop(player);
        if samples.is_empty() {
            println!("no frames measured (window never became visible?)");
            std::process::exit(2);
        }
        report(&samples, "1080p, uncapped, on the player", &extra);
    }
}
