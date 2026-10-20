//! `ofeliaengine edit [DIR]`: a game in its editor (ADR 0107 §1). The game's `main` comes
//! from `engine_app::main!`, which runs the editor around the game when the game's `editor`
//! feature is on. This launches it:
//!
//! 1. builds the game with the feature, as the editor's own builds do (the dev profile),
//!    cargo's messages shown as they come;
//! 2. copies the executable and its PDB into a slot of `.ofelia/run/`: the editor runs that
//!    copy, so its builds can write cargo's own file (E1-5c);
//! 3. starts the copy in a job and waits until the editor, and the editors its restarts
//!    start, have all exited. A failed first build starts the last good one.

use engine_platform::process::Job;
use engine_platform::time::HighResTimer;
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

/// Cargo's path of the build the editor runs a copy of (`engine_editor::BUILD_VAR`).
const BUILD_VAR: &str = "ENGINE_EDITOR_BUILD";

/// The slots of `.ofelia/run/`.
const SLOTS: [&str; 3] = ["a", "b", "c"];

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

/// The arguments of the `cargo` that builds the game for its editor: the editor's own
/// builds' (the dev profile, the `editor` feature), cargo showing its messages itself and
/// writing JSON (the executable's path) to stdout.
fn cargo_args() -> [&'static str; 4] {
    [
        "build",
        "--features",
        "editor",
        "--message-format=json-render-diagnostics",
    ]
}

/// The game's executable in one line of cargo's JSON messages: its path and every file
/// cargo wrote for it.
fn executable(line: &str, manifest: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v["reason"].as_str() != Some("compiler-artifact") {
        return None;
    }
    let same = |a: &str| {
        let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
        norm(Path::new(a)) == norm(manifest)
    };
    if !v["manifest_path"].as_str().is_some_and(same) {
        return None;
    }
    let exe = PathBuf::from(v["executable"].as_str()?);
    let files = v["filenames"]
        .as_array()?
        .iter()
        .filter_map(|f| f.as_str().map(PathBuf::from))
        .collect();
    Some((exe, files))
}

/// Copies the executable and its PDB into the first slot that takes them.
fn copy_to_slot(run: &Path, exe: &Path, files: &[PathBuf]) -> Result<PathBuf, String> {
    let mut last = String::from("no slot");
    for slot in SLOTS {
        let dir = run.join(slot);
        let copied = std::fs::create_dir_all(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))
            .and_then(|()| {
                files
                    .iter()
                    .filter(|f| {
                        f.extension().is_some_and(|e| {
                            e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("pdb")
                        })
                    })
                    .try_for_each(|f| {
                        let to = dir.join(f.file_name().unwrap_or_default());
                        std::fs::copy(f, &to)
                            .map(drop)
                            .map_err(|e| format!("{}: {e}", to.display()))
                    })
            });
        match copied {
            Ok(()) => return Ok(dir.join(exe.file_name().unwrap_or_default())),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The newest executable in the slots: the last good build.
fn last_good(run: &Path) -> Option<PathBuf> {
    SLOTS
        .iter()
        .filter_map(|slot| std::fs::read_dir(run.join(slot)).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
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
    let dir =
        std::fs::canonicalize(&args.dir).map_err(|e| format!("{}: {e}", args.dir.display()))?;
    let dir = PathBuf::from(dir.to_string_lossy().trim_start_matches(r"\\?\"));
    let manifest_path = dir.join("Cargo.toml");
    let run = dir.join(".ofelia").join("run");

    // 1. The build: cargo shows its progress and the compiler's messages on stderr.
    let mut child = std::process::Command::new("cargo")
        .args(cargo_args())
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cargo: {e}"))?;
    let mut built = None;
    if let Some(stdout) = child.stdout.take() {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if let Some(found) = executable(&line, &manifest_path) {
                built = Some(found);
            }
        }
    }
    let success = child.wait().map_err(|e| format!("cargo: {e}"))?.success();

    // 2. Its copy in a slot, or the last good build.
    let (exe, artifact) = match built {
        Some((exe, files)) if success => (copy_to_slot(&run, &exe, &files)?, Some(exe)),
        _ => {
            let previous =
                last_good(&run).ok_or("the build failed, and there is no earlier one")?;
            eprintln!(
                "the build failed: starting the last good one, {}",
                previous.display()
            );
            (previous, None)
        }
    };

    // 3. The editor, and the editors its restarts start, in one job.
    let mut command = std::process::Command::new(&exe);
    command.args(&args.rest).current_dir(&dir);
    if let Some(artifact) = &artifact {
        command.env(BUILD_VAR, artifact);
    }
    let job = Job::new().map_err(|e| format!("a job for the editor: {e}"))?;
    let mut editor = command
        .spawn()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    job.assign(&editor)
        .map_err(|e| format!("a job for the editor: {e}"))?;
    let timer = HighResTimer::new().map_err(|e| e.to_string())?;
    let mut code = None;
    loop {
        if code.is_none()
            && let Ok(Some(status)) = editor.try_wait()
        {
            code = Some(status.code().unwrap_or(1));
        }
        if code.is_some() && job.active_processes().unwrap_or(0) == 0 {
            break;
        }
        let _ = timer.sleep(Duration::from_millis(200));
    }
    Ok(match code {
        Some(0) | None => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
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
    fn edit_builds_the_editor_profile_and_finds_the_games_executable() {
        let args = parse_edit(
            ["games/mine", "--", "scenes/a.scene"]
                .map(String::from)
                .into_iter(),
        )
        .unwrap();
        assert_eq!(args.dir, PathBuf::from("games/mine"));
        assert_eq!(args.rest, ["scenes/a.scene"]);
        assert_eq!(
            parse_edit(std::iter::empty()).unwrap().dir,
            PathBuf::from(".")
        );
        assert!(parse_edit(["--fast".to_owned()].into_iter()).is_err());
        // The dev profile and the editor feature, as the editor's own builds.
        assert_eq!(
            cargo_args(),
            [
                "build",
                "--features",
                "editor",
                "--message-format=json-render-diagnostics"
            ]
        );
        let manifest = Path::new("C:\\games\\mine\\Cargo.toml");
        let line = r#"{"reason":"compiler-artifact","manifest_path":"C:\\games\\mine\\Cargo.toml","target":{"kind":["bin"]},"filenames":["C:\\t\\debug\\mine.exe","C:\\t\\debug\\mine.pdb"],"executable":"C:\\t\\debug\\mine.exe","fresh":true}"#;
        let (exe, files) = executable(line, manifest).unwrap();
        assert_eq!(exe, PathBuf::from("C:\\t\\debug\\mine.exe"));
        assert_eq!(files.len(), 2);
        let other = line.replace("games\\\\mine", "engine\\\\crates\\\\engine_app");
        assert!(executable(&other, manifest).is_none());
        assert!(executable(r#"{"reason":"build-finished","success":true}"#, manifest).is_none());
    }

    #[test]
    fn a_failed_build_starts_the_last_good_one() {
        let dir = std::env::temp_dir().join(format!("ofelia-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let run = dir.join(".ofelia/run");
        assert!(last_good(&run).is_none());
        let built = dir.join("target");
        std::fs::create_dir_all(&built).unwrap();
        let exe = built.join("mine.exe");
        std::fs::write(&exe, "exe").unwrap();
        std::fs::write(built.join("mine.pdb"), "pdb").unwrap();
        let files = [exe.clone(), built.join("mine.pdb")];
        let copy = copy_to_slot(&run, &exe, &files).unwrap();
        assert_eq!(copy, run.join("a").join("mine.exe"));
        assert!(run.join("a").join("mine.pdb").is_file());
        assert_eq!(last_good(&run), Some(copy));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
