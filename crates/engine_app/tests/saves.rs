//! Saves in the player (ADR 0104): a quicksave taken after a frame's ticks and written
//! behind it, then a quickload that brings back everything the game had: a named entity's
//! state, its timer and tween where they were, a resource, the spawned objects; the
//! clock goes on from the save. Offscreen, so it runs without a window.

#![allow(clippy::unwrap_used, clippy::float_cmp, clippy::print_stdout)]

mod common;

use engine_app::{Encoding, Game, Player, SceneId, Window};
use engine_ecs::reflect::Reflect;
use engine_ecs::{QueryState, Transform, World, reflect_component, reflect_resource};
use engine_runtime::{MeshRenderer, Timer, Tween};
use std::time::{Duration, Instant};

#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq)]
struct Counter {
    ticks: u32,
}
reflect_component!(Counter);

#[derive(Reflect, Default, Debug, PartialEq, Clone)]
struct Score {
    points: u32,
    name: String,
}
reflect_resource!(Score);

/// Counts the hero's ticks.
struct Counting {
    counters: Option<QueryState<&'static mut Counter>>,
}

impl Game for Counting {
    fn tick(&mut self, world: &mut World, _dt: f32) {
        let q = self.counters.get_or_insert_with(|| QueryState::new(world));
        q.for_each(world, |c| c.ticks += 1);
    }
}

fn hero(player: &mut Player) -> engine_ecs::Entity {
    let world = player.world_mut();
    let mut q = QueryState::<(engine_ecs::Entity, &SceneId)>::new(world);
    q.iter_readonly(world)
        .find(|(_, id)| **id == SceneId::named("hero"))
        .map(|(e, _)| e)
        .unwrap()
}

fn state(player: &mut Player) -> (Counter, Timer, Tween, Transform) {
    let e = hero(player);
    let world = player.world_mut();
    (
        *world.get::<Counter>(e).unwrap(),
        *world.get::<Timer>(e).unwrap(),
        world.get::<Tween>(e).unwrap().clone(),
        *world.get::<Transform>(e).unwrap(),
    )
}

fn renderers(player: &mut Player) -> usize {
    let world = player.world_mut();
    let mut q = QueryState::<&MeshRenderer>::new(world);
    q.iter_readonly(world).count()
}

fn wait_written(player: &mut Player) {
    let start = Instant::now();
    while player.saves_mut().unwrap().writing() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the save is written"
        );
        std::thread::yield_now();
    }
}

#[test]
fn a_quickload_brings_back_what_the_quicksave_took() {
    let folder = std::env::temp_dir().join(format!("engine_app_saves_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    let Some((mut player, _)) = common::player_with(|settings| {
        settings.window = Window::Offscreen {
            width: 64,
            height: 64,
        };
        settings.saves.folder = Some(folder.clone());
        settings.saves.encoding = Some(Encoding::Text);
        settings.saves.game_version = "test 1".to_owned();
    }) else {
        return;
    };
    player.world_mut().spawn((
        SceneId::named("hero"),
        Counter::default(),
        Transform::default(),
        Timer::repeating(0.25),
        Tween::new(
            "Transform.translation",
            &[0.0, 0.0, 0.0],
            &[10.0, 0.0, 0.0],
            2.0,
        ),
    ));
    player.world_mut().insert_resource(Score {
        points: 7,
        name: "Ofelia".to_owned(),
    });
    // A tick per frame.
    player.set_frame_time(Some(1.0 / 64.0));
    let mut game = Counting { counters: None };
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    let cubes = renderers(&mut player);
    player.saves_mut().unwrap().quicksave();
    player.frame(&mut game).unwrap();
    wait_written(&mut player);
    let saved = state(&mut player);
    assert_eq!(saved.0.ticks, 11, "taken after the frame's tick");
    let written = player.saves_mut().unwrap().last_write().unwrap().unwrap();
    assert_eq!(written.0, "quick");
    println!("quicksave: {:?}", written.1);

    // Play on: the state moves, the score changes, a cube is gone.
    for _ in 0..20 {
        player.frame(&mut game).unwrap();
    }
    player.world_mut().resource_mut::<Score>().unwrap().points = 1000;
    let gone = {
        let world = player.world_mut();
        let mut q = QueryState::<(engine_ecs::Entity, &MeshRenderer)>::new(world);
        q.iter_readonly(world).next().map(|(e, _)| e).unwrap()
    };
    player.world_mut().despawn(gone);
    assert_ne!(state(&mut player).0, saved.0);

    // Back: applied after the next frame's tick, at the tick boundary.
    player.saves_mut().unwrap().quickload();
    player.frame(&mut game).unwrap();
    let report = player
        .saves_mut()
        .unwrap()
        .last_load()
        .cloned()
        .unwrap()
        .unwrap();
    println!("quickload: {report:?}");
    let loaded = state(&mut player);
    assert_eq!(loaded.0, saved.0, "the hero's own state");
    assert_eq!(
        loaded.1.remaining, saved.1.remaining,
        "the timer where it was"
    );
    assert_eq!(loaded.2.elapsed, saved.2.elapsed, "the tween where it was");
    assert_eq!(loaded.3.translation, saved.3.translation);
    assert_eq!(
        player.world_mut().resource::<Score>(),
        Some(&Score {
            points: 7,
            name: "Ofelia".to_owned()
        })
    );
    assert_eq!(
        renderers(&mut player),
        cubes,
        "the spawned objects made again"
    );
    // And it goes on from there: the tween moves the hero on.
    player.frame(&mut game).unwrap();
    let after = state(&mut player);
    assert_eq!(after.0.ticks, saved.0.ticks + 1);
    assert!(after.2.elapsed > saved.2.elapsed);
    assert!(after.3.translation.x > saved.3.translation.x);

    // The slot is in the folder, readable text.
    let slots = player.saves_mut().unwrap().slots();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].header.game_version, "test 1");
    let text = engine_save::dump(&std::fs::read(&slots[0].path).unwrap()).unwrap();
    assert!(text.contains("Counter.ticks = 11"), "{text}");
    assert!(text.contains("[state clock ClockState]"), "{text}");
    drop(player);
    let _ = std::fs::remove_dir_all(&folder);
}
