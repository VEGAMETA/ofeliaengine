//! What the game gives the player: its hooks, and the parts of the player they reach.

use engine_core::jobs::ThreadPool;
use engine_ecs::World;
use engine_frame::UiCanvas;
use engine_platform::window::WindowEvent;
use engine_render::gpu_scene::{GpuScene, InstanceId};
use engine_render::mesh::GeometryPool;
use engine_render::skinning::Skinning;
use engine_runtime::Runtime;
use std::sync::Arc;

/// The game, as the player's frame loop calls it ([`Player::frame`](crate::Player::frame)).
///
/// One frame:
/// 1. [`Self::event`] for each window event since the last frame;
/// 2. [`Self::tick`] for each fixed tick due (none in some frames, several after a
///    hitch);
/// 3. [`Self::present`]: what follows input every frame, after the ticks;
/// 4. [`Self::animate`]: work on the renderer's parts with the frame's interpolated
///    transforms (skinned poses);
/// 5. [`Self::ui`]: the frame's UI layer, drawn on every presented frame.
pub trait Game {
    /// A window event (input, focus, size), as the window thread reported it.
    fn event(&mut self, event: &WindowEvent) {
        let _ = event;
    }

    /// A fixed tick of the simulation, `dt` seconds long (movement, physics, AI). The
    /// player then propagates transforms and mirrors them into the GPU scene.
    fn tick(&mut self, world: &mut World, dt: f32);

    /// Every rendered frame, after the ticks: what follows input every frame, outside the
    /// simulation (the player's camera). `alpha` is how far the frame lies between the
    /// last two ticks.
    fn present(&mut self, world: &mut World, alpha: f32) {
        let _ = (world, alpha);
    }

    /// Every rendered frame, after the runtime and before rendering, `dt` seconds after
    /// the previous frame: work on the renderer's parts that needs the frame's
    /// interpolated transforms (skinned poses set with their scene instance's transform,
    /// bit for bit).
    fn animate(&mut self, parts: &mut FrameParts<'_>, dt: f32) {
        let _ = (parts, dt);
    }

    /// The frame's UI layer: call `draw` with the canvases (a HUD, panels in the world or
    /// on the camera). Drawn on every presented frame, generated ones too. Not calling it
    /// draws none.
    fn ui(&self, draw: &mut dyn FnMut(&[UiCanvas<'_>])) {
        let _ = draw;
    }
}

/// The parts of the player [`Game::animate`] works on.
pub struct FrameParts<'a> {
    /// The runtime: its world, and which scene instance mirrors an entity
    /// (`extract().instance(entity)`).
    pub runtime: &'a Runtime<InstanceId>,
    /// The GPU scene, its instances' transforms interpolated for this frame.
    pub scene: &'a mut GpuScene,
    /// Skinned meshes: poses and their scene instances.
    pub skinning: &'a mut Skinning,
    /// The geometry pool (skinned bounds).
    pub geometry: &'a mut GeometryPool,
    /// The job system.
    pub jobs: &'a Arc<ThreadPool>,
}
