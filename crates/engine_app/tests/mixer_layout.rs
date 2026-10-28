//! The mixer's layout file (ADR 0121): the player lays its mixer out from the project's
//! `main.mixer` at the start, and again when the file is written, the world's `Mixer`
//! following with the game's volumes kept.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::prelude::*;
use engine_app::{BusLayout, MixerFile};
use engine_scene::AssetId;

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn buses(player: &mut Player) -> Vec<String> {
    let mixer = player.world_mut().resource::<Mixer>().unwrap();
    mixer.buses().map(str::to_owned).collect()
}

#[test]
fn the_player_follows_its_projects_mixer_file() {
    let dir = std::env::temp_dir().join(format!("ofelia-mixer-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let id = AssetId::new();
    let mut layout = MixerFile::default();
    layout
        .buses
        .insert(String::from("sfx"), BusLayout::new().reverberant());
    layout.buses.insert(
        String::from("steps"),
        BusLayout::new().under("sfx").at(-6.0),
    );
    std::fs::write(dir.join("main.mixer"), layout.to_text(id)).unwrap();
    let settings = PlayerSettings {
        title: String::from("mixer file test"),
        window: Window::Offscreen {
            width: 320,
            height: 180,
        },
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
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
    let mut game = Still;
    assert_eq!(player.mixer_layout_path(), dir.join("main.mixer"));
    // Its buses, and none of the built-in four.
    assert_eq!(buses(&mut player), ["master", "sfx", "steps"]);
    // The game turns steps down on top of its fader.
    assert!(
        player
            .world_mut()
            .resource_mut::<Mixer>()
            .unwrap()
            .set_volume("steps", 0.5)
    );
    // The file written anew (as the editor's watcher says): a music bus, steps under it.
    layout
        .buses
        .insert(String::from("music"), BusLayout::new().at(-3.0));
    layout.buses.get_mut("steps").unwrap().parent = String::from("music");
    std::fs::write(dir.join("main.mixer"), layout.to_text(id)).unwrap();
    player.asset_changed(id, "main.mixer");
    player.frame(&mut game).unwrap();
    assert_eq!(buses(&mut player), ["master", "music", "steps", "sfx"]);
    let mixer = player.world_mut().resource::<Mixer>().unwrap();
    assert_eq!(mixer.volume("steps"), Some(0.5), "the game's volume kept");
    assert_eq!(mixer.volume("music"), Some(1.0));
    // Another file changing does nothing; the file gone, the built-in four.
    player.asset_changed(AssetId::new(), "other.mixer");
    std::fs::remove_file(dir.join("main.mixer")).unwrap();
    player.reload_mixer_layout();
    player.frame(&mut game).unwrap();
    assert_eq!(
        buses(&mut player),
        ["master", "effects", "music", "ui", "voice"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
