//! A cube on a floor under the sun, and a camera you fly with WASD and the mouse: the quick
//! start's game (`docs/dev-guide/quick-start.md`), which `ofeliaengine new` starts a game
//! from. Its scene is `scenes/main.scene` in the game's folder (in the engine's
//! repository, `examples/quick_start/`).
//!
//! ```text
//! cargo run --release                     play it
//! cargo run --release --features editor   edit its scene (`ofeliaengine edit`)
//! ```

use engine_app::prelude::*;
use std::time::Instant;

/// The game: what it remembers between frames.
pub(crate) struct MyGame {
    cube: Entity,
    camera: Entity,
    yaw: f32,
    pitch: f32,
    last_frame: Option<Instant>,
}

impl Game for MyGame {
    /// 64 times a second: the simulation. The cube turns.
    fn tick(&mut self, world: &mut World, dt: f32) {
        if let Some(cube) = world.get_mut::<Transform>(self.cube) {
            cube.rotation *= Quat::from_rotation_y(dt);
        }
    }

    /// Every frame, after the ticks: the camera follows the mouse and the keys at once.
    fn present(&mut self, world: &mut World, _alpha: f32) {
        let Some(input) = world.resource::<Input>() else {
            return;
        };
        let [look_x, look_y] = input.frame().vector("look");
        let [walk_x, walk_y] = input.frame().vector("move");
        let now = Instant::now();
        let dt = self.last_frame.map_or(0.0, |t| (now - t).as_secs_f32());
        self.last_frame = Some(now);
        self.yaw -= look_x;
        self.pitch = (self.pitch + look_y).clamp(-1.5, 1.5);
        let rotation = Quat::from_rotation_y(self.yaw) * Quat::from_rotation_x(self.pitch);
        if let Some(camera) = world.get_mut::<Transform>(self.camera) {
            camera.rotation = rotation;
            // Forward is the camera's −Z: 3 metres a second.
            camera.translation += rotation * Vec3::new(walk_x, 0.0, -walk_y) * 3.0 * dt;
        }
    }
}

/// The window, the controls and the scene.
pub(crate) fn settings() -> Result<PlayerSettings, Box<dyn std::error::Error>> {
    let mut controls = InputMap::new();
    let walk = controls.add_axis2d("move")?;
    controls.bind(walk, Binding::wasd())?;
    let look = controls.add_axis2d("look")?;
    controls.bind(look, Binding::mouse_motion(0.003))?;
    Ok(PlayerSettings {
        title: String::from("My game"),
        window: Window::Windowed {
            width: 1280,
            height: 720,
        },
        input: Some(controls),
        // A floor, a cube, the sun, a camera: what the editor saves.
        scene: Some("scenes/main.scene".into()),
        ..PlayerSettings::default()
    })
}

/// The game, once the player has loaded its scene: the cube and the camera, found by their
/// names.
pub(crate) fn start(player: &mut Player) -> Result<MyGame, Box<dyn std::error::Error>> {
    let world = player.world_mut();
    let cube = find_named(world, "Cube").ok_or("the scene has no `Cube`")?;
    let camera = find_named(world, "Camera").ok_or("the scene has no `Camera`")?;
    let lens = *world
        .get::<Camera>(camera)
        .ok_or("the scene's `Camera` has no camera")?;
    let forward = world
        .get::<Transform>(camera)
        .map_or(Vec3::NEG_Z, |t| t.rotation * Vec3::NEG_Z);
    // The scene's lens, with the antialiasing the player chose for this GPU.
    let lens = player.camera(lens.fov_y, lens.near, lens.far);
    player.world_mut().insert(camera, lens);
    Ok(MyGame {
        cube,
        camera,
        yaw: (-forward.x).atan2(-forward.z),
        pitch: forward.y.asin(),
        last_frame: None,
    })
}

// The player around the game; with the `editor` feature, the editor around it.
engine_app::main!(settings, start);
