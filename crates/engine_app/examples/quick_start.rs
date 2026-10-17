//! A cube on a floor under the sun, and a camera you fly with WASD and the mouse: the quick
//! start's game (`docs/dev-guide/quick-start.md`). `ofeliaengine new` starts a game from
//! this file.
//!
//! ```text
//! cargo run --release
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

/// The window and the controls.
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
        ..PlayerSettings::default()
    })
}

/// The scene: a floor, a cube, the sun, a camera.
pub(crate) fn scene(player: &mut Player) -> Result<MyGame, Box<dyn std::error::Error>> {
    let setup = player.setup();
    let mesh = setup.geometry.add_mesh(&cube().as_data())?;
    let stone = setup
        .materials
        .add(MaterialDesc::solid([0.3, 0.3, 0.32], 0.0, 0.9));
    let paint = setup
        .materials
        .add(MaterialDesc::solid([0.8, 0.15, 0.1], 0.0, 0.4));
    setup.world.spawn((
        Transform::from_xyz(0.0, -0.55, 0.0).with_scale(Vec3::new(20.0, 0.1, 20.0)),
        GlobalTransform::IDENTITY,
        MeshRenderer::new(mesh, stone),
    ));
    let cube = setup.world.spawn((
        Transform::from_xyz(0.0, 0.0, 0.0),
        GlobalTransform::IDENTITY,
        MeshRenderer::new(mesh, paint),
    ));
    setup.world.spawn((
        GlobalTransform(Affine3A::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let eye = Vec3::new(2.0, 1.2, 3.0);
    let lens = player.camera(1.0, 0.05, 200.0);
    let camera = player.world_mut().spawn((
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        lens,
    ));
    let (yaw, pitch) = {
        let look = Vec3::ZERO - eye;
        ((-look.x).atan2(-look.z), (look.y / look.length()).asin())
    };
    Ok(MyGame {
        cube,
        camera,
        yaw,
        pitch,
        last_frame: None,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut player = Player::new(&settings()?)?;
    let mut game = scene(&mut player)?;
    player.run(&mut game)?;
    Ok(())
}
