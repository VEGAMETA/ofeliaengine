//! `ofeliaengine new <DIR>`: a game started from the quick start
//! (`docs/dev-guide/quick-start.md`). Its manifest depends on this engine's player, its
//! build settings are the engine's, its `src/main.rs` is the quick start's game (a cube on
//! a floor, a camera flown with WASD and the mouse), and its `sounds/` folder is the one
//! the player reads while developing.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The quick start's game, which the player's tests render (`crates/engine_app/tests`).
const MAIN: &str = include_str!("../../../crates/engine_app/examples/quick_start.rs");
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
    let files: [(&str, String); 7] = [
        ("Cargo.toml", manifest(&name, &engine)),
        (".cargo/config.toml", String::from(CONFIG)),
        ("src/main.rs", MAIN.replace("pub(crate) ", "")),
        ("sounds/README.md", String::from(SOUNDS)),
        ("rust-toolchain.toml", String::from(TOOLCHAIN)),
        (".gitignore", String::from("/target\n*.pack\n")),
        ("README.md", readme(&name, &engine)),
    ];
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
    println!("next:\n  cd {}\n  cargo run --release", dir.display());
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
         cargo run --release\n\
         ```\n\n\
         A red cube turns on a floor; fly around it with WASD and the mouse.\n\
         `src/main.rs` is the whole game.\n\n\
         Next: the guides in `{docs}`:\n\
         `quick-start.md` first, then `player.md`, `input.md`, `scenes.md` and the rest.\n"
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
        let manifest = std::fs::read_to_string(game.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("name = \"my_game\""));
        assert!(manifest.contains("/crates/engine_app\" }"));
        assert!(!manifest.contains('\\'));
        let main = std::fs::read_to_string(game.join("src/main.rs")).unwrap();
        assert!(main.contains("fn main()") && !main.contains("pub(crate)"));
        assert!(game.join(".cargo/config.toml").is_file());
        assert!(game.join("sounds/README.md").is_file());
        assert!(game.join("rust-toolchain.toml").is_file());
        // Not over a game already there.
        assert!(run_new(&args).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
