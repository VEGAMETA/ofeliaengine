//! The script debugger over DAP (ADR 0119 E9-2): a client speaks to the editor's endpoint
//! as VS Code does. It attaches with the session's token, sets a breakpoint, follows Play
//! to the stop, and reads the stack, the scopes and the variables. It evaluates, steps, goes
//! on, reads a logpoint's message in its console and lets go.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Assets, Game, Model, Player, PlayerSettings, Window, builtin};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{Transform, World};
use engine_editor::{EditorGame, EditorOverlay};
use engine_runtime::{Camera, Sun, look_rotation};
use engine_scene::{AssetId, Name, SaveOptions, SceneId, SceneRegistry, find_named, save_with};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[path = "../../engine_app/tests/common/dap_client.rs"]
mod dap_client;
use dap_client::{Client, event};

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

fn start(_player: &mut Player) -> Result<Still, Box<dyn std::error::Error>> {
    Ok(Still)
}

/// The project's script: it turns its entity about y.
const SPIN: &str = r#"local EditorSpin = component("EditorSpin", { rate = 2 })
function EditorSpin.on_tick(self, dt: number)
    local t = self.Transform
    t.rotation = quat.axis_angle(vector.create(0, 1, 0), self.rate * dt) * t.rotation
end
return EditorSpin
"#;

fn frame<G: Game>(player: &mut Player, game: &mut EditorGame<G>, overlay: &mut EditorOverlay) {
    player.frame_overlaid(game, Some(overlay)).unwrap();
    player.set_view_rect(game.view_rect());
    game.sync_assets(player);
}

/// The scene (a door, the sun, a camera) and the script.
fn write_project(dir: &Path) {
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Front door"),
        Transform::IDENTITY,
        Model::new(builtin::CUBE, builtin::MATERIAL),
    ));
    world.spawn((
        SceneId::new(),
        Name::new("Sun"),
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sun {
            color: Vec3::splat(1.2),
            ambient: Vec3::splat(0.04),
        },
    ));
    let eye = Vec3::new(2.0, 2.0, 6.0);
    world.spawn((
        SceneId::new(),
        Name::new("Camera"),
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        Camera::perspective(1.0, 0.05, 200.0),
    ));
    let paths = Assets::new(dir.to_path_buf()).paths().clone();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
        prefabs: None,
    };
    let mut text = String::new();
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/spin.luau"), SPIN).unwrap();
}

fn player(dir: &Path) -> Option<(Player, PlayerSettings)> {
    let settings = PlayerSettings {
        title: String::from("editor DAP test"),
        window: Window::Offscreen {
            width: 1280,
            height: 720,
        },
        scene: Some("main.scene".into()),
        project: Some(dir.to_path_buf()),
        debuggable_scripts: true,
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    match Player::new(&settings) {
        Ok(player) => Some((player, settings)),
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            None
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

#[test]
fn vs_code_attaches_over_dap_and_debugs_a_stop() {
    // The logger the editor runs with: the scripts' messages reach VS Code's console from
    // its ring.
    let _log = engine_log::init(&engine_log::LogConfig {
        stderr: false,
        ..engine_log::LogConfig::default()
    })
    .unwrap();
    let dir = std::env::temp_dir().join(format!("ofelia-editor-dap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_project(&dir);
    let Some((mut player, settings)) = player(&dir) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (mut game, mut overlay) = engine_editor::editor(&mut player, &settings, start).unwrap();
    game.begin_session(&mut player).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    for _ in 0..3 {
        frame(&mut player, &mut game, &mut overlay);
    }
    let door = find_named(player.world_mut(), "Front door").unwrap();
    assert!(player.add_script(door, "EditorSpin"));
    // Where the editor listens, and its token: what the extension reads.
    let endpoint: PathBuf = dir.join(".ofelia/debug/editor.json");
    let found: Value = serde_json::from_str(&std::fs::read_to_string(&endpoint).unwrap()).unwrap();
    let port = u16::try_from(found["port"].as_u64().unwrap()).unwrap();
    let token = found["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 32);
    let (p, g, o) = (&mut player, &mut game, &mut overlay);
    // A wrong token: refused, and let go.
    let mut client = Client::connect(port);
    let r = client.request(
        &mut || frame(p, g, o),
        "initialize",
        json!({ "adapterID": "ofeliaengine" }),
    );
    assert_eq!(r["success"], true);
    let r = client.request(&mut || frame(p, g, o), "attach", json!({ "token": "nope" }));
    assert_eq!(r["success"], false, "{r}");
    for _ in 0..5 {
        frame(p, g, o);
        client.pump();
    }
    assert!(client.closed, "let go");
    // The right one.
    let mut client = Client::connect(port);
    let r = client.request(
        &mut || frame(p, g, o),
        "initialize",
        json!({ "adapterID": "ofeliaengine" }),
    );
    let filters = r["body"]["exceptionBreakpointFilters"].as_array().unwrap();
    assert_eq!(filters[0]["filter"], "uncaught");
    assert_eq!(filters[0]["default"], true);
    client.wait(&mut || frame(p, g, o), "initialized", event("initialized"));
    let r = client.request(&mut || frame(p, g, o), "attach", json!({ "token": token }));
    assert_eq!(r["success"], true, "{r}");
    let script = std::path::absolute(dir.join("scripts").join("spin.luau")).unwrap();
    let r = client.request(
        &mut || frame(p, g, o),
        "setBreakpoints",
        json!({
            "source": { "path": script },
            "breakpoints": [{ "line": 3 }, { "line": 4, "logMessage": "rate {self.rate}" }],
        }),
    );
    let set = r["body"]["breakpoints"].as_array().unwrap();
    assert!(set.iter().all(|b| b["verified"] == true), "{r}");
    let r = client.request(
        &mut || frame(p, g, o),
        "setExceptionBreakpoints",
        json!({ "filters": ["uncaught"] }),
    );
    assert_eq!(r["success"], true);
    let r = client.request(&mut || frame(p, g, o), "configurationDone", json!({}));
    assert_eq!(r["success"], true);
    // Play: the tick stops at the breakpoint.
    assert!(g.run_command(p.world_mut(), "play.play"));
    let stopped = client.wait(&mut || frame(p, g, o), "the stop", event("stopped"));
    assert_eq!(stopped["body"]["reason"], "breakpoint");
    let r = client.request(&mut || frame(p, g, o), "threads", json!({}));
    assert_eq!(r["body"]["threads"][0]["name"], "Scripts");
    let r = client.request(
        &mut || frame(p, g, o),
        "stackTrace",
        json!({ "threadId": 1 }),
    );
    let top = &r["body"]["stackFrames"][0];
    assert_eq!(
        (top["name"].as_str(), top["line"].as_u64()),
        (Some("on_tick"), Some(3))
    );
    let path = top["source"]["path"].as_str().unwrap().replace('\\', "/");
    assert!(path.ends_with("scripts/spin.luau"), "{path}");
    let r = client.request(&mut || frame(p, g, o), "scopes", json!({ "frameId": 0 }));
    let locals = r["body"]["scopes"][0]["variablesReference"]
        .as_u64()
        .unwrap();
    let r = client.request(
        &mut || frame(p, g, o),
        "variables",
        json!({ "variablesReference": locals }),
    );
    let vars = r["body"]["variables"].as_array().unwrap().clone();
    let this = vars.iter().find(|v| v["name"] == "self").unwrap();
    assert_eq!(this["value"], "EditorSpin · Front door");
    let reference = this["variablesReference"].as_u64().unwrap();
    assert!(reference > 0, "self opens");
    let r = client.request(
        &mut || frame(p, g, o),
        "variables",
        json!({ "variablesReference": reference }),
    );
    let props = r["body"]["variables"].as_array().unwrap();
    assert!(
        props
            .iter()
            .any(|v| v["name"] == "rate" && v["value"] == "2"),
        "{props:#?}"
    );
    let r = client.request(
        &mut || frame(p, g, o),
        "evaluate",
        json!({ "expression": "self.rate * 10", "frameId": 0, "context": "repl" }),
    );
    assert_eq!(r["body"]["result"], "20", "{r}");
    // A step: the next line, `step` its reason; then on, and the logpoint's message comes
    // to the console.
    client.request(&mut || frame(p, g, o), "next", json!({ "threadId": 1 }));
    let stopped = client.wait(&mut || frame(p, g, o), "the step's stop", event("stopped"));
    assert_eq!(stopped["body"]["reason"], "step");
    let r = client.request(
        &mut || frame(p, g, o),
        "stackTrace",
        json!({ "threadId": 1 }),
    );
    assert_eq!(r["body"]["stackFrames"][0]["line"], 4);
    // The breakpoint taken away from VS Code: the editor's set follows.
    client.request(
        &mut || frame(p, g, o),
        "setBreakpoints",
        json!({
            "source": { "path": script },
            "breakpoints": [{ "line": 4, "logMessage": "rate {self.rate}" }],
        }),
    );
    client.request(&mut || frame(p, g, o), "continue", json!({ "threadId": 1 }));
    client.wait(&mut || frame(p, g, o), "continued", event("continued"));
    let said = client.wait(&mut || frame(p, g, o), "the logpoint's message", |m| {
        m["event"] == "output"
            && m["body"]["output"]
                .as_str()
                .is_some_and(|t| t.contains("rate 2"))
    });
    assert_eq!(said["body"]["category"], "console");
    for _ in 0..10 {
        frame(p, g, o);
    }
    client.pump();
    assert!(
        !client.came.iter().any(|m| m["event"] == "stopped"),
        "no stop without the breakpoint: {:#?}",
        client.came
    );
    let r = client.request(&mut || frame(p, g, o), "disconnect", json!({}));
    assert_eq!(r["success"], true);
    // Attach and Play: the open scene plays, and the session's end stops it.
    g.stop_playing(p);
    for _ in 0..2 {
        frame(p, g, o);
    }
    assert!(!g.playing());
    let mut client = Client::connect(port);
    client.request(
        &mut || frame(p, g, o),
        "initialize",
        json!({ "adapterID": "ofeliaengine" }),
    );
    let r = client.request(
        &mut || frame(p, g, o),
        "attach",
        json!({ "token": token, "play": true }),
    );
    assert_eq!(r["success"], true, "{r}");
    for _ in 0..3 {
        frame(p, g, o);
    }
    assert!(g.playing(), "attached and playing");
    client.request(&mut || frame(p, g, o), "disconnect", json!({}));
    for _ in 0..3 {
        frame(p, g, o);
    }
    assert!(!g.playing(), "the session's end stopped Play");
    // The session's end takes the endpoint away.
    game.end_session(player.world_mut());
    assert!(!endpoint.exists());
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
