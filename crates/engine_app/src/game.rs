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

use crate::videos::Videos;

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

    /// A save was taken (ADR 0104), before it is written: add the state of what the game
    /// owns outside the world (`physics.save(world, image)`), or correct what was taken
    /// (`image.exclude(entity)`, `image.set_state(...)`).
    fn before_save(&mut self, world: &World, image: &mut engine_save::SaveImage) {
        let _ = (world, image);
    }

    /// A save is about to be applied: the level's objects destroyed in play are back,
    /// the entities the save makes again removed.
    fn before_load(&mut self, world: &mut World, save: &engine_save::LoadedSave) {
        let _ = (world, save);
    }

    /// A save was applied: restore what the game owns (`physics.restore(world, save)`),
    /// give entities made again what saves do not hold (a body, a mesh), migrate old
    /// saves (`save.version_of(...)`, `report.notes`).
    fn after_load(
        &mut self,
        world: &mut World,
        save: &engine_save::LoadedSave,
        report: &engine_save::LoadReport,
    ) {
        let _ = (world, save, report);
    }

    /// Whether an autosave may happen now (`PlayerSettings::saves.autosave`): not during
    /// a cutscene or a fight, say.
    fn can_autosave(&self, world: &World) -> bool {
        let _ = world;
        true
    }

    /// Networking (ADR 0106): the input for the next tick, from what the player pressed
    /// (`world.resource_mut::<Network>()` … `.set_input(&input)`). Called before every
    /// tick while hosting and every frame while joined; never offline.
    fn net_input(&mut self, world: &mut World) {
        let _ = world;
    }

    /// The physics world sound travels through (ADR 0105): walls between a placed sound
    /// and the listener muffle it, and the room around the listener gives the reverb
    /// (`AudioSettings::acoustics`). With a [`ScenePhysics`](crate::ScenePhysics):
    /// `Some(self.physics.backend())`. `None` (the default): sound ignores the world.
    fn acoustics(&self) -> Option<&dyn engine_physics::PhysicsBackend> {
        None
    }

    /// Every rendered frame, after the ticks: what follows input every frame, outside the
    /// simulation (the player's camera). `alpha` is how far the frame lies between the
    /// last two ticks.
    fn present(&mut self, world: &mut World, alpha: f32) {
        let _ = (world, alpha);
    }

    /// Every rendered frame, after the runtime and before rendering, `dt` seconds after
    /// the previous frame: the animation's skinning matrices
    /// (`parts.skinning.set_palette`). The player then places every
    /// `engine_runtime::SkinnedMesh` at its scene instance.
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

/// Where the editor's "Play from Here" starts the game (ADR 0107 E3): the editor's camera,
/// in the world as a resource while the game's `start` runs and until Stop. A game puts
/// its player there; the scene's main camera is already there.
///
/// ```ignore
/// fn start(player: &mut Player) -> Result<MyGame, Box<dyn std::error::Error>> {
///     let world = player.world_mut();
///     let from = world.resource::<PlayFrom>().copied();
///     let hero = find_named(world, "Hero").ok_or("no hero")?;
///     if let (Some(from), Some(t)) = (from, world.get_mut::<Transform>(hero)) {
///         t.translation = from.position;
///     }
///     Ok(MyGame { hero })
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayFrom {
    /// Where the camera was.
    pub position: engine_core::math::Vec3,
    /// Which way it looked.
    pub rotation: engine_core::math::Quat,
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
    /// Videos (ADR 0100): open, control, show in materials and the UI.
    pub videos: &'a mut Videos,
}
