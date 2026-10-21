//! Extraction from the ECS into a scene (ADR 0064), against a recording stand-in for the
//! GPU scene: additions, edits, removals of every kind, cost that follows changes,
//! interpolation between ticks, rebuilds after a restore, and determinism.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Affine3A, Mat4, Quat, Vec3};
use engine_ecs::{
    Entity, GlobalTransform, Snapshot, Transform, TransformPropagation, World, bytemuck,
    pod_component,
};
use engine_render::gpu_scene::SceneInstance;
use engine_render::material::MaterialId;
use engine_render::mesh::MeshId;
use engine_runtime::{
    ActivityPropagation, Disabled, FixedStep, MeshRenderer, RenderExtract, Runtime, SceneSink,
};
use std::collections::HashMap;

/// Records what extraction does to it.
#[derive(Default)]
struct Recorder {
    next: u32,
    live: HashMap<u32, SceneInstance>,
    inserts: u32,
    removes: u32,
    moves: u32,
}

impl SceneSink for Recorder {
    type Id = u32;
    fn insert(&mut self, instance: SceneInstance) -> u32 {
        self.next += 1;
        self.inserts += 1;
        self.live.insert(self.next, instance);
        self.next
    }
    fn remove(&mut self, id: u32) -> bool {
        self.removes += 1;
        self.live.remove(&id).is_some()
    }
    fn set_transform(&mut self, id: u32, world_from_object: &Mat4) -> bool {
        let Some(i) = self.live.get_mut(&id) else {
            return false;
        };
        if i.world_from_object == *world_from_object {
            return false;
        }
        self.moves += 1;
        i.world_from_object = *world_from_object;
        true
    }
    fn set_mesh(&mut self, id: u32, mesh: MeshId) {
        if let Some(i) = self.live.get_mut(&id) {
            i.mesh = mesh;
        }
    }
    fn set_material(&mut self, id: u32, material: MaterialId) {
        if let Some(i) = self.live.get_mut(&id) {
            i.material = material;
        }
    }
}

/// Distinct mesh and material ids (plain data: any bits are an id).
fn mesh(n: u32) -> MeshId {
    bytemuck::cast([n, 0u32])
}
fn material(n: u32) -> MaterialId {
    bytemuck::cast([n, 0u32])
}

#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Spin(f32);
pod_component!(Spin);

fn renderable(w: &mut World, x: f32) -> Entity {
    w.spawn((
        Transform::from_xyz(x, 0.0, 0.0),
        GlobalTransform::IDENTITY,
        MeshRenderer::new(mesh(1), material(1)),
    ))
}

/// The scene equals the world: every visible renderer once, at its world transform.
fn assert_mirrors(world: &mut World, extract: &RenderExtract<u32>, scene: &Recorder) {
    let mut expected = 0;
    let mut q = engine_ecs::QueryState::<(Entity, &GlobalTransform, &MeshRenderer)>::new(world);
    q.for_each_readonly(world, |(e, g, r)| {
        if !r.is_visible() {
            assert!(extract.instance(e).is_none());
            return;
        }
        expected += 1;
        let id = extract.instance(e).unwrap();
        let i = scene.live[&id];
        assert_eq!(i.world_from_object, Mat4::from(g.0), "{e:?}");
        assert_eq!((i.mesh, i.material), (r.mesh, r.material));
    });
    assert_eq!(scene.live.len(), expected);
}

#[test]
fn the_scene_follows_additions_edits_and_removals() {
    let mut w = World::new();
    let mut p = TransformPropagation::new(&mut w);
    let mut x = RenderExtract::<u32>::new(&mut w, false);
    let mut s = Recorder::default();
    let a = renderable(&mut w, 1.0);
    let b = renderable(&mut w, 2.0);
    let c = renderable(&mut w, 3.0);
    let d = renderable(&mut w, 4.0);
    let e = renderable(&mut w, 5.0);
    w.set_parent(e, Some(d)).unwrap();
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    assert!(x.stats().rebuilt);
    assert_eq!(x.stats().inserted, 5);
    assert_mirrors(&mut w, &x, &s);

    // Nothing changed: nothing visited.
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    assert_eq!(x.stats(), Default::default());

    // Moves (the child follows its parent), a new material, a hidden one, a despawn, a
    // removed renderer, and a new entity reusing a despawned one's index.
    w.get_mut::<Transform>(d).unwrap().translation.y = 1.0;
    w.get_mut::<MeshRenderer>(a).unwrap().material = material(7);
    w.get_mut::<MeshRenderer>(b).unwrap().visible = 0;
    w.despawn(c);
    let f = renderable(&mut w, 6.0);
    assert_eq!(f.index(), c.index(), "the index is reused");
    w.remove::<MeshRenderer>(a);
    w.insert(a, MeshRenderer::new(mesh(2), material(3)));
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    println!("{:?}", x.stats());
    assert_mirrors(&mut w, &x, &s);
    assert_eq!(x.stats().moved, 2, "d and its child e");
    assert!(x.instance(c).is_none() && x.instance(f).is_some());

    // Shown again, its world transform removed, the subtree despawned.
    w.get_mut::<MeshRenderer>(b).unwrap().visible = 1;
    w.remove::<GlobalTransform>(f);
    w.despawn_recursive(d);
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    assert_mirrors(&mut w, &x, &s);
    assert_eq!(s.live.len(), 2, "a and b");
}

#[test]
fn a_static_world_costs_nothing_and_movers_cost_their_chunks() {
    let mut w = World::new();
    let mut p = TransformPropagation::new(&mut w);
    let mut x = RenderExtract::<u32>::new(&mut w, false);
    let mut s = Recorder::default();
    for i in 0..10_000 {
        renderable(&mut w, i as f32);
    }
    // Movers carry a component of their own: their own archetype, their own chunks.
    let movers: Vec<Entity> = (0..100)
        .map(|i| {
            w.spawn((
                Transform::from_xyz(0.0, i as f32, 0.0),
                GlobalTransform::IDENTITY,
                MeshRenderer::new(mesh(1), material(1)),
                Spin(1.0),
            ))
        })
        .collect();
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    assert_eq!(x.stats().inserted, 10_100);
    for frame in 0..3 {
        p.run(&mut w);
        x.sync(&mut w, &mut s);
        assert_eq!(x.stats().visited, 0, "frame {frame}");
    }
    for &m in &movers[..10] {
        w.get_mut::<Transform>(m).unwrap().translation.z += 1.0;
    }
    let moves = s.moves;
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    let stats = x.stats();
    println!("10 of 10,100 moved: {stats:?}");
    assert_eq!(stats.moved, 10);
    assert_eq!(s.moves - moves, 10);
    assert!(stats.visited <= 100, "only the movers' chunk: {stats:?}");
    assert_mirrors(&mut w, &x, &s);
}

#[test]
fn moving_instances_are_interpolated_between_ticks() {
    let mut w = World::new();
    let mut p = TransformPropagation::new(&mut w);
    let mut x = RenderExtract::<u32>::new(&mut w, true);
    let mut s = Recorder::default();
    let a = renderable(&mut w, 0.0);
    let still = renderable(&mut w, 10.0);
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    let at = |s: &Recorder, x: &RenderExtract<u32>, e: Entity| {
        s.live[&x.instance(e).unwrap()].world_from_object.w_axis.x
    };
    // Tick: 0 → 1. Frames at alpha 0.25 and 0.75 show 0.25 and 0.75.
    w.get_mut::<Transform>(a).unwrap().translation.x = 1.0;
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    assert_eq!(x.moving(), 1);
    for alpha in [0.25, 0.75] {
        x.interpolate(&mut s, alpha);
        assert!((at(&s, &x, a) - alpha).abs() < 1e-6);
        assert_eq!(at(&s, &x, still), 10.0);
    }
    // Next tick it stands still: shown at 1 whatever alpha.
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    assert_eq!(x.moving(), 0);
    assert_eq!(at(&s, &x, a), 1.0);
    x.interpolate(&mut s, 0.5);
    assert_eq!(at(&s, &x, a), 1.0);
    // Rotation blends too, and a despawned mover is dropped from interpolation.
    w.get_mut::<Transform>(a).unwrap().rotation = Quat::from_rotation_y(0.1);
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    w.despawn(a);
    p.run(&mut w);
    x.sync(&mut w, &mut s);
    x.interpolate(&mut s, 0.5);
    assert!(x.instance(a).is_none());
    assert_eq!(s.live.len(), 1);
}

#[test]
fn a_restore_rebuilds_the_scene_and_extraction_keeps_the_simulation_deterministic() {
    // Two identical simulations, one extracted every tick: same state hash.
    let build = || {
        let mut w = World::new();
        for i in 0..200 {
            let e = renderable(&mut w, i as f32);
            if i % 3 == 0 {
                w.insert(e, Spin(0.1 * i as f32));
            }
        }
        w
    };
    let step = |w: &mut World| {
        let mut spins = engine_ecs::QueryState::<(&mut Transform, &Spin)>::new(w);
        spins.for_each(w, |(t, s)| {
            t.rotation *= Quat::from_rotation_y(s.0);
            t.translation.y += 0.01;
        });
    };
    let mut a = build();
    let mut b = build();
    let mut pa = TransformPropagation::new(&mut a);
    let mut pb = TransformPropagation::new(&mut b);
    let mut x = RenderExtract::<u32>::new(&mut a, true);
    let mut s = Recorder::default();
    let mut snapshot = Snapshot::new();
    for tick in 0..30 {
        for (w, p) in [(&mut a, &mut pa), (&mut b, &mut pb)] {
            step(w);
            w.advance_sim_tick();
            p.run(w);
        }
        x.sync(&mut a, &mut s);
        x.interpolate(&mut s, 0.5);
        assert_eq!(a.state_hash(), b.state_hash(), "tick {tick}");
        if tick == 10 {
            a.save(&mut snapshot).unwrap();
        }
    }
    // Rolled back (entities spawned since vanish without logs): rebuilt from scratch.
    for i in 0..5 {
        renderable(&mut a, 100.0 + i as f32);
    }
    pa.run(&mut a);
    x.sync(&mut a, &mut s);
    a.restore(&snapshot).unwrap();
    pa.run(&mut a);
    x.sync(&mut a, &mut s);
    assert!(x.stats().rebuilt);
    assert_mirrors(&mut a, &x, &s);
}

#[test]
fn the_runtime_runs_due_ticks_then_presents_then_interpolates() {
    let mut w = World::new();
    let mover = renderable(&mut w, 0.0);
    w.insert(mover, Spin(1.0));
    let mut runtime = Runtime::<u32>::new(w, 64.0);
    let mut s = Recorder::default();
    let order = std::cell::RefCell::new(Vec::new());
    let mut camera = 0.0f32;
    let mut ticks = 0;
    for frame in 0..240 {
        let input = frame as f32;
        let stages = runtime.frame(
            1.0 / 240.0,
            &input,
            &mut s,
            |w, input, dt| {
                order.borrow_mut().push(('t', *input));
                let mut q = engine_ecs::QueryState::<(&mut Transform, &Spin)>::new(w);
                q.for_each(w, |(t, spin)| t.translation.x += spin.0 * dt);
            },
            |_, input, _| {
                order.borrow_mut().push(('p', *input));
                camera = *input;
            },
        );
        ticks += stages.ticks;
        // The camera saw this frame's input, whether or not a tick ran.
        assert_eq!(camera, input);
        assert_eq!(order.borrow().last(), Some(&('p', input)));
    }
    assert!((63..=64).contains(&ticks), "{ticks}");
    // Every tick ran before the presentation of its frame, with the same input.
    for pair in order.borrow().windows(2) {
        if pair[1].0 == 'p' && pair[0].0 == 't' {
            assert_eq!(pair[0].1, pair[1].1);
        }
    }
    // The mover is drawn between its last two ticks.
    let id = runtime.extract().instance(mover).unwrap();
    let shown = s.live[&id].world_from_object.w_axis.x;
    let simulated = runtime
        .world
        .get::<GlobalTransform>(mover)
        .unwrap()
        .0
        .translation
        .x;
    assert!(
        shown <= simulated && shown >= simulated - 1.0 / 64.0,
        "{shown} {simulated}"
    );
}

/// Input-to-camera latency of the two designs (a model on a virtual clock, real
/// arithmetic): the camera updated every render frame from the latest input, against a
/// camera that is simulation state, updated per tick and interpolated between ticks.
/// Input events arrive at uniformly spread times; a frame samples input at its start.
#[test]
fn per_frame_camera_latency_against_a_tick_camera() {
    for (fps, hz) in [(240.0, 64.0), (144.0, 64.0), (60.0, 64.0), (240.0, 30.0)] {
        let frame_dt = 1.0f64 / fps;
        let mut clock = FixedStep::new(hz);
        // The tick camera: last two ticks' yaw; the input is the target yaw.
        let (mut prev, mut cur) = (0.0f64, 0.0f64);
        let mut events: Vec<f64> = Vec::new();
        let (mut per_frame, mut tick_camera) = (Vec::new(), Vec::new());
        let mut pending_tick: Vec<(f64, f64)> = Vec::new(); // (event time, target yaw)
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut t = 0.0;
        let mut target = 0.0;
        for _ in 0..20_000 {
            // An event somewhere in the previous frame's interval sets a new target.
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let event = t - frame_dt * ((seed >> 11) as f64 / (1u64 << 53) as f64);
            target += 1.0;
            events.push(event);
            // Per-frame camera: this frame shows it.
            per_frame.push(t - event);
            pending_tick.push((event, target));
            // Tick camera: ticks consume the latest target, the frame interpolates.
            let count = clock.advance(frame_dt as f32);
            for _ in 0..count {
                prev = cur;
                cur = target;
            }
            let shown = prev + (cur - prev) * f64::from(clock.alpha());
            // Events whose target the shown yaw has fully reached.
            pending_tick.retain(|&(event, target)| {
                if shown >= target - 1e-9 {
                    tick_camera.push(t - event);
                    false
                } else {
                    true
                }
            });
            t += frame_dt;
        }
        let median = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2] * 1e3
        };
        let worst = |v: &[f64]| v.iter().copied().fold(0.0, f64::max) * 1e3;
        let (m1, w1) = (median(&mut per_frame), worst(&per_frame));
        let (m2, w2) = (median(&mut tick_camera), worst(&tick_camera));
        println!(
            "{fps} FPS, {hz} Hz ticks: per-frame camera median {m1:.1} ms (worst {w1:.1}); \
             tick camera median {m2:.1} ms (worst {w2:.1})"
        );
        assert!(w1 <= frame_dt * 1e3 + 1e-6);
        assert!(m2 > m1);
    }
}

#[test]
fn camera_moves_with_input_without_ticks() {
    // A frame without a due tick still updates the camera.
    let mut runtime = Runtime::<u32>::new(World::new(), 10.0);
    let mut s = Recorder::default();
    let mut camera = Vec3::ZERO;
    let stages = runtime.frame(
        0.001,
        &Vec3::X,
        &mut s,
        |_, _, _| {},
        |_, input, _| camera += *input,
    );
    assert_eq!(stages.ticks, 0);
    assert_eq!(camera, Vec3::X);
}

/// A sink that places moving instances itself, as a `GpuScene` does on the GPU: it gets
/// each moving instance's two ticks once per tick and only the factor every frame.
#[derive(Default)]
struct Interpolating {
    inner: Recorder,
    motion: HashMap<u32, (Mat4, Mat4)>,
    alpha: f32,
    motions: u32,
    alphas: u32,
}

impl SceneSink for Interpolating {
    type Id = u32;
    fn insert(&mut self, instance: SceneInstance) -> u32 {
        self.inner.insert(instance)
    }
    fn remove(&mut self, id: u32) -> bool {
        self.motion.remove(&id);
        self.inner.remove(id)
    }
    fn set_transform(&mut self, id: u32, world_from_object: &Mat4) -> bool {
        // A move stops the motion, as `GpuScene::set_affine` does.
        let was_moving = self.motion.remove(&id).is_some();
        self.inner.set_transform(id, world_from_object) || was_moving
    }
    fn set_mesh(&mut self, id: u32, mesh: MeshId) {
        self.inner.set_mesh(id, mesh);
    }
    fn set_material(&mut self, id: u32, material: MaterialId) {
        self.inner.set_material(id, material);
    }
    fn interpolates(&self) -> bool {
        true
    }
    fn set_motion(&mut self, id: u32, from: &Affine3A, to: &Affine3A) -> bool {
        self.motions += 1;
        self.motion.insert(id, (Mat4::from(*from), Mat4::from(*to)));
        true
    }
    fn step_motion(&mut self, id: u32, to: &Affine3A) -> bool {
        // From what it was last given, as `GpuScene::step_motion`.
        let from = match self.motion.get(&id) {
            Some((_, last)) => *last,
            None => self.inner.live[&id].world_from_object,
        };
        self.motions += 1;
        self.motion.insert(id, (from, Mat4::from(*to)));
        true
    }
    fn set_alpha(&mut self, alpha: f32) {
        self.alphas += 1;
        self.alpha = alpha;
    }
}

impl Interpolating {
    /// Where it shows instance `id` (the GPU's blend).
    fn shown(&self, id: u32) -> Mat4 {
        match self.motion.get(&id) {
            Some((from, to)) => {
                let (a, b) = (from.to_cols_array(), to.to_cols_array());
                Mat4::from_cols_array(&core::array::from_fn(|i| a[i] + (b[i] - a[i]) * self.alpha))
            }
            None => self.inner.live[&id].world_from_object,
        }
    }
}

#[test]
fn a_sink_that_interpolates_gets_motion_once_per_tick_and_shows_what_the_cpu_blend_shows() {
    // Two identical worlds: one extracted into the CPU blend, one into the interpolating
    // sink.
    let build = || {
        let mut w = World::new();
        let movers = [renderable(&mut w, 0.0), renderable(&mut w, 5.0)];
        renderable(&mut w, 10.0);
        let p = TransformPropagation::new(&mut w);
        (w, p, movers)
    };
    let (mut wc, mut pc, mc) = build();
    let (mut wg, mut pg, mg) = build();
    let mut xc = RenderExtract::<u32>::new(&mut wc, true);
    let mut xg = RenderExtract::<u32>::new(&mut wg, true);
    let mut cpu = Recorder::default();
    let mut gpu = Interpolating::default();
    // The first sync adds the instances.
    pc.run(&mut wc);
    pg.run(&mut wg);
    xc.sync(&mut wc, &mut cpu);
    xg.sync(&mut wg, &mut gpu);
    for tick in 0..6 {
        for (w, p, m) in [(&mut wc, &mut pc, &mc), (&mut wg, &mut pg, &mg)] {
            // The first moves every tick (and turns), the second only in tick 1.
            let t = &mut *w.get_mut::<Transform>(m[0]).unwrap();
            t.translation.x += 1.0;
            t.rotation *= Quat::from_rotation_y(0.2);
            if tick == 1 {
                w.get_mut::<Transform>(m[1]).unwrap().translation.y += 2.0;
            }
            p.run(w);
        }
        xc.sync(&mut wc, &mut cpu);
        let motions = gpu.motions;
        xg.sync(&mut wg, &mut gpu);
        let moved = if tick == 1 { 2 } else { 1 };
        assert_eq!(
            gpu.motions - motions,
            moved,
            "tick {tick}: one motion per mover"
        );
        // Frames at a few factors: the same placements, nothing per mover.
        for alpha in [0.0, 0.3, 0.9] {
            xc.interpolate(&mut cpu, alpha);
            let (moves, alphas) = (gpu.inner.moves, gpu.alphas);
            xg.interpolate(&mut gpu, alpha);
            assert_eq!(gpu.inner.moves, moves, "no move per frame");
            assert_eq!(gpu.alphas, alphas + 1);
            for (ec, eg) in mc.iter().zip(&mg).chain([(&mc[0], &mg[0])]) {
                let c = cpu.live[&xc.instance(*ec).unwrap()].world_from_object;
                let g = gpu.shown(xg.instance(*eg).unwrap());
                assert!(
                    c.abs_diff_eq(g, 1e-5),
                    "tick {tick}, alpha {alpha}: {c:?} vs {g:?}"
                );
            }
        }
    }
}

/// Every call extraction makes, in order.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Call {
    Insert(u32, SceneInstance),
    Remove(u32),
    Transform(u32, Mat4),
    Affine(u32, Affine3A),
    Mesh(u32, MeshId),
    Material(u32, MaterialId),
    Step(u32, Affine3A),
    Alpha(f32),
}

/// Logs the calls; interpolates between ticks itself (as a `GpuScene`) or not.
struct Logger {
    interpolates: bool,
    next: u32,
    calls: Vec<Call>,
}

impl SceneSink for Logger {
    type Id = u32;
    fn insert(&mut self, instance: SceneInstance) -> u32 {
        self.next += 1;
        self.calls.push(Call::Insert(self.next, instance));
        self.next
    }
    fn remove(&mut self, id: u32) -> bool {
        self.calls.push(Call::Remove(id));
        true
    }
    fn set_transform(&mut self, id: u32, world_from_object: &Mat4) -> bool {
        self.calls.push(Call::Transform(id, *world_from_object));
        true
    }
    fn set_affine(&mut self, id: u32, world_from_object: &Affine3A) -> bool {
        self.calls.push(Call::Affine(id, *world_from_object));
        true
    }
    fn set_mesh(&mut self, id: u32, mesh: MeshId) {
        self.calls.push(Call::Mesh(id, mesh));
    }
    fn set_material(&mut self, id: u32, material: MaterialId) {
        self.calls.push(Call::Material(id, material));
    }
    fn interpolates(&self) -> bool {
        self.interpolates
    }
    fn step_motion(&mut self, id: u32, to: &Affine3A) -> bool {
        self.calls.push(Call::Step(id, *to));
        true
    }
    fn set_alpha(&mut self, alpha: f32) {
        self.calls.push(Call::Alpha(alpha));
    }
}

/// Extraction with its comparisons on a thread pool (`RenderExtract::sync_on`) tells the
/// scene the same things in the same order as on one thread: with GPU and CPU
/// interpolation and without, through moves, mesh and material edits, hiding, despawns
/// and spawns, over enough entities for the pool's path.
#[test]
fn extraction_on_a_pool_makes_the_same_calls_as_on_one_thread() {
    let pool = engine_core::jobs::ThreadPool::builder()
        .workers(4)
        .build()
        .unwrap();
    for (interpolation, gpu) in [(true, true), (true, false), (false, false)] {
        let mut worlds = [World::new(), World::new()];
        let mut entities = Vec::new();
        for w in &mut worlds {
            entities = (0..12_000).map(|i| renderable(w, i as f32)).collect();
        }
        let [a, b] = &mut worlds;
        let mut propagation = [TransformPropagation::new(a), TransformPropagation::new(b)];
        let mut extracts = [
            RenderExtract::<u32>::new(a, interpolation),
            RenderExtract::<u32>::new(b, interpolation),
        ];
        let mut sinks = [false, true].map(|_| Logger {
            interpolates: gpu,
            next: 0,
            calls: Vec::new(),
        });
        let mut visited = 0;
        for tick in 0..6u32 {
            for w in &mut worlds {
                for (k, &e) in entities.iter().enumerate() {
                    match (k % 97, tick) {
                        (1, 2) => w.get_mut::<MeshRenderer>(e).unwrap().mesh = mesh(2),
                        (2, 3) => w.get_mut::<MeshRenderer>(e).unwrap().material = material(2),
                        (3, 2) => w.get_mut::<MeshRenderer>(e).unwrap().visible = 0,
                        (3, 4) => w.get_mut::<MeshRenderer>(e).unwrap().visible = 1,
                        (4, 3) => {
                            w.despawn(e);
                        }
                        _ if k % 3 != 0 => {
                            if let Some(t) = w.get_mut::<Transform>(e) {
                                t.translation.y = (tick as usize * k) as f32 * 0.01;
                            }
                        }
                        _ => {}
                    }
                }
                if tick == 4 {
                    for i in 0..300 {
                        renderable(w, -(i as f32));
                    }
                }
            }
            let [a, b] = &mut worlds;
            let [sa, sb] = &mut sinks;
            propagation[0].run(a);
            propagation[1].run_parallel(b, &pool);
            extracts[0].sync(a, sa);
            extracts[1].sync_on(b, sb, Some(&pool));
            if interpolation {
                extracts[0].interpolate(sa, 0.5);
                extracts[1].interpolate(sb, 0.5);
            }
            let (x, y) = (extracts[0].stats(), extracts[1].stats());
            assert_eq!(x, y, "tick {tick}");
            visited = visited.max(x.visited);
            let first = sa.calls.iter().zip(&sb.calls).position(|(p, q)| p != q);
            assert!(
                first.is_none() && sa.calls.len() == sb.calls.len(),
                "tick {tick}, interpolation {interpolation}, gpu {gpu}: calls differ at {first:?}                  ({} and {} calls)",
                sa.calls.len(),
                sb.calls.len()
            );
            sa.calls.clear();
            sb.calls.clear();
        }
        assert!(visited >= 8000, "the pool's path: {visited} visited");
    }
}

#[test]
fn a_driven_frame_runs_the_drivers_ticks_and_shows_the_newest_state() {
    let mut w = World::new();
    let mover = renderable(&mut w, 0.0);
    w.insert(mover, Spin(1.0));
    let mut runtime = Runtime::<u32>::new(w, 64.0);
    let mut s = Recorder::default();
    // A frame with a tick before: the mover is in the scene.
    runtime.frame(1.5 / 64.0, &(), &mut s, |_, _, _| {}, |_, _, _| {});
    let id = runtime.extract().instance(mover).unwrap();
    let moves = s.moves;
    let mut presented = None;
    // The driver re-simulates two past ticks and predicts one: three steps, one frame.
    let stages = runtime.frame_driven(
        &(),
        &mut s,
        |w, (), dt| {
            let mut q = engine_ecs::QueryState::<(&mut Transform, &Spin)>::new(w);
            q.for_each(w, |(t, spin)| t.translation.x += spin.0 * dt);
        },
        |w, step| {
            for _ in 0..3 {
                step(w, 0.25);
                // Each step sees the transforms the one before it propagated.
                let x = w.get::<GlobalTransform>(mover).unwrap().translation().x;
                assert!(x > 0.0);
            }
            engine_runtime::Driven {
                alpha: 0.4,
                tick: 77,
            }
        },
        |_, (), alpha| presented = Some(alpha),
    );
    assert_eq!(stages.ticks, 3);
    assert_eq!(stages.alpha, 0.4);
    assert_eq!(presented, Some(0.4));
    assert_eq!(runtime.world.sim_tick(), 77);
    // Extracted once, at the newest state, shown as it is.
    assert_eq!(s.moves, moves + 1);
    assert_eq!(s.live[&id].world_from_object.w_axis.x, 0.75);
}

#[test]
fn disabled_entities_and_what_is_below_them_are_not_drawn() {
    let mut w = World::new();
    let mut p = TransformPropagation::new(&mut w);
    let mut a = ActivityPropagation::new(&mut w);
    let mut x = RenderExtract::<u32>::new(&mut w, false);
    let mut s = Recorder::default();
    let parent = renderable(&mut w, 1.0);
    let child = renderable(&mut w, 2.0);
    let other = renderable(&mut w, 3.0);
    w.set_parent(child, Some(parent)).unwrap();
    step(&mut w, &mut a, &mut p, &mut x, &mut s);
    assert_eq!(s.live.len(), 3);
    // Disabled: it and its child go; the other stays.
    w.insert(parent, Disabled {});
    step(&mut w, &mut a, &mut p, &mut x, &mut s);
    assert_eq!(s.live.len(), 1);
    assert!(x.instance(parent).is_none() && x.instance(child).is_none());
    assert!(x.instance(other).is_some());
    // Edited while off: still not drawn.
    w.get_mut::<MeshRenderer>(child).unwrap().material = material(9);
    step(&mut w, &mut a, &mut p, &mut x, &mut s);
    assert!(x.instance(child).is_none());
    // Enabled: back, as they are now.
    w.remove::<Disabled>(parent);
    step(&mut w, &mut a, &mut p, &mut x, &mut s);
    assert_eq!(s.live.len(), 3);
    assert_eq!(s.live[&x.instance(child).unwrap()].material, material(9));
    assert_mirrors(&mut w, &x, &s);
}

/// One tick's activity, propagation and extraction.
fn step(
    w: &mut World,
    a: &mut ActivityPropagation,
    p: &mut TransformPropagation,
    x: &mut RenderExtract<u32>,
    s: &mut Recorder,
) {
    a.run(w);
    p.run(w);
    x.sync(w, s);
}
