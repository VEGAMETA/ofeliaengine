use super::*;

fn folder(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ofelia-luau-analysis-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn type_errors_syntax_errors_and_lint_findings_are_placed() {
    let dir = folder("found");
    let mut a = Analysis::new(&dir, Solver::Old).unwrap();
    a.set_source(
        "door.luau",
        "--!strict\nlocal speed: number = \"fast\"\nreturn speed\n",
    );
    let found = a.check("door.luau");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::Error);
    // The old solver places it on the statement; the range ends in its line.
    assert_eq!((found[0].start.0, found[0].end.0), (1, 1), "{found:?}");
    assert!(found[0].end.1 > found[0].start.1);
    assert!(found[0].message.contains("string"), "{}", found[0].message);
    // A syntax error.
    a.set_source("door.luau", "local x = = 2\n");
    let found = a.check("door.luau");
    assert!(
        found
            .iter()
            .any(|d| d.severity == Severity::Error && d.start.0 == 0),
        "{found:?}"
    );
    // A lint warning, named.
    a.set_source("door.luau", "local unused = 1\nreturn nil\n");
    let found = a.check("door.luau");
    assert!(
        found
            .iter()
            .any(|d| d.severity == Severity::Warning && d.message.starts_with("LocalUnused")),
        "{found:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn requires_resolve_by_string_from_memory_and_the_folder() {
    let dir = folder("require");
    std::fs::create_dir_all(dir.join("shared")).unwrap();
    // On disk only: read when required.
    std::fs::write(
        dir.join("shared/math.luau"),
        "--!strict\nlocal M = {}\nfunction M.double(n: number): number\n    return n * 2\nend\nreturn M\n",
    )
    .unwrap();
    let mut a = Analysis::new(&dir, Solver::Old).unwrap();
    a.set_source(
        "scripts/door.luau",
        "--!strict\nlocal M = require(\"../shared/math\")\nlocal d: number = M.double(\"two\")\nreturn d\n",
    );
    let found = a.check("scripts/door.luau");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].start.0, 2, "the call's line");
    // Fixed in memory: nothing found.
    a.set_source(
        "scripts/door.luau",
        "--!strict\nlocal M = require(\"../shared/math\")\nlocal d: number = M.double(2)\nreturn d\n",
    );
    assert_eq!(a.check("scripts/door.luau"), []);
    // A module that is not there.
    a.set_source(
        "scripts/door.luau",
        "--!strict\nlocal M = require(\"./missing\")\nreturn M\n",
    );
    assert!(!a.check("scripts/door.luau").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn definitions_give_every_module_the_engines_names() {
    let dir = folder("definitions");
    let mut a = Analysis::new(&dir, Solver::Old).unwrap();
    a.add_definitions(
        "engine",
        "declare function engine_log(message: string): ()\n",
    )
    .unwrap();
    a.set_source(
        "door.luau",
        "--!strict\nengine_log(\"open\")\nengine_log(42)\n",
    );
    let found = a.check("door.luau");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].start.0, 2);
    let broken = a.add_definitions("broken", "declare function (: string\n");
    assert!(broken.is_err(), "{broken:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_new_solver_finds_the_same() {
    let dir = folder("new");
    let mut a = Analysis::new(&dir, Solver::New).unwrap();
    a.set_source(
        "door.luau",
        "--!strict\nlocal speed: number = \"fast\"\nreturn speed\n",
    );
    let found = a.check("door.luau");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].start.0, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a check costs: `cargo test --release -p engine_luau_analysis measure_checks --
/// --ignored --nocapture`.
#[test]
#[ignore = "a measurement"]
#[allow(clippy::print_stdout)]
fn measure_checks() {
    use std::time::Instant;
    let dir = folder("measure");
    let door = "local Door{i} = {}\nDoor{i}.speed = 2.5\nfunction Door{i}.on_tick(self, dt: number)\n    local near = self.angle < 3\n    if near and self.angle < 90 then\n        self.angle = math.min(self.angle + Door{i}.speed * dt * 60, 90)\n    elseif not near and self.angle > 0 then\n        self.angle = math.max(self.angle - Door{i}.speed * dt * 60, 0)\n    end\n    print(`{{self.name}} at {{self.angle}}`)\nend\n";
    let text: String = (0..20)
        .map(|i| door.replace("{i}", &i.to_string()))
        .collect();
    for solver in [Solver::Old, Solver::New] {
        let t = Instant::now();
        let mut a = Analysis::new(&dir, solver).unwrap();
        let start = t.elapsed();
        let mut times = Vec::new();
        for k in 0..20 {
            // An edit each time, as typing makes.
            a.set_source("door.luau", &format!("{text}-- {k}\n"));
            let t = Instant::now();
            let found = a.check("door.luau");
            times.push(t.elapsed());
            assert!(found.iter().all(|d| d.severity == Severity::Warning) || !found.is_empty());
        }
        times.sort();
        println!(
            "{solver:?}: started in {start:?}; a check of {} lines after an edit: median {:?}, \
             slowest {:?}",
            text.lines().count(),
            times[times.len() / 2],
            times[times.len() - 1]
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
