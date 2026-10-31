//! `ofeliaengine build [DIR]`: the game at DIR built into its folder (ADR 0126), as the
//! editor's Build does (`engine_build`). Its build file (`game.build`) is made the first
//! time, from the game's package name and its `scenes/main.scene` (else its first scene).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use engine_build::{BUILD_FILE, BuildFile, Kind, Options, Step};

/// What `ofeliaengine build` was asked.
pub(crate) struct BuildArgs {
    /// The game's folder.
    pub(crate) dir: PathBuf,
    /// A Build or a Ship in place of the file's.
    pub(crate) kind: Option<Kind>,
    /// Where builds go in place of the file's.
    pub(crate) out: Option<PathBuf>,
    /// The outcome as one line of JSON on stdout (for tools and the Claude Code layer).
    pub(crate) json: bool,
}

pub(crate) fn parse_build(args: impl Iterator<Item = String>) -> Result<BuildArgs, String> {
    let mut parsed = BuildArgs {
        dir: PathBuf::from("."),
        kind: None,
        out: None,
        json: false,
    };
    let mut dir = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ship" => parsed.kind = Some(Kind::Ship),
            "--build" => parsed.kind = Some(Kind::Build),
            "--json" => parsed.json = true,
            "--out" => {
                parsed.out = Some(
                    args.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| String::from("--out: a folder"))?,
                );
            }
            other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
            path if dir.is_none() => dir = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected `{extra}`")),
        }
    }
    if let Some(dir) = dir {
        parsed.dir = dir;
    }
    Ok(parsed)
}

/// The game's build file, made the first time (said on stderr).
fn build_file(dir: &Path) -> Result<BuildFile, String> {
    if dir.join(BUILD_FILE).is_file() {
        return BuildFile::load(dir).map_err(|e| e.to_string());
    }
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).map_err(|e| {
        format!(
            "{}: {e} (not a game's folder?)",
            dir.join("Cargo.toml").display()
        )
    })?;
    let name = manifest
        .lines()
        .find_map(|l| l.trim().strip_prefix("name").map(str::trim))
        .and_then(|l| l.strip_prefix('='))
        .map(|l| l.trim().trim_matches('"').to_owned())
        .unwrap_or_else(|| String::from("game"));
    let start = if dir.join("scenes/main.scene").is_file() {
        String::from("scenes/main.scene")
    } else {
        first_scene(dir, dir).unwrap_or_default()
    };
    let file = BuildFile::for_game(dir, &name, &start);
    let path = dir.join(BUILD_FILE);
    std::fs::write(&path, file.to_text(engine_scene::AssetId::new()))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!("made {BUILD_FILE}: {name}, starting in {start}");
    Ok(file)
}

/// The first `.scene` under `at`, by its path from `root`, in the folders' sorted order.
fn first_scene(root: &Path, at: &Path) -> Option<String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(at)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.') || n == "target" || n == "builds")
        })
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if let Some(found) = first_scene(root, &path) {
                return Some(found);
            }
        } else if path.extension().is_some_and(|e| e == "scene") {
            return path
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"));
        }
    }
    None
}

/// `text` as a JSON string.
fn json(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(crate) fn run_build(args: &BuildArgs) -> Result<ExitCode, String> {
    let file = build_file(&args.dir)?;
    let options = Options {
        kind: args.kind,
        output: args.out.clone(),
        engine_shaders: Some(PathBuf::from(crate::ENGINE_SHADERS)),
    };
    let quiet = args.json;
    let result = engine_build::build(&args.dir, &file, &options, |step| match step {
        Step::Planned { files } if !quiet => eprintln!("{files} files"),
        Step::Cargo(line) if !quiet => eprintln!("{line}"),
        Step::Materials { types } if !quiet => {
            eprintln!(
                "compiling {types} material type{}",
                if types == 1 { "" } else { "s" }
            );
        }
        _ => {}
    });
    match result {
        Ok(built) => {
            if args.json {
                println!(
                    "{{\"ok\":true,\"folder\":{},\"program\":{},\"kind\":{},\"files\":{},\
                     \"material_types\":{},\"bytes\":{},\"copied\":{},\"removed\":{},\
                     \"compile_seconds\":{:.1},\"seconds\":{:.1}}}",
                    json(&built.folder.to_string_lossy()),
                    json(&built.program.to_string_lossy()),
                    json(built.kind.profile()),
                    built.files,
                    built.material_types,
                    built.bytes,
                    built.copied,
                    built.removed,
                    built.compile_seconds,
                    built.seconds
                );
            } else {
                println!(
                    "built {} ({}): {} files, {:.1} MB ({} copied, {} removed) in {:.1} s, \
                     {:.1} s of it compiling",
                    built.program.display(),
                    built.kind.profile(),
                    built.files,
                    built.bytes as f64 / 1e6,
                    built.copied,
                    built.removed,
                    built.seconds,
                    built.compile_seconds
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            if args.json {
                let missing: Vec<String> = e
                    .missing
                    .iter()
                    .map(|m| format!("{{\"path\":{},\"from\":{}}}", json(&m.path), json(&m.from)))
                    .collect();
                println!(
                    "{{\"ok\":false,\"error\":{},\"missing\":[{}]}}",
                    json(&e.what),
                    missing.join(",")
                );
            } else {
                eprintln!("error: {e}");
            }
            Ok(ExitCode::from(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_build_commands() {
        let parse = |line: &str| parse_build(line.split_whitespace().map(str::to_owned));
        let a = parse("../game --ship --json --out D:/out").unwrap();
        assert_eq!(a.dir, PathBuf::from("../game"));
        assert_eq!(a.kind, Some(Kind::Ship));
        assert_eq!(a.out, Some(PathBuf::from("D:/out")));
        assert!(a.json);
        let b = parse("").unwrap();
        assert_eq!((b.dir, b.kind, b.json), (PathBuf::from("."), None, false));
        assert!(parse("--fast").is_err());
        assert_eq!(json("a \"b\"\\\n"), "\"a \\\"b\\\"\\\\\\n\"");
    }
}
