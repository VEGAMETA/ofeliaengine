//! One frame of the game: input, ticks, presentation, interpolation.

use crate::extract::{RenderExtract, SceneSink};
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
    extract: RenderExtract<Id>,
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
        let mut clock = FixedStep::new(hz);
        clock.max_steps = 4;
        let extract = RenderExtract::new(&mut world, true);
        Self {
            world,
            clock,
            propagation,
            extract,
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
        mut tick: impl FnMut(&mut World, &I, f32),
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
            match &self.jobs {
                Some(jobs) => self.propagation.run_parallel(&mut self.world, jobs),
                None => self.propagation.run(&mut self.world),
            };
            stages.propagate_us += us(start);
            let start = Instant::now();
            self.extract
                .sync_on(&mut self.world, scene, self.jobs.as_deref());
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
}
