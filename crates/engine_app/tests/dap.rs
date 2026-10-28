//! VS Code's script debugger in a standalone player (ADR 0119 E9-3): a client attaches to a
//! player started as `--wait-for-debugger` asks, as VS Code does through the ofeliaengine
//! extension. The world waits until the client's configuration is done; then the script
//! stops at its breakpoint with the mouse freed, and the client reads the stack, the values
//! and an evaluation, and steps. It goes on, the logpoint's message comes to the console,
//! and what the client set goes with it. A pause it asked for and left behind does not hold
//! the game.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::DebuggerSettings;
use engine_app::prelude::*;
use engine_app::scripts::ErrorStops;
use serde_json::{Value, json};

#[path = "common/dap_client.rs"]
mod dap_client;
use dap_client::{Client, event};

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

/// The project's script: it turns its entity about y.
const SPIN: &str = r#"local PlayerSpin = component("PlayerSpin", { rate = 2 })
function PlayerSpin.on_tick(self, dt: number)
    local t = self.Transform
    t.rotation = quat.axis_angle(vector.create(0, 1, 0), self.rate * dt) * t.rotation
end
return PlayerSpin
"#;

fn run(player: &mut Player, game: &mut Still) {
    player.frame(game).unwrap();
}

#[test]
fn vs_code_debugs_a_standalone_players_scripts() {
    // The logger a game's `run` installs: the scripts' messages reach VS Code's console
    // from its ring.
    let _log = engine_log::init(&engine_log::LogConfig {
        stderr: false,
        ..engine_log::LogConfig::default()
    })
    .unwrap();
    let dir = std::env::temp_dir().join(format!("ofelia-player-dap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/spin.luau"), SPIN).unwrap();
    let settings = PlayerSettings {
        title: String::from("player DAP test"),
        window: Window::Offscreen {
            width: 64,
            height: 64,
        },
        project: Some(dir.clone()),
        debugger: Some(DebuggerSettings { wait: true }),
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
    let endpoint = dir
        .join(".ofelia/debug")
        .join(format!("player-{}.json", std::process::id()));
    if !cfg!(debug_assertions) {
        // Only a development build listens.
        assert!(!endpoint.exists());
        drop(player);
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let door = player.world_mut().spawn((
        Name::new("Front door"),
        Transform::IDENTITY,
        GlobalTransform::IDENTITY,
    ));
    assert!(player.add_script(door, "PlayerSpin"));
    player.set_frame_time(Some(1.0 / 60.0));
    // The game has the mouse.
    player.lock_cursor(true);
    let mut game = Still;
    // Where it listens, and its token: what the extension reads.
    let found: Value = serde_json::from_str(&std::fs::read_to_string(&endpoint).unwrap()).unwrap();
    assert_eq!(
        (found["kind"].as_str(), found["title"].as_str()),
        (Some("player"), Some("player DAP test"))
    );
    let port = u16::try_from(found["port"].as_u64().unwrap()).unwrap();
    let token = found["token"].as_str().unwrap().to_owned();
    // Waiting for the debugger: the world is kept still.
    for _ in 0..3 {
        run(&mut player, &mut game);
    }
    assert!(!player.simulating());
    assert_eq!(
        player.world_mut().get::<Transform>(door).unwrap().rotation,
        Quat::IDENTITY
    );
    let (p, g) = (&mut player, &mut game);
    let mut client = Client::connect(port);
    let r = client.request(
        &mut || run(p, g),
        "initialize",
        json!({ "adapterID": "ofeliaengine" }),
    );
    let filters = r["body"]["exceptionBreakpointFilters"].as_array().unwrap();
    assert_eq!(
        (
            filters[0]["filter"].as_str(),
            filters[0]["default"].as_bool()
        ),
        (Some("uncaught"), Some(true))
    );
    client.wait(&mut || run(p, g), "initialized", event("initialized"));
    let r = client.request(&mut || run(p, g), "attach", json!({ "token": token }));
    assert_eq!(r["success"], true, "{r}");
    let script = std::path::absolute(dir.join("scripts").join("spin.luau")).unwrap();
    let r = client.request(
        &mut || run(p, g),
        "setBreakpoints",
        json!({
            "source": { "path": script },
            "breakpoints": [{ "line": 3 }, { "line": 4, "logMessage": "rate {self.rate}" }],
        }),
    );
    let set = r["body"]["breakpoints"].as_array().unwrap();
    assert!(set.iter().all(|b| b["verified"] == true), "{r}");
    assert_eq!(set[0]["line"], 3, "placed where asked: the line has code");
    // A file outside the project is not one of the game's scripts.
    let elsewhere = std::path::absolute(std::env::temp_dir().join("elsewhere.luau")).unwrap();
    let r = client.request(
        &mut || run(p, g),
        "setBreakpoints",
        json!({ "source": { "path": elsewhere }, "breakpoints": [{ "line": 1 }] }),
    );
    assert_eq!(r["body"]["breakpoints"][0]["verified"], false, "{r}");
    client.request(
        &mut || run(p, g),
        "setExceptionBreakpoints",
        json!({ "filters": ["uncaught"] }),
    );
    assert!(!p.simulating(), "still waiting for the configuration's end");
    let r = client.request(&mut || run(p, g), "configurationDone", json!({}));
    assert_eq!(r["success"], true);
    assert!(p.simulating(), "the world runs once VS Code is ready");
    // The first tick stops at the breakpoint, the mouse freed.
    let stopped = client.wait(&mut || run(p, g), "the stop", event("stopped"));
    assert_eq!(stopped["body"]["reason"], "breakpoint");
    assert!(!p.cursor_locked(), "the mouse is freed at a stop");
    let r = client.request(&mut || run(p, g), "threads", json!({}));
    assert_eq!(r["body"]["threads"][0]["name"], "Scripts");
    let r = client.request(&mut || run(p, g), "stackTrace", json!({ "threadId": 1 }));
    let top = &r["body"]["stackFrames"][0];
    assert_eq!(
        (top["name"].as_str(), top["line"].as_u64()),
        (Some("on_tick"), Some(3))
    );
    let path = top["source"]["path"].as_str().unwrap().replace('\\', "/");
    assert!(path.ends_with("scripts/spin.luau"), "{path}");
    let r = client.request(&mut || run(p, g), "scopes", json!({ "frameId": 0 }));
    let locals = r["body"]["scopes"][0]["variablesReference"]
        .as_u64()
        .unwrap();
    let r = client.request(
        &mut || run(p, g),
        "variables",
        json!({ "variablesReference": locals }),
    );
    let vars = r["body"]["variables"].as_array().unwrap().clone();
    let this = vars.iter().find(|v| v["name"] == "self").unwrap();
    assert_eq!(this["value"], "PlayerSpin \u{b7} Front door");
    let r = client.request(
        &mut || run(p, g),
        "evaluate",
        json!({ "expression": "self.rate * 10", "frameId": 0, "context": "repl" }),
    );
    assert_eq!(r["body"]["result"], "20", "{r}");
    // A step: the next line, `step` its reason.
    client.request(&mut || run(p, g), "next", json!({ "threadId": 1 }));
    let stopped = client.wait(&mut || run(p, g), "the step's stop", event("stopped"));
    assert_eq!(stopped["body"]["reason"], "step");
    let r = client.request(&mut || run(p, g), "stackTrace", json!({ "threadId": 1 }));
    assert_eq!(r["body"]["stackFrames"][0]["line"], 4);
    // The stop's breakpoint taken away, then on: the logpoint's message comes to the
    // console, and the mouse is the game's again.
    client.request(
        &mut || run(p, g),
        "setBreakpoints",
        json!({
            "source": { "path": script },
            "breakpoints": [{ "line": 4, "logMessage": "rate {self.rate}" }],
        }),
    );
    client.request(&mut || run(p, g), "continue", json!({ "threadId": 1 }));
    client.wait(&mut || run(p, g), "continued", event("continued"));
    let said = client.wait(&mut || run(p, g), "the logpoint's message", |m| {
        m["event"] == "output"
            && m["body"]["output"]
                .as_str()
                .is_some_and(|t| t.contains("rate 2"))
    });
    assert_eq!(said["body"]["category"], "console");
    assert!(p.cursor_locked(), "the mouse is the game's again");
    // The client goes: what it set goes with it.
    let debugger = p.script_debugger().unwrap();
    assert_eq!(debugger.breakpoint_states("spin").len(), 1);
    let r = client.request(&mut || run(p, g), "disconnect", json!({}));
    assert_eq!(r["success"], true);
    for _ in 0..3 {
        run(p, g);
    }
    assert!(
        debugger.breakpoint_states("spin").is_empty(),
        "its breakpoints went with it"
    );
    assert_eq!(debugger.error_stops(), ErrorStops::default());
    // A pause asked for and left behind: reached with nobody to show it to, it goes on.
    let mut client = Client::connect(port);
    client.request(
        &mut || run(p, g),
        "initialize",
        json!({ "adapterID": "ofeliaengine" }),
    );
    client.request(&mut || run(p, g), "attach", json!({ "token": token }));
    let stops = p.script_stops();
    client.send("pause", json!({ "threadId": 1 }));
    client.send("disconnect", json!({}));
    for _ in 0..10 {
        run(p, g);
    }
    assert!(p.script_stops() > stops, "the pause was reached");
    assert!(p.script_stop().is_none(), "and went on");
    // The endpoint's file goes with the player.
    drop(player);
    assert!(!endpoint.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
