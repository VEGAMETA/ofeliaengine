//! A script's error inside an engine binding stops its instance, not the game, in every
//! profile (ADR 0118): Luau's errors are C++ exceptions, which unwind through the engine's
//! callbacks. `tools/check.ps1 -Profiles` runs it built with `shipping`; a profile whose
//! panics abort would end the process at the first error.
//!
//! ```text
//! cargo run --profile shipping -p engine_app --example script_errors
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use engine_app::scripts::Scripts;
use engine_ecs::{Transform, World};
use std::process::ExitCode;

/// A script that reads a component its entity lacks (an error an engine function returns),
/// one that writes a prop a value of the wrong type (an error the fast path raises), and
/// one that fails in Luau itself.
const SCRIPTS: [(&str, &str); 3] = [
    (
        "lacks.luau",
        r#"local Lacks = component("Lacks", { ticks = 0 })
function Lacks.on_tick(self, dt: number)
    self.ticks += 1
    local missing = self.Name
end
return Lacks
"#,
    ),
    (
        "wrong.luau",
        r#"local Wrong = component("Wrong", { speed = 1 })
function Wrong.on_tick(self, dt: number)
    self.speed = "fast"
end
return Wrong
"#,
    ),
    (
        "fails.luau",
        r#"local Fails = component("Fails", {})
function Fails.on_tick(self, dt: number)
    error("on purpose")
end
return Fails
"#,
    ),
];

fn main() -> ExitCode {
    let dir = std::env::temp_dir().join(format!("ofelia-script-errors-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (file, text) in SCRIPTS {
        let path = dir.join("scripts").join(file);
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!("{}: {e}", parent.display());
            return ExitCode::FAILURE;
        }
        if let Err(e) = std::fs::write(&path, text) {
            eprintln!("{}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }
    let mut world = World::new();
    let mut scripts = match Scripts::load(&mut world, &dir, &dir.join("scripts")) {
        Ok(scripts) => scripts,
        Err(e) => {
            eprintln!("the scripts did not load: {e}");
            return ExitCode::FAILURE;
        }
    };
    for name in ["Lacks", "Wrong", "Fails"] {
        let e = world.spawn((Transform::IDENTITY,));
        if !scripts.add(&mut world, e, name) {
            eprintln!("no component {name}");
            return ExitCode::FAILURE;
        }
    }
    // Each fails in its first tick, and stops; the next ticks go on without them.
    for _ in 0..3 {
        scripts.tick(&mut world, 1.0 / 64.0);
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("the errors stopped their instances; the game went on");
    ExitCode::SUCCESS
}
