//! A script's tab on the player (ADR 0107 E8-1b): the session brings the tab back, its
//! code draws where the viewport is, and keys edit it. `ENGINE_EDITOR_CODE_PNG=<file>`
//! writes the frame; `ENGINE_EDITOR_FIND_PNG=<file>` the find bar's (E8-2).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Game, Player, PlayerSettings, Window};
use engine_ecs::World;
use engine_platform::window::{WindowEvent, WindowEventKind, WindowId};
use std::time::Instant;

struct NoGame;

impl Game for NoGame {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

/// Indented with spaces, and its last lines with tabs (as Roblox Studio writes them): both
/// line up.
const DOOR: &str = "--!strict
-- A door that opens when the player comes near.
local Door = {}

Door.speed = 2.5
Door.open_angle = 90

function Door.on_tick(self, dt: number)
    local near = self:distance_to(self.world.player) < 3
    if near and self.angle < Door.open_angle then
        self.angle = math.min(self.angle + Door.speed * dt * 60, Door.open_angle)
\telseif not near and self.angle > 0 then
\t\tself.angle = math.max(self.angle - Door.speed * dt * 60, 0)
\tend
end

return Door
";

fn typed<G: Game>(game: &mut engine_editor::EditorGame<G>, text: &str) {
    for c in text.chars() {
        game.event(&WindowEvent {
            window: WindowId(0),
            time: Instant::now(),
            kind: WindowEventKind::Char(c),
        });
    }
}

fn write_png(
    player: &mut Player,
    game: &mut engine_editor::EditorGame<NoGame>,
    overlay: &mut engine_editor::EditorOverlay,
    var: &str,
) {
    if let Ok(path) = std::env::var(var) {
        let extent = player.extent();
        let rgba = player
            .capture_overlaid(game, Some(overlay))
            .unwrap()
            .unwrap();
        let png = engine_render::png::encode_png(
            extent.width,
            extent.height,
            engine_render::png::PngFormat::Rgba,
            &rgba,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
}

fn key<G: Game>(game: &mut engine_editor::EditorGame<G>, vk: u16, scancode: u16, down: bool) {
    let extended = matches!(vk, 0x25..=0x28);
    game.event(&WindowEvent {
        window: WindowId(0),
        time: Instant::now(),
        kind: WindowEventKind::Key {
            virtual_key: vk,
            scancode,
            extended,
            pressed: down,
            repeat: false,
        },
    });
}

#[test]
fn a_scripts_tab_draws_its_code_where_the_viewport_is() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-code-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::create_dir_all(dir.join(".ofelia")).unwrap();
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    std::fs::write(dir.join("scripts/door.luau"), DOOR).unwrap();
    // The last session left the script's tab shown, the caret in `on_tick`.
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\nscript = 0\n\n[script]\npath = \
         \"scripts/door.luau\"\ncaret = (8, 4)\nscroll = (0, 0)\n",
    )
    .unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor code test"),
        window: Window::Offscreen { width, height },
        scene: Some("main.scene".into()),
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
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    game.begin_session(&mut player).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    let mut frame = |player: &mut Player, game: &mut engine_editor::EditorGame<NoGame>| {
        player.frame_overlaid(game, Some(&mut overlay)).unwrap();
        player.set_view_rect(game.view_rect());
    };
    for _ in 0..3 {
        frame(&mut player, &mut game);
    }
    // Shift+Down twice: two lines selected from the caret.
    key(&mut game, 0x10, 0x2A, true);
    for _ in 0..2 {
        key(&mut game, 0x28, 0x50, true);
        frame(&mut player, &mut game);
        key(&mut game, 0x28, 0x50, false);
    }
    key(&mut game, 0x10, 0x2A, false);
    frame(&mut player, &mut game);
    let said = game.message().unwrap_or_default();
    assert!(!said.contains("could not"), "{said}");
    write_png(
        &mut player,
        &mut game,
        &mut overlay,
        "ENGINE_EDITOR_CODE_PNG",
    );
    // The find bar, replacing: every `angle` lit, the one it is on more.
    assert!(game.run_command(player.world_mut(), "code.replace"));
    for _ in 0..2 {
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
    }
    assert!(game.run_command(player.world_mut(), "code.find"));
    for _ in 0..2 {
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
    }
    typed(&mut game, "angle");
    for _ in 0..3 {
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
    }
    write_png(
        &mut player,
        &mut game,
        &mut overlay,
        "ENGINE_EDITOR_FIND_PNG",
    );
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
