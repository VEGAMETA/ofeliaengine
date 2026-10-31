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

/// The VS Code extension (ADR 0119 E9-2, `tools/engine/vscode/ofeliaengine`): its files in the
/// `.vsix` the game gets, under `extension/`.
const EXTENSION: [(&str, &str); 3] = [
    (
        "package.json",
        include_str!("../vscode/ofeliaengine/package.json"),
    ),
    (
        "extension.js",
        include_str!("../vscode/ofeliaengine/extension.js"),
    ),
    (
        "README.md",
        include_str!("../vscode/ofeliaengine/README.md"),
    ),
];

/// The extension's version (its `package.json`'s).
const EXTENSION_VERSION: &str = "0.2.0";

/// The game's debug configurations: attaching to the editor, attaching while the editor
/// plays the open scene, and attaching to a player started with `--debug`.
const VSCODE_LAUNCH: &str = r#"{
    "version": "0.2.0",
    "configurations": [
        {
            "type": "ofeliaengine",
            "request": "attach",
            "name": "Attach to the editor"
        },
        {
            "type": "ofeliaengine",
            "request": "attach",
            "name": "Attach and Play",
            "play": true
        },
        {
            "type": "ofeliaengine",
            "request": "attach",
            "name": "Attach to the player",
            "target": "player"
        }
    ]
}
"#;

/// VS Code recommends the extension for the game's folder.
const VSCODE_EXTENSIONS: &str = r#"{
    "recommendations": [
        "ofeliaengine.ofeliaengine",
        "johnnymorganz.luau-lsp"
    ]
}
"#;

pub(crate) struct NewArgs {
    pub(crate) dir: PathBuf,
    pub(crate) engine: Option<PathBuf>,
    pub(crate) name: Option<String>,
    /// Install the VS Code extension where VS Code's `code` is found (`--no-extension`
    /// leaves it in `.vscode/` only).
    pub(crate) install_extension: bool,
}

pub(crate) fn parse_new(mut args: impl Iterator<Item = String>) -> Result<NewArgs, String> {
    let mut parsed = NewArgs {
        dir: PathBuf::new(),
        engine: None,
        name: None,
        install_extension: true,
    };
    let mut dir = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--engine" => parsed.engine = Some(PathBuf::from(value("--engine")?)),
            "--name" => parsed.name = Some(value("--name")?),
            "--no-extension" => parsed.install_extension = false,
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

/// VS Code's settings for luau-lsp (checked with luau-lsp 1.70.1, 2026-10-22): Luau without
/// Roblox's API, no Rojo sourcemap, the engine's types (`types/ofelia.d.luau`, written by
/// the editor, ADR 0114 E7-2) under `@ofelia`, and the old type solver, which types a
/// hook's `self` from `component` (as the editor's checker does).
const VSCODE_SETTINGS: &str = r#"{
    "luau-lsp.platform.type": "standard",
    "luau-lsp.sourcemap.enabled": false,
    "luau-lsp.types.definitionFiles": {
        "@ofelia": "types/ofelia.d.luau"
    },
    "luau-lsp.fflags.enableNewSolver": false
}
"#;

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
        (".cargo/config.toml", config(&name)),
        ("src/main.rs", MAIN.replace("pub(crate) ", "")),
        ("sounds/README.md", String::from(SOUNDS)),
        ("rust-toolchain.toml", String::from(TOOLCHAIN)),
        // `.ofelia/` is the editor's: its autosaves, its session, the asset cache; `types/`
        // the scripts' types it writes.
        (
            ".gitignore",
            String::from("/target\n*.pack\n/.ofelia/\n/types/\n/builds/\n"),
        ),
        (".vscode/settings.json", String::from(VSCODE_SETTINGS)),
        (".vscode/launch.json", String::from(VSCODE_LAUNCH)),
        (".vscode/extensions.json", String::from(VSCODE_EXTENSIONS)),
        ("README.md", readme(&name, &engine)),
        // Its build (ADR 0126): `ofeliaengine build` and the editor's Build read it.
        (
            engine_build::BUILD_FILE,
            engine_build::BuildFile::for_game(Path::new(""), &name, "scenes/main.scene")
                .to_text(AssetId::new()),
        ),
    ];
    files.extend(assets()?);
    // The engine's dependencies at the versions it is tested with, which every game on it
    // then shares: a lock of the game's own would take the newest ones (ADR 0111).
    if let Ok(lock) = std::fs::read_to_string(engine.join("Cargo.lock")) {
        files.push(("Cargo.lock", lock));
    }
    for (path, text) in &files {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    // The VS Code extension that debugs the scripts (ADR 0119 E9-2), installed where VS
    // Code is.
    let vsix = dir
        .join(".vscode")
        .join(format!("ofeliaengine-{EXTENSION_VERSION}.vsix"));
    std::fs::write(&vsix, extension_vsix()).map_err(|e| format!("{}: {e}", vsix.display()))?;
    if args.install_extension {
        match install_extension(&vsix) {
            Ok(()) => {
                println!("VS Code has the ofeliaengine extension: F5 there attaches to the editor")
            }
            Err(why) => println!(
                "the VS Code extension is in {} ({why}): install it with \
                 `code --install-extension` or VS Code's Install from VSIX",
                vsix.display()
            ),
        }
    }
    println!(
        "a game `{name}` in {}, on the engine in {}",
        dir.display(),
        toml_path(&engine)
    );
    println!(
        "next:\n  cd {}\n  cargo run                      play it\n  \
         cargo run --features editor    edit it; a code change rebuilds and restarts it \
         (`ofeliaengine edit`)\n\
         the engine builds once for every game on this machine (in {}): the first \
         game's first build takes a couple of minutes, every other game's seconds",
        dir.display(),
        shared_build().map_or_else(
            || String::from("your cargo folder"),
            |p| p.display().to_string()
        )
    );
    Ok(ExitCode::SUCCESS)
}

/// The extension as VS Code installs it: a `.vsix`, a zip of its manifest, its content
/// types and its files under `extension/` (what `vsce package` makes, uncompressed).
fn extension_vsix() -> Vec<u8> {
    let manifest = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011" xmlns:d="http://schemas.microsoft.com/developer/vsx-schema-design/2011">
  <Metadata>
    <Identity Language="en-US" Id="ofeliaengine" Version="{EXTENSION_VERSION}" Publisher="ofeliaengine"/>
    <DisplayName>ofeliaengine</DisplayName>
    <Description xml:space="preserve">Debug an ofeliaengine game's Luau scripts: attach to the editor's Play or to a player started with --debug.</Description>
    <Categories>Debuggers</Categories>
    <Properties>
      <Property Id="Microsoft.VisualStudio.Code.Engine" Value="^1.80.0"/>
      <Property Id="Microsoft.VisualStudio.Code.ExtensionKind" Value="workspace"/>
    </Properties>
  </Metadata>
  <Installation>
    <InstallationTarget Id="Microsoft.VisualStudio.Code"/>
  </Installation>
  <Dependencies/>
  <Assets>
    <Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true"/>
    <Asset Type="Microsoft.VisualStudio.Services.Content.Details" Path="extension/README.md" Addressable="true"/>
  </Assets>
</PackageManifest>
"#
    );
    let types = r#"<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension=".json" ContentType="application/json"/>
  <Default Extension=".js" ContentType="application/javascript"/>
  <Default Extension=".md" ContentType="text/markdown"/>
  <Default Extension=".vsixmanifest" ContentType="text/xml"/>
</Types>
"#;
    let mut files: Vec<(String, &[u8])> = vec![
        (String::from("[Content_Types].xml"), types.as_bytes()),
        (String::from("extension.vsixmanifest"), manifest.as_bytes()),
    ];
    for (name, text) in EXTENSION {
        files.push((format!("extension/{name}"), text.as_bytes()));
    }
    zip(&files)
}

/// `files` as a zip, stored (no compression): what a `.vsix` of a few kilobytes needs.
fn zip(files: &[(String, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    // 1980-01-01, zip's first day: no clock in the package.
    let (time, date) = (0u16, 0x21u16);
    for (name, data) in files {
        let crc = crc32(data);
        let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
        let name_len = u16::try_from(name.len()).unwrap_or(u16::MAX);
        let offset = u32::try_from(out.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        for v in [20u16, 0, 0, time, date] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, size, size] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        for v in [20u16, 20, 0, 0, time, date] {
            central.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, size, size] {
            central.extend_from_slice(&v.to_le_bytes());
        }
        for v in [name_len, 0, 0, 0, 0] {
            central.extend_from_slice(&v.to_le_bytes());
        }
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let at = u32::try_from(out.len()).unwrap_or(u32::MAX);
    let central_len = u32::try_from(central.len()).unwrap_or(u32::MAX);
    let count = u16::try_from(files.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    for v in [0u16, 0, count, count] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&central_len.to_le_bytes());
    out.extend_from_slice(&at.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// The CRC-32 zip checks its files with (IEEE 802.3, reflected).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// `vsix` installed in VS Code by its `code` command, where it is on the path.
fn install_extension(vsix: &Path) -> Result<(), String> {
    let out = std::process::Command::new("cmd")
        .args(["/C", "code", "--install-extension"])
        .arg(vsix)
        .arg("--force")
        .output()
        .map_err(|e| format!("VS Code's `code` was not run: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from("VS Code's `code` is not on the path"))
    }
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
# `cargo run --features editor` (or `ofeliaengine edit`): `engine_app::main!` runs the
# editor around the game, which rebuilds it when its code changes.
editor = ["dep:engine_editor"]

# A workspace of its own, even inside another folder that has one.
[workspace]

# The engine's build settings: your code builds fast, the engine runs fast. Playing
# (`cargo run`) and the editor build with this profile, so the engine is built once: file
# and line for your code in panics and the debugger, its overflow checks and debug
# assertions; the engine as in a release build. A change rebuilds in about 1.5 s.
[profile.dev]
opt-level = 1
debug = "line-tables-only"

[profile.dev.package."*"]
opt-level = 3
debug = false
overflow-checks = false
codegen-units = 16

# The Vulkan validation layer is for the engine's own developers and slows every frame's
# CPU side: off in your builds (`ENGINE_VALIDATION=1` turns it on, `sync` with
# synchronization validation). The rest of the engine keeps what a development build has:
# sounds from the live folder, saves as text, the allocation counters.
[profile.dev.package.engine_rhi]
debug-assertions = false

[profile.release]
opt-level = 3
debug = false
incremental = false

# What players get: `cargo build --profile shipping`. It unwinds as every other profile
# (ADR 0118): Luau's errors are C++ exceptions, and under `abort` one raised through the
# engine's callbacks would end the game where it stops one script instance.
[profile.shipping]
inherits = "release"
lto = "fat"
codegen-units = 1
"#,
        engine = toml_path(engine),
    )
}

/// Where every game's build of the engine is, as `.cargo/config.toml` writes it.
const SHARED_BUILD: &str = "{cargo-cache-home}/ofeliaengine/build";

/// [`SHARED_BUILD`] on this machine: cargo's home is `CARGO_HOME`, else `.cargo` in the
/// user's folder.
fn shared_build() -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|u| PathBuf::from(u).join(".cargo")))?;
    Some(home.join("ofeliaengine").join("build"))
}

/// The engine's linker and instruction set (CLAUDE.md §5: the Windows 11 24H2 minimum),
/// and the build of the engine every game shares (ADR 0111).
fn config(name: &str) -> String {
    format!(
        r#"[target.x86_64-pc-windows-msvc]
# `rust-lld` ships with rustup; nothing else to install.
linker = "rust-lld.exe"
# The instruction set every Windows 11 24H2 machine has.
rustflags = ["-C", "target-cpu=x86-64-v2"]

[build]
# The engine's build, shared by every game on this machine, in your cargo folder
# (`%USERPROFILE%\.cargo` unless CARGO_HOME says otherwise): the engine compiles once, and a
# new game builds only its own code. The game's program still lands in its `target`.
# `cargo clean -p {name}` cleans this game's files; a bare `cargo clean` removes the
# engine's build too, which the next build makes again.
build-dir = "{SHARED_BUILD}"
"#
    )
}

const SOUNDS: &str = "# Sounds\n\nYour game's sound files (WAV, Ogg Vorbis, Opus, FLAC), by their paths from here:\n`AudioSource::new(\"music/theme.ogg\")`. While you develop the game plays a changed file at\nonce; for a release, `ofeliaengine sounds build sounds -o sounds.pack`.\n";

fn readme(name: &str, engine: &Path) -> String {
    let docs = toml_path(&engine.join("docs/dev-guide"));
    format!(
        "# {name}\n\n\
         A game on ofeliaengine, started from its quick start.\n\n\
         ```text\n\
         cargo run                     # play it\n\
         cargo run --features editor   # edit it (`ofeliaengine edit`): a code change rebuilds and restarts it\n\
         ```\n\n\
         A red cube turns on a floor; fly around it with WASD and the mouse.\n\
         `src/main.rs` is the game's code, `scenes/main.scene` its scene (what the editor\n\
         edits) and `materials/` the scene's materials.\n\n\
         The engine builds once for every game on this machine, in your cargo folder\n\
         (`.cargo/config.toml`), so a new game builds only its own code. `cargo clean -p {name}`\n\
         cleans this game's files; a bare `cargo clean` removes the engine's build too.\n\n\
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
    fn the_extension_is_a_zip_vs_code_reads() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926, "zip's CRC-32");
        let vsix = extension_vsix();
        // The end of the central directory names the entries and where they are.
        let end = vsix.len() - 22;
        assert_eq!(&vsix[end..end + 4], &0x0605_4b50u32.to_le_bytes());
        let count = u16::from_le_bytes([vsix[end + 10], vsix[end + 11]]);
        assert_eq!(usize::from(count), 2 + EXTENSION.len());
        let text = String::from_utf8_lossy(&vsix);
        for name in [
            "[Content_Types].xml",
            "extension.vsixmanifest",
            "extension/package.json",
            "extension/extension.js",
        ] {
            assert!(text.contains(name), "{name}");
        }
        // The manifest's version is the package's.
        let package: serde_json::Value = serde_json::from_str(EXTENSION[0].1).unwrap();
        assert_eq!(package["version"], EXTENSION_VERSION);
        assert_eq!(
            package["contributes"]["debuggers"][0]["type"],
            "ofeliaengine"
        );
    }

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
            install_extension: false,
        };
        run_new(&args).unwrap();
        let read = |path: &str| std::fs::read_to_string(game.join(path)).unwrap();
        let manifest = read("Cargo.toml");
        assert!(manifest.contains("name = \"my_game\""));
        assert!(manifest.contains("/crates/engine_app\" }"));
        assert!(manifest.contains("/crates/engine_editor\", optional = true }"));
        // What `ofeliaengine edit` looks for.
        assert!(crate::edit::has_editor_feature(&manifest));
        // The engine as in a release build, playing and editing on one build of it, the
        // validation layer off.
        for line in [
            "overflow-checks = false",
            "codegen-units = 16",
            "[profile.dev.package.engine_rhi]",
            "debug-assertions = false",
        ] {
            assert!(manifest.lines().any(|l| l == line), "{line}");
        }
        // A shipped game unwinds (ADR 0118): a script's error stops its instance.
        assert!(manifest.contains("[profile.shipping]") && !manifest.contains("panic = \"abort\""));
        assert!(!manifest.contains('\\'));
        let main = read("src/main.rs");
        assert!(
            main.contains("engine_app::main!(settings, start);") && !main.contains("pub(crate)")
        );
        assert!(read(".gitignore").lines().any(|l| l == "/.ofelia/"));
        assert!(read(".gitignore").lines().any(|l| l == "/builds/"));
        let (build, _) = engine_build::BuildFile::parse(&read("game.build")).unwrap();
        assert_eq!(build.start, "scenes/main.scene");
        assert!(build.always.is_empty(), "{:?}", build.always);
        assert!(read(".gitignore").lines().any(|l| l == "/types/"));
        // luau-lsp in VS Code sees the engine's types as the editor writes them.
        let settings = read(".vscode/settings.json");
        assert!(settings.contains("\"@ofelia\": \"types/ofelia.d.luau\""));
        assert!(settings.contains("\"luau-lsp.platform.type\": \"standard\""));
        // The script debugger in VS Code: the attach configurations, the extension
        // recommended and its package beside them.
        assert!(read(".vscode/launch.json").contains("\"type\": \"ofeliaengine\""));
        assert!(read(".vscode/extensions.json").contains("\"ofeliaengine.ofeliaengine\""));
        assert!(read(".vscode/launch.json").contains("\"target\": \"player\""));
        let vsix = std::fs::read(game.join(".vscode/ofeliaengine-0.2.0.vsix")).unwrap();
        assert_eq!(&vsix[..4], b"PK\x03\x04");
        // The engine's build, shared, and its dependencies' versions.
        let config = read(".cargo/config.toml");
        assert!(
            config
                .lines()
                .any(|l| l == "build-dir = \"{cargo-cache-home}/ofeliaengine/build\"")
        );
        assert!(config.contains("`cargo clean -p my_game`"));
        let engine_lock = std::fs::read_to_string(this_engine().join("Cargo.lock")).unwrap();
        assert_eq!(read("Cargo.lock"), engine_lock);
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
            install_extension: false,
        })
        .unwrap();
        let other_scene = std::fs::read_to_string(other.join("scenes/main.scene")).unwrap();
        assert_ne!(header_id(&other_scene), header_id(&scene));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
