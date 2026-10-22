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
fn completions_and_types_come_from_the_checked_module() {
    for solver in [Solver::New, Solver::Old] {
        let dir = folder("complete");
        let mut a = Analysis::new(&dir, solver).unwrap();
        let text = "--!strict\nlocal Door = { speed = 2.5, open_angle = 90 }\nfunction Door.open(self, by: number): number\n    return by\nend\nlocal n = Door.\nreturn Door\n";
        a.set_source("door.luau", text);
        let _ = a.check("door.luau");
        // After `Door.`: its fields and its function.
        let found = a.complete("door.luau", 5, 15);
        let labels: Vec<&str> = found.iter().map(|c| c.label.as_str()).collect();
        for field in ["open", "open_angle", "speed"] {
            assert!(labels.contains(&field), "{solver:?}: {labels:?}");
        }
        assert!(
            found.iter().all(|c| c.kind == CompletionKind::Property),
            "{solver:?}: {found:?}"
        );
        let speed = found.iter().find(|c| c.label == "speed").unwrap();
        assert_eq!(speed.detail, "number");
        // What `by` is where it is used and where it is declared, and `Door` where it is.
        assert_eq!(
            a.type_at("door.luau", 3, 12).as_deref(),
            Some("number"),
            "{solver:?}"
        );
        assert_eq!(
            a.type_at("door.luau", 2, 26).as_deref(),
            Some("number"),
            "{solver:?}"
        );
        let door = a.type_at("door.luau", 1, 7).unwrap_or_default();
        assert!(door.contains("speed"), "{solver:?}: {door}");
        // On a line of its own: the locals and the keywords, `Door` among them.
        a.set_source(
            "door.luau",
            "--!strict\nlocal Door = { speed = 2.5 }\n\nreturn Door\n",
        );
        let _ = a.check("door.luau");
        let found = a.complete("door.luau", 2, 0);
        assert!(
            found.iter().any(|c| c.label == "Door"),
            "{solver:?}: {found:?}"
        );
        assert!(
            found
                .iter()
                .any(|c| c.label == "local" && c.kind == CompletionKind::Keyword)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn definitions_are_found_in_the_module_and_across_requires() {
    for solver in [Solver::New, Solver::Old] {
        let dir = folder("definition");
        std::fs::create_dir_all(dir.join("shared")).unwrap();
        std::fs::write(
            dir.join("shared/math.luau"),
            "local M = {}\nfunction M.double(n: number): number\n    return n * 2\nend\nreturn M\n",
        )
        .unwrap();
        let mut a = Analysis::new(&dir, solver).unwrap();
        a.set_source(
            "door.luau",
            "local M = require(\"./shared/math\")\nlocal speed = 2\nreturn M.double(speed)\n",
        );
        let _ = a.check("door.luau");
        // `speed` where it is used: its local.
        assert_eq!(
            a.definition("door.luau", 2, 17),
            Some((String::from("door.luau"), 1, 6)),
            "{solver:?}"
        );
        // `double` of the required module: there.
        let (module, line, _) = a.definition("door.luau", 2, 11).unwrap();
        assert_eq!(
            (module.as_str(), line),
            ("shared/math.luau", 1),
            "{solver:?}"
        );
        // `print`, Luau's own: nowhere in the project.
        a.set_source("door.luau", "print(1)\n");
        let _ = a.check("door.luau");
        assert_eq!(a.definition("door.luau", 0, 2), None, "{solver:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_calls_signature_shows_the_argument_being_written() {
    for solver in [Solver::New, Solver::Old] {
        let dir = folder("signature");
        let mut a = Analysis::new(&dir, solver).unwrap();
        a.set_source(
            "door.luau",
            "local function open(by: number, speed: number): boolean\n    return by > speed\nend\nlocal ok = open(1, 2)\nreturn ok\n",
        );
        let _ = a.check("door.luau");
        // In `open(1, 2)`: the second argument.
        let (text, (start, end)) = a.signature("door.luau", 3, 19).unwrap();
        assert_eq!(
            text, "open(by: number, speed: number): boolean",
            "{solver:?}"
        );
        assert_eq!(&text[start..end], "speed: number", "{solver:?}");
        let (_, (start, end)) = a.signature("door.luau", 3, 16).unwrap();
        assert_eq!(&text[start..end], "by: number", "{solver:?}");
        // Outside the parentheses: none.
        assert_eq!(a.signature("door.luau", 3, 3), None, "{solver:?}");
        assert_eq!(
            a.signature("door.luau", 3, 21),
            None,
            "after `)`: {solver:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
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
