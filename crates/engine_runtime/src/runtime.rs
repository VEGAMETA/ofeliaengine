//! One frame of the game: input, ticks, presentation, interpolation.

use crate::activity::ActivityPropagation;
use crate::extract::{RenderExtract, SceneSink};
use crate::lights::LightExtract;
use crate::skinned::SkinnedQuery;
use engine_core::jobs::ThreadPool;
use engine_ecs::{TransformPropagation, World};
use engine_physics::ecs::FixedStep;
use engine_render::gpu_scene::InstanceId;
use std::sync::Arc;
use std::time::Instant;

/// Where one [`Runtime::frame`] spent its time, in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameStages {
    /// Ticks run.
    pub ticks: u32,
    /// The interpolation factor rendered.
    pub alpha: f32,
    /// The game's tick systems.
    pub simulate_us: f64,
    /// Transform propagation, over the ticks.
    pub propagate_us: f64,
    /// Extraction into the scene, over the ticks.
    pub extract_us: f64,
    /// The per-frame presentation step (camera from the latest input).
    pub present_us: f64,
    /// Interpolation of the moving instances.
    pub interpolate_us: f64,
}

/// What a driven frame's driver reports ([`Runtime::frame_driven`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Driven {
    /// How far its clock is into its next tick, 0..1 (the game's, to present what it
    /// predicts between ticks).
    pub alpha: f32,
    /// Its newest tick: the world's simulation tick after the frame.
    pub tick: u64,
}

/// The game's world and frame loop (see the crate documentation): one world, one
/// fixed-step simulation, extraction into the renderer's scene.
///
/// ```
/// use engine_ecs::{GlobalTransform, Transform, World};
/// use engine_runtime::{MeshRenderer, Runtime, SceneSink};
/// # use engine_core::math::Mat4;
/// # use engine_render::gpu_scene::SceneInstance;
/// # use engine_render::{material::MaterialId, mesh::MeshId};
/// # #[derive(Default)] struct Scene(u32);
/// # impl SceneSink for Scene {
/// #     type Id = u32;
/// #     fn insert(&mut self, _: SceneInstance) -> u32 { self.0 += 1; self.0 }
/// #     fn remove(&mut self, _: u32) -> bool { true }
/// #     fn set_transform(&mut self, _: u32, _: &Mat4) -> bool { true }
/// #     fn set_mesh(&mut self, _: u32, _: MeshId) {}
/// #     fn set_material(&mut self, _: u32, _: MaterialId) {}
/// # }
/// # let (mesh, material) = (bytemuck::Zeroable::zeroed(), bytemuck::Zeroable::zeroed());
/// let mut world = World::new();
/// world.spawn((Transform::IDENTITY, GlobalTransform::IDENTITY, MeshRenderer::new(mesh, material)));
/// let mut runtime = Runtime::<u32>::new(world, 64.0);
/// let mut scene = Scene::default(); // a `GpuScene` in a game
/// let mut yaw = 0.0f32;
/// // Each frame: sample input, then run the frame, then render with `yaw`.
/// let input = 0.01f32; // mouse delta, say
/// runtime.frame(
///     1.0 / 120.0,
///     &input,
///     &mut scene,
///     |world, input, dt| { /* the tick: movement, physics, ... consume `input` */ },
///     |world, input, alpha| yaw += *input, // the camera follows input every frame
/// );
/// ```
pub struct Runtime<Id = InstanceId> {
    /// The simulation's world.
    pub world: World,
    /// The simulation clock.
    pub clock: FixedStep,
    propagation: TransformPropagation,
    /// What is off ([`crate::Disabled`]), over the ticks, before the propagation.
    activity: ActivityPropagation,
    pub(crate) extract: RenderExtract<Id>,
    /// The light components, mirrored after every tick (ADR 0099 step 3).
    lights: LightExtract,
    /// The skinned entities ([`Self::place_skinned`]).
    pub(crate) skinned: SkinnedQuery,
    /// Workers for each tick's propagation ([`Self::set_jobs`]).
    jobs: Option<Arc<ThreadPool>>,
}

impl<Id> core::fmt::Debug for Runtime<Id> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Runtime")
            .field("world", &self.world)
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

/// Microseconds since `start`.
fn us(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e6
}

impl<Id: Copy + Eq + Send + Sync> Runtime<Id> {
    /// Runs `world` at `hz` ticks per second (at most 4 per frame: a longer hitch slows
    /// the game down instead of piling up ticks), interpolating rendered motion between
    /// ticks.
    pub fn new(mut world: World, hz: f64) -> Self {
        let propagation = TransformPropagation::new(&mut world);
        let activity = ActivityPropagation::new(&mut world);
        let mut clock = FixedStep::new(hz);
        clock.max_steps = 4;
        let extract = RenderExtract::new(&mut world, true);
        let lights = LightExtract::new(&mut world);
        let skinned = SkinnedQuery::new(&mut world);
        Self {
            world,
            clock,
            propagation,
            activity,
            extract,
            lights,
            skinned,
            jobs: None,
        }
    }

    /// Spreads each tick's transform propagation and extraction over `jobs`' workers when
    /// many transforms changed (`TransformPropagation::run_parallel`,
    /// [`RenderExtract::sync_on`]: the same results as on one thread).
    pub fn set_jobs(&mut self, jobs: Arc<ThreadPool>) {
        self.jobs = Some(jobs);
    }

    /// The extraction state (instances of entities, last sync's statistics).
    pub fn extract(&self) -> &RenderExtract<Id> {
        &self.extract
    }

    /// The light components, as the renderer takes them: the light list (slots stable)
    /// and the sun.
    pub fn lights(&self) -> &LightExtract {
        &self.lights
    }

    /// One frame, `elapsed` seconds after the previous one, with the input sampled just
    /// before the call:
    ///
    /// 1. every due tick: `tick(world, input, dt)` (the game's systems; they consume the
    ///    input), then transform propagation, then extraction into `scene`;
    /// 2. `present(world, input, alpha)`: what follows input every frame, outside the
    ///    simulation (the player's camera, aim presentation). It runs after the ticks so
    ///    it sees their results, and right before rendering;
    /// 3. interpolation of the instances that moved.
    ///
    /// The caller then renders `scene` (its `add_passes`, then the views).
    pub fn frame<S, I>(
        &mut self,
        elapsed: f64,
        input: &I,
        scene: &mut S,
        tick: impl FnMut(&mut World, &I, f32),
        present: impl FnMut(&mut World, &I, f32),
    ) -> FrameStages
    where
        S: SceneSink<Id = Id>,
    {
        self.frame_with(elapsed, input, scene, tick, |_| {}, present)
    }

    /// [`Self::frame`], `extracted(world)` called after each tick's extraction: where an
    /// extraction of the caller's own keeps what each tick moved, the world's transforms
    /// propagated (the player's sprites, ADR 0125, interpolated between ticks as the
    /// scene's instances are).
    pub fn frame_with<S, I>(
        &mut self,
        elapsed: f64,
        input: &I,
        scene: &mut S,
        mut tick: impl FnMut(&mut World, &I, f32),
        mut extracted: impl FnMut(&mut World),
        mut present: impl FnMut(&mut World, &I, f32),
    ) -> FrameStages
    where
        S: SceneSink<Id = Id>,
    {
        engine_profiler::zone!("runtime::frame");
        let count = self.clock.advance(elapsed as f32);
        let alpha = self.clock.alpha();
        let mut stages = FrameStages {
            ticks: count,
            alpha,
            ..FrameStages::default()
        };
        let dt = self.clock.dt();
        for _ in 0..count {
            let start = Instant::now();
            tick(&mut self.world, input, dt);
            self.world.advance_sim_tick();
            stages.simulate_us += us(start);
            let start = Instant::now();
            self.activity.run(&mut self.world);
            match &self.jobs {
                Some(jobs) => self.propagation.run_parallel(&mut self.world, jobs),
                None => self.propagation.run(&mut self.world),
            };
            stages.propagate_us += us(start);
            let start = Instant::now();
            self.extract
                .sync_on(&mut self.world, scene, self.jobs.as_deref());
            self.lights.sync(&mut self.world);
            extracted(&mut self.world);
            stages.extract_us += us(start);
        }
        let start = Instant::now();
        present(&mut self.world, input, alpha);
        stages.present_us = us(start);
        let start = Instant::now();
        self.extract.interpolate(scene, alpha);
        stages.interpolate_us = us(start);
        stages
    }

    /// One frame whose ticks someone else runs: a network client (ADR 0106), whose
    /// prediction decides how many ticks a frame runs and runs past ones again after a
    /// correction.
    ///
    /// 1. `drive(world, step)` runs the client's frame. It calls `step(world, dt)` for
    ///    every tick it simulates, re-simulated ones too; each step runs `tick` (the
    ///    game's systems) and propagates transforms, so the next tick sees them.
    /// 2. The frame's state, with the remote entities the client interpolated, is
    ///    propagated and extracted into `scene` once.
    /// 3. `present(world, input, alpha)` runs with the driver's fraction of its next tick
    ///    (`Driven::alpha`).
    /// 4. The scene shows the frame as it is: remote entities are where the client put
    ///    them, its predicted ones at their newest tick.
    ///
    /// The world's simulation tick becomes the driver's (`Driven::tick`); the runtime's
    /// own clock stands still.
    pub fn frame_driven<S, I>(
        &mut self,
        input: &I,
        scene: &mut S,
        tick: impl FnMut(&mut World, &I, f32),
        drive: impl FnOnce(&mut World, &mut dyn FnMut(&mut World, f32)) -> Driven,
        present: impl FnMut(&mut World, &I, f32),
    ) -> FrameStages
    where
        S: SceneSink<Id = Id>,
    {
        self.frame_driven_with(input, scene, tick, drive, |_| {}, present)
    }

    /// [`Self::frame_driven`], `extracted(world)` called after the frame's extraction (see
    /// [`Self::frame_with`]).
    pub fn frame_driven_with<S, I>(
        &mut self,
        input: &I,
        scene: &mut S,
        mut tick: impl FnMut(&mut World, &I, f32),
        drive: impl FnOnce(&mut World, &mut dyn FnMut(&mut World, f32)) -> Driven,
        mut extracted: impl FnMut(&mut World),
        mut present: impl FnMut(&mut World, &I, f32),
    ) -> FrameStages
    where
        S: SceneSink<Id = Id>,
    {
        engine_profiler::zone!("runtime::frame_driven");
        let Self {
            world,
            propagation,
            activity,
            extract,
            lights,
            jobs,
            ..
        } = self;
        let mut stages = FrameStages::default();
        let driven = {
            let mut step = |world: &mut World, dt: f32| {
                let start = Instant::now();
                tick(world, input, dt);
                stages.simulate_us += us(start);
                stages.ticks += 1;
                let start = Instant::now();
                activity.run(world);
                match jobs.as_ref() {
                    Some(jobs) => propagation.run_parallel(world, jobs),
                    None => propagation.run(world),
                };
                stages.propagate_us += us(start);
            };
            drive(world, &mut step)
        };
        world.set_sim_tick(driven.tick);
        stages.alpha = driven.alpha;
        let start = Instant::now();
        activity.run(world);
        match jobs.as_ref() {
            Some(jobs) => propagation.run_parallel(world, jobs),
            None => propagation.run(world),
        };
        stages.propagate_us += us(start);
        let start = Instant::now();
        extract.sync_on(world, scene, jobs.as_deref());
        lights.sync(world);
        extracted(world);
        stages.extract_us += us(start);
        let start = Instant::now();
        present(world, input, driven.alpha);
        stages.present_us = us(start);
        let start = Instant::now();
        extract.interpolate(scene, 1.0);
        stages.interpolate_us = us(start);
        stages
    }
}
