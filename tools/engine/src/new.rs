//! `ofeliaengine new <DIR>`: a game started from the quick start
//! (`docs/dev-guide/quick-start.md`). Its manifest depends on this engine's player, with
//! the editor behind its `editor` feature (ADR 0107 §1); its build settings are the
//! engine's; its `src/main.rs` is the quick start's game (a camera flown with WASD and the
//! mouse, a cube that turns) and `scenes/main.scene` its scene (a cube on a floor under
//! the sun), with the scene's materials; its `sounds/` folder is the one the player reads
//! while developing.

use engine_scene::AssetId;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The quick start's game, which the player's tests render (`crates/engine_app/tests`).
const MAIN: &str = include_str!("../../../crates/engine_app/examples/quick_start.rs");
/// The quick start's scene and its materials (`crates/engine_app/examples/quick_start/`),
/// by their paths in the game.
const ASSETS: [(&str, &str); 3] = [
    (
        "scenes/main.scene",
        include_str!("../../../crates/engine_app/examples/quick_start/scenes/main.scene"),
    ),
    (
        "materials/floor.material",
        include_str!("../../../crates/engine_app/examples/quick_start/materials/floor.material"),
    ),
    (
        "materials/paint.material",
        include_str!("../../../crates/engine_app/examples/quick_start/materials/paint.material"),
    ),
];
/// The engine's pinned toolchain: the game builds the engine with it.
const TOOLCHAIN: &str = include_str!("../../../rust-toolchain.toml");

pub(crate) struct NewArgs {
    pub(crate) dir: PathBuf,
    pub(crate) engine: Option<PathBuf>,
    pub(crate) name: Option<String>,
}

pub(crate) fn parse_new(mut args: impl Iterator<Item = String>) -> Result<NewArgs, String> {
    let mut parsed = NewArgs {
        dir: PathBuf::new(),
        engine: None,
        name: None,
    };
    let mut dir = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--engine" => parsed.engine = Some(PathBuf::from(value("--engine")?)),
            "--name" => parsed.name = Some(value("--name")?),
            other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
            path if dir.is_none() => dir = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected `{extra}`")),
        }
    }
    parsed.dir = dir.ok_or("no folder given for the game")?;
    Ok(parsed)
}

/// A crate name from a folder's name: lower case, letters, digits and `_`, starting with a
/// letter.
fn crate_name(from: &str) -> String {
    let mut name: String = from
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        name.insert_str(0, "game_");
    }
    name
}

/// The engine this tool was built from.
fn this_engine() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::canonicalize(&root).unwrap_or(root)
}

/// A path as a TOML string's contents: forward slashes, no `\\?\` prefix.
fn toml_path(path: &Path) -> String {
    let text = path.display().to_string();
    text.strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .replace('\\', "/")
}

/// The id in a scene's or a material's header (`[scene version=3 id=<32 hex digits>]`).
fn header_id(text: &str) -> Option<&str> {
    let header = text.lines().next()?;
    header[header.find(" id=")? + 4..].get(..32)
}

/// The quick start's scene and materials with fresh ids, the scene's references to the
/// materials too: assets of two games started from it never share an id, so a material
/// copied from one game to the other stays an asset of its own.
fn assets() -> Result<Vec<(&'static str, String)>, String> {
    let mut files: Vec<(&str, String)> = ASSETS
        .iter()
        .map(|&(path, text)| (path, text.to_owned()))
        .collect();
    for (path, text) in ASSETS {
        let old = header_id(text).ok_or_else(|| format!("{path}: no id in its header"))?;
        let new = AssetId::new().to_string();
        for (_, text) in &mut files {
            *text = text.replace(old, &new);
        }
    }
    Ok(files)
}

pub(crate) fn run_new(args: &NewArgs) -> Result<ExitCode, String> {
    let dir = &args.dir;
    if dir.exists()
        && std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .next()
            .is_some()
    {
        return Err(format!("{} is not empty", dir.display()));
    }
    let engine = match &args.engine {
        Some(engine) => {
            std::fs::canonicalize(engine).map_err(|e| format!("{}: {e}", engine.display()))?
        }
        None => this_engine(),
    };
    if !engine.join("crates/engine_app/Cargo.toml").is_file() {
        return Err(format!(
            "{} is not an ofeliaengine checkout (no crates/engine_app); give it with --engine",
            engine.display()
        ));
    }
    let name = args.name.clone().unwrap_or_else(|| {
        crate_name(&dir.file_name().map_or_else(
            || String::from("game"),
            |n| n.to_string_lossy().into_owned(),
        ))
    });
    let mut files: Vec<(&str, String)> = vec![
        ("Cargo.toml", manifest(&name, &engine)),
        (".cargo/config.toml", String::from(CONFIG)),
        ("src/main.rs", MAIN.replace("pub(crate) ", "")),
        ("sounds/README.md", String::from(SOUNDS)),
        ("rust-toolchain.toml", String::from(TOOLCHAIN)),
        // `.ofelia/` is the editor's: its autosaves, its session, the asset cache.
        (".gitignore", String::from("/target\n*.pack\n/.ofelia/\n")),
        ("README.md", readme(&name, &engine)),
    ];
    files.extend(assets()?);
    for (path, text) in &files {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    println!(
        "a game `{name}` in {}, on the engine in {}",
        dir.display(),
        toml_path(&engine)
    );
    println!(
        "next:\n  cd {}\n  cargo run --release                     play it\n  \
         cargo run --release --features editor   edit its scene (`ofeliaengine edit`)",
        dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn manifest(name: &str, engine: &Path) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
# The player (`engine_app`): the window, the GPU, the renderer, input, sound, saves,
# networking, driven by your game's hooks. `engine_app::prelude` has what a game uses most.
engine_app = {{ path = "{engine}/crates/engine_app" }}
# The editor around your game, built only with the `editor` feature: a shipped game does
# not link it.
engine_editor = {{ path = "{engine}/crates/engine_editor", optional = true }}

[features]
# `cargo run --release --features editor` (or `ofeliaengine edit`): `engine_app::main!`
# runs the editor around the game.
editor = ["dep:engine_editor"]

# A workspace of its own, even inside another folder that has one.
[workspace]

# The engine's build settings: your code builds fast, the engine runs fast.
[profile.dev]
opt-level = 1

[profile.dev.package."*"]
opt-level = 3

[profile.release]
opt-level = 3
debug = false
incremental = false

# What players get: `cargo build --profile shipping`.
[profile.shipping]
inherits = "release"
lto = "fat"
codegen-units = 1
panic = "abort"
"#,
        engine = toml_path(engine),
    )
}

/// The engine's linker and instruction set (CLAUDE.md §5: the Windows 11 24H2 minimum).
const CONFIG: &str = r#"[target.x86_64-pc-windows-msvc]
# `rust-lld` ships with rustup; nothing else to install.
linker = "rust-lld.exe"
# The instruction set every Windows 11 24H2 machine has.
rustflags = ["-C", "target-cpu=x86-64-v2"]
"#;

const SOUNDS: &str = "# Sounds\n\nYour game's sound files (WAV, Ogg Vorbis, Opus, FLAC), by their paths from here:\n`AudioSource::new(\"music/theme.ogg\")`. While you develop the game plays a changed file at\nonce; for a release, `ofeliaengine sounds build sounds -o sounds.pack`.\n";

fn readme(name: &str, engine: &Path) -> String {
    let docs = toml_path(&engine.join("docs/dev-guide"));
    format!(
        "# {name}\n\n\
         A game on ofeliaengine, started from its quick start.\n\n\
         ```text\n\
         cargo run --release                     # play it\n\
         cargo run --release --features editor   # edit its scene (`ofeliaengine edit`)\n\
         ```\n\n\
         A red cube turns on a floor; fly around it with WASD and the mouse.\n\
         `src/main.rs` is the game's code, `scenes/main.scene` its scene (what the editor\n\
         edits) and `materials/` the scene's materials.\n\n\
         Next: the guides in `{docs}`:\n\
         `quick-start.md` first, then `editor.md`, `player.md`, `input.md`, `scenes.md` and\n\
         the rest.\n"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn folder_names_become_crate_names() {
        assert_eq!(crate_name("My Game"), "my_game");
        assert_eq!(crate_name("space-shooter"), "space_shooter");
        assert_eq!(crate_name("2048"), "game_2048");
    }

    #[test]
    fn a_new_game_has_its_files_and_refuses_a_full_folder() {
        let dir = std::env::temp_dir().join(format!("ofelia-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let game = dir.join("My Game");
        let args = NewArgs {
            dir: game.clone(),
            engine: None,
            name: None,
        };
        run_new(&args).unwrap();
        let read = |path: &str| std::fs::read_to_string(game.join(path)).unwrap();
        let manifest = read("Cargo.toml");
        assert!(manifest.contains("name = \"my_game\""));
        assert!(manifest.contains("/crates/engine_app\" }"));
        assert!(manifest.contains("/crates/engine_editor\", optional = true }"));
        // What `ofeliaengine edit` looks for.
        assert!(crate::edit::has_editor_feature(&manifest));
        assert!(!manifest.contains('\\'));
        let main = read("src/main.rs");
        assert!(
            main.contains("engine_app::main!(settings, start);") && !main.contains("pub(crate)")
        );
        assert!(read(".gitignore").lines().any(|l| l == "/.ofelia/"));
        assert!(game.join(".cargo/config.toml").is_file());
        assert!(game.join("sounds/README.md").is_file());
        assert!(game.join("rust-toolchain.toml").is_file());

        // The scene and its materials, under ids of their own: not the template's, and
        // the scene names the materials by theirs.
        let scene = read("scenes/main.scene");
        let paint = read("materials/paint.material");
        let floor = read("materials/floor.material");
        for (path, text) in ASSETS {
            let old = header_id(text).unwrap();
            for new in [&scene, &paint, &floor] {
                assert!(!new.contains(old), "{path}'s id stayed");
            }
        }
        let paint_id = header_id(&paint).unwrap();
        assert!(scene.contains(&format!("asset(\"materials/paint.material\", {paint_id})")));
        let floor_id = header_id(&floor).unwrap();
        assert!(scene.contains(&format!("asset(\"materials/floor.material\", {floor_id})")));
        // The engine's built-in cube keeps its id.
        assert!(scene.contains("asset(\"builtin#cube\", 6f66656c6961206275696c74696e0001)"));

        // Not over a game already there.
        assert!(run_new(&args).is_err());
        // Another game gets other ids.
        let other = dir.join("Other");
        run_new(&NewArgs {
            dir: other.clone(),
            engine: None,
            name: None,
        })
        .unwrap();
        let other_scene = std::fs::read_to_string(other.join("scenes/main.scene")).unwrap();
        assert_ne!(header_id(&other_scene), header_id(&scene));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
