//! `ofeliaengine edit [DIR]`: a game in its editor (ADR 0107 §1). The game's `main` comes
//! from `engine_app::main!`, which runs the editor around the game when the game's `editor`
//! feature is on: this builds and runs the game in `DIR` with it.

use std::path::PathBuf;
use std::process::ExitCode;

pub(crate) struct EditArgs {
    pub(crate) dir: PathBuf,
    /// Passed to the game after `--`.
    pub(crate) rest: Vec<String>,
}

pub(crate) fn parse_edit(mut args: impl Iterator<Item = String>) -> Result<EditArgs, String> {
    let mut parsed = EditArgs {
        dir: PathBuf::from("."),
        rest: Vec::new(),
    };
    let mut dir = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => {
                parsed.rest.extend(args.by_ref());
                break;
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

/// Whether the game's manifest declares the `editor` feature.
pub(crate) fn has_editor_feature(manifest: &str) -> bool {
    let mut in_features = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_features = line == "[features]";
            continue;
        }
        if in_features && line.split('=').next().is_some_and(|k| k.trim() == "editor") {
            return true;
        }
    }
    false
}

/// The arguments of the `cargo` that builds and runs the game in its editor.
fn cargo_args(rest: &[String]) -> Vec<String> {
    let mut args: Vec<String> = ["run", "--release", "--features", "editor"]
        .map(String::from)
        .to_vec();
    if !rest.is_empty() {
        args.push(String::from("--"));
        args.extend(rest.iter().cloned());
    }
    args
}

pub(crate) fn run_edit(args: &EditArgs) -> Result<ExitCode, String> {
    let manifest_path = args.dir.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{}: {e} (is it a game's folder?)", manifest_path.display()))?;
    if !has_editor_feature(&manifest) {
        return Err(format!(
            "{} has no `editor` feature: add `engine_editor = {{ ..., optional = true }}` to \
             its [dependencies] and `editor = [\"dep:engine_editor\"]` to its [features] \
             (`ofeliaengine new` writes both)",
            manifest_path.display()
        ));
    }
    let status = std::process::Command::new("cargo")
        .args(cargo_args(&args.rest))
        .current_dir(&args.dir)
        .status()
        .map_err(|e| format!("cargo: {e}"))?;
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        None => ExitCode::FAILURE,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_editor_feature_is_found_in_the_features_table_only() {
        assert!(has_editor_feature(
            "[package]\nname = \"g\"\n\n[features]\neditor = [\"dep:engine_editor\"]\n"
        ));
        assert!(!has_editor_feature(
            "[package]\neditor = 1\n\n[features]\nfast = []\n"
        ));
        assert!(!has_editor_feature("[package]\nname = \"g\"\n"));
    }

    #[test]
    fn edit_runs_the_game_with_its_editor_feature() {
        let args = parse_edit(
            ["games/mine", "--", "scenes/a.scene"]
                .map(String::from)
                .into_iter(),
        )
        .unwrap();
        assert_eq!(args.dir, PathBuf::from("games/mine"));
        assert_eq!(
            cargo_args(&args.rest),
            [
                "run",
                "--release",
                "--features",
                "editor",
                "--",
                "scenes/a.scene"
            ]
        );
        assert_eq!(
            parse_edit(std::iter::empty()).unwrap().dir,
            PathBuf::from(".")
        );
        assert!(parse_edit(["--fast".to_owned()].into_iter()).is_err());
    }
}
