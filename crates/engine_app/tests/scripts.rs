//! Luau scripts in the player (ADR 0114, E7-1a): a project's `scripts/` loaded at start, a
//! script component given to an entity, its hooks run in the fixed ticks only while the
//! world simulates.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::prelude::*;

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

#[test]
fn a_scripts_component_spins_its_entity_in_the_ticks() {
    let dir = std::env::temp_dir().join(format!("ofelia-scripts-player-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("scripts/spin.luau"),
        r#"local Spin = component("Spin", { rate = 1.5 })
function Spin.on_start(self)
    print("spinning", self.entity.name)
end
function Spin.on_tick(self, dt)
    local t = self.Transform
    t.rotation = quat.axis_angle(vector.create(0, 1, 0), self.rate * dt) * t.rotation
end
return Spin
"#,
    )
    .unwrap();
    let settings = PlayerSettings {
        title: String::from("scripts test"),
        window: Window::Offscreen {
            width: 64,
            height: 64,
        },
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        tick_hz: 64.0,
        ..PlayerSettings::default()
    };
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    let names: Vec<String> = player
        .scripts()
        .unwrap()
        .component_names()
        .map(str::to_owned)
        .collect();
    assert_eq!(names, ["Spin"]);
    let cube = player.world_mut().spawn((
        Name::new("Cube"),
        Transform::IDENTITY,
        GlobalTransform::IDENTITY,
    ));
    assert!(player.add_script(cube, "Spin"));
    assert!(!player.add_script(cube, "Nothing"));
    let mut game = Still;
    // Held still: the scripts wait.
    player.set_simulating(false);
    player.set_frame_time(Some(0.25));
    player.frame(&mut game).unwrap();
    assert_eq!(
        player.world_mut().get::<Transform>(cube).unwrap().rotation,
        Quat::IDENTITY
    );
    // Simulating: 1.5 rad/s for every tick the frames ran (64 a second, a few a frame at
    // most).
    player.set_simulating(true);
    player.set_frame_time(Some(1.0 / 32.0));
    let mut ticks = 0;
    for _ in 0..8 {
        ticks += player.frame(&mut game).unwrap().map_or(0, |r| r.ticks);
    }
    assert!(ticks >= 8, "{ticks}");
    let rotation = player.world_mut().get::<Transform>(cube).unwrap().rotation;
    let expected = Quat::from_axis_angle(Vec3::Y, 1.5 * ticks as f32 / 64.0);
    assert!(rotation.angle_between(expected) < 1e-3, "{rotation:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
