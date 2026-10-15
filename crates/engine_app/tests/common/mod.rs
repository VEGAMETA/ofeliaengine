//! A small game for the player's tests: spinning cubes over a floor, a camera.

#![allow(dead_code, unreachable_pub, clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{FrameParts, Game, Player, PlayerSettings, Window};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{GlobalTransform, QueryState, Transform, World, bytemuck, pod_component};
use engine_render::material::MaterialDesc;
use engine_render::mesh::cube;
use engine_runtime::{MeshRenderer, PointLight, Sun};

/// Turns its entity about Y, radians per second.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Spin(pub f32);
pod_component!(Spin);

/// What the hooks saw.
#[derive(Debug, Default)]
pub struct Calls {
    pub events: usize,
    pub ticks: usize,
    pub presents: usize,
    pub animates: usize,
}

pub struct Cubes {
    spins: QueryState<(&'static mut Transform, &'static Spin)>,
    pub calls: Calls,
}

impl Game for Cubes {
    fn event(&mut self, _event: &engine_platform::window::WindowEvent) {
        self.calls.events += 1;
    }

    fn tick(&mut self, world: &mut World, dt: f32) {
        self.calls.ticks += 1;
        self.spins.for_each(world, |(t, spin)| {
            t.rotation *= Quat::from_rotation_y(spin.0 * dt);
        });
    }

    fn present(&mut self, _world: &mut World, _alpha: f32) {
        self.calls.presents += 1;
    }

    fn animate(&mut self, _parts: &mut FrameParts<'_>, _dt: f32) {
        self.calls.animates += 1;
    }
}

/// A player in a small window with the test game's world, or `None` (with a note)
/// without a desktop or a GPU, unless `ENGINE_REQUIRE_GPU=1`.
pub fn player() -> Option<(Player, Cubes)> {
    player_with(|_| {})
}

/// [`player`] with its settings changed by `adjust`.
pub fn player_with(adjust: impl FnOnce(&mut PlayerSettings)) -> Option<(Player, Cubes)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let mut settings = PlayerSettings {
        title: String::from("engine_app test"),
        window: Window::Windowed {
            width: 320,
            height: 240,
        },
        // The test process stays as Windows started it; no cache file is written.
        process_setup: false,
        pipeline_cache: Some(None),
        // The warm-up thread compiles shading variants for seconds after start-up (ADR
        // 0079): its allocations are not the frame's.
        warm_up: false,
        ..PlayerSettings::default()
    };
    adjust(&mut settings);
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            return None;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    // A floor and a ring of spinning cubes, a camera looking at them.
    {
        let setup = player.setup();
        let mesh = setup.geometry.add_mesh(&cube().as_data()).unwrap();
        let floor = setup
            .materials
            .add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 0.8));
        let paint = setup
            .materials
            .add(MaterialDesc::solid([0.8, 0.3, 0.2], 0.0, 0.4));
        setup.world.spawn((
            Transform::from_xyz(0.0, -0.6, 0.0).with_scale(Vec3::new(10.0, 0.2, 10.0)),
            GlobalTransform::IDENTITY,
            MeshRenderer::new(mesh, floor),
        ));
        // A sun from above and a light in the middle (components, ADR 0099 step 3).
        setup.world.spawn((
            GlobalTransform(engine_core::math::Affine3A::from_rotation_translation(
                Quat::from_rotation_x(-1.2),
                Vec3::ZERO,
            )),
            Sun {
                color: Vec3::splat(2.0),
                ambient: Vec3::splat(0.05),
            },
        ));
        setup.world.spawn((
            GlobalTransform(engine_core::math::Affine3A::from_translation(Vec3::new(
                0.0, 1.0, 0.0,
            ))),
            PointLight::new(Vec3::new(8.0, 6.0, 4.0), 4.0).with_shadow(),
        ));
        for k in 0..8 {
            let a = k as f32 * core::f32::consts::TAU / 8.0;
            setup.world.spawn((
                Transform::from_xyz(2.0 * a.cos(), 0.0, 2.0 * a.sin()).with_scale(Vec3::splat(0.5)),
                GlobalTransform::IDENTITY,
                MeshRenderer::new(mesh, paint),
                Spin(1.0 + k as f32 * 0.1),
            ));
        }
    }
    let camera = player.camera(1.0, 0.05, 100.0);
    let eye = Vec3::new(0.0, 2.0, 6.0);
    player.world_mut().spawn((
        Transform::from_translation(eye).with_rotation(engine_runtime::look_rotation(
            eye,
            Vec3::ZERO,
            Vec3::Y,
        )),
        camera,
    ));
    // The same game time every frame: as many ticks on every machine.
    player.set_frame_time(Some(1.0 / 60.0));
    let game = Cubes {
        spins: QueryState::new(player.world_mut()),
        calls: Calls::default(),
    };
    Some((player, game))
}
