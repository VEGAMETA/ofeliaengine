//! Builds of the game's code while the editor runs (ADR 0107 §1, phase E1-5c;
//! `docs/research/rebuild-restart.md`).
//!
//! - The game's folder is watched, and after a build every folder its sources came from
//!   (the engine's too, from cargo's dep-info). A change to a source starts `cargo build`
//!   in the background, below normal priority, one build at a time; changes during a
//!   build start the next one.
//! - Cargo's messages are read as JSON: the compiler's errors with their file, line and
//!   column.
//! - A new executable is copied with its PDB into a slot of `.ofelia/run/` (`a`, `b`,
//!   `c`). The editor runs such a copy, never cargo's own file, so the next build can
//!   write that file.

use engine_platform::fs_watch::{DirectoryWatcher, FileEvent, FileEventKind};
use std::collections::HashSet;
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The game's crate, as `engine_app::main!` gives it: where its code is built.
#[derive(Debug, Clone)]
pub struct GameCrate {
    /// The folder of its `Cargo.toml`.
    pub manifest_dir: PathBuf,
}

impl GameCrate {
    /// The crate whose manifest is in `manifest_dir` (`env!("CARGO_MANIFEST_DIR")`).
    pub fn new(manifest_dir: impl Into<PathBuf>) -> Self {
        Self {
            manifest_dir: manifest_dir.into(),
        }
    }

    /// `.ofelia/run/`: the slots the editor runs builds from.
    pub(crate) fn run_dir(&self) -> PathBuf {
        self.manifest_dir.join(crate::session::DIR).join("run")
    }
}

/// The slots of `.ofelia/run/`, each an executable and its PDB.
pub(crate) const SLOTS: [&str; 3] = ["a", "b", "c"];

/// Whether `exe` is a slot's copy of the game's build (`.ofelia/run/<slot>/`): what
/// builds never write over.
pub(crate) fn in_slot(game: &GameCrate, exe: &Path) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    key(&canonical(exe)).starts_with(&key(&canonical(&game.run_dir())))
}

/// Set for an editor started again by a restart: when, in nanoseconds since 1970. It waits
/// for the last one's lock and says how long the restart took.
pub(crate) const RESTART_VAR: &str = "ENGINE_EDITOR_RESTART";

/// Cargo's own path of the build the editor runs a copy of: its dep-info names the
/// sources to watch. `ofeliaengine edit` and restarts set it.
pub const BUILD_VAR: &str = "ENGINE_EDITOR_BUILD";

/// How long changes settle before a build starts (an editor may write a file twice).
const SETTLE: Duration = Duration::from_millis(100);

/// One of the compiler's errors.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Problem {
    /// Its file (absolute), line and column; none for an error of no file (a link error).
    pub(crate) at: Option<(PathBuf, u32, u32)>,
    pub(crate) message: String,
}

impl Problem {
    /// `src/main.rs:12:5` (relative to the game's folder when it is in it), as shown.
    pub(crate) fn location(&self, game: &Path) -> String {
        match &self.at {
            Some((path, line, column)) => {
                let shown = path.strip_prefix(game).unwrap_or(path);
                format!(
                    "{}:{line}:{column}",
                    shown.to_string_lossy().replace('\\', "/")
                )
            }
            None => String::from("the build"),
        }
    }
}

/// A build that finished.
#[derive(Debug, Default)]
pub(crate) struct Built {
    pub(crate) success: bool,
    pub(crate) took: Duration,
    pub(crate) errors: Vec<Problem>,
    pub(crate) warnings: usize,
    /// The new executable in its slot; none when nothing was rebuilt or the build failed.
    pub(crate) exe: Option<PathBuf>,
    /// Cargo's own path of the executable (for `BUILD_VAR`).
    pub(crate) artifact: Option<PathBuf>,
    /// Why it failed when it was not the compiler's errors (cargo, the copy).
    pub(crate) failure: Option<String>,
}

/// What the builder tells the editor.
#[derive(Debug)]
pub(crate) enum BuildEvent {
    Started,
    Finished(Built),
}

enum Message {
    File(FileEvent),
    Build,
    Quit,
}

/// The builder: a thread that watches the game's sources and builds them.
pub(crate) struct Builds {
    messages: mpsc::Sender<Message>,
    events: mpsc::Receiver<BuildEvent>,
}

impl Builds {
    /// Watches the game's folder and builds it when a source changes. `running` is the
    /// editor's own executable (a slot's copy, never written over); `artifact`, cargo's
    /// path of it when known (its dep-info names every source, the engine's too).
    ///
    /// # Errors
    ///
    /// When the game's folder cannot be watched or the thread cannot start.
    pub(crate) fn start(
        game: &GameCrate,
        running: PathBuf,
        artifact: Option<PathBuf>,
    ) -> Result<Self, String> {
        let (messages, inbox) = mpsc::channel();
        let (report, events) = mpsc::channel();
        let mut builder = Builder {
            game: game.manifest_dir.clone(),
            run_dir: game.run_dir(),
            running,
            sources: Sources::new(&game.manifest_dir),
            watchers: Vec::new(),
            watched: Vec::new(),
            sink: messages.clone(),
        };
        builder.watch(&game.manifest_dir)?;
        if let Some(artifact) = &artifact {
            builder.learn(artifact);
        }
        std::thread::Builder::new()
            .name(String::from("editor builds"))
            .spawn(move || builder.run(&inbox, &report))
            .map_err(|e| format!("the editor's builds: {e}"))?;
        Ok(Self { messages, events })
    }

    /// Builds now, as a change would.
    pub(crate) fn build_now(&self) {
        let _ = self.messages.send(Message::Build);
    }

    /// What happened since the last call.
    pub(crate) fn events(&self) -> impl Iterator<Item = BuildEvent> + '_ {
        self.events.try_iter()
    }
}

impl Drop for Builds {
    fn drop(&mut self) {
        // A build in progress finishes on its own; the thread then ends.
        let _ = self.messages.send(Message::Quit);
    }
}

/// What a build's inputs are: the files cargo's dep-info names, and before the first
/// build the game's Rust files and manifests.
struct Sources {
    game: String,
    /// Normalized paths (see [`key`]).
    files: HashSet<String>,
    /// Cargo's output folder: its writes are never inputs.
    target: Option<String>,
}

/// A path as compared here: lower case, backslashes, no `\\?\` prefix.
fn key(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.replace('/', "\\").to_lowercase()
}

impl Sources {
    fn new(game: &Path) -> Self {
        let game = std::fs::canonicalize(game).unwrap_or_else(|_| game.to_path_buf());
        Self {
            game: key(&game),
            files: HashSet::new(),
            target: None,
        }
    }

    /// Whether a change can change the build.
    fn changed_by(&self, event: &FileEvent) -> bool {
        if event.kind == FileEventKind::Rescan {
            // Notifications were lost: cargo tells whether anything changed.
            return true;
        }
        let k = key(&event.path);
        if self
            .target
            .as_ref()
            .is_some_and(|t| k.starts_with(t.as_str()))
        {
            return false;
        }
        if self.files.contains(&k) {
            return true;
        }
        let Some(inside) = k.strip_prefix(self.game.as_str()) else {
            return false;
        };
        let inside = inside.trim_start_matches('\\');
        if inside.starts_with("target\\") || inside.starts_with(".ofelia\\") {
            return false;
        }
        // A new source the dep-info does not know yet, and the build's own settings.
        inside.ends_with(".rs")
            || matches!(
                inside,
                "cargo.toml" | "cargo.lock" | "build.rs" | "rust-toolchain.toml"
            )
            || inside.starts_with(".cargo\\")
    }
}

/// The files cargo's dep-info (`<exe>.d`, Makefile syntax) names as the build's inputs.
/// Spaces in paths are written `\ `; a backslash before anything else is part of the path.
pub(crate) fn dep_info_files(text: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for line in text.lines() {
        // `target: inputs…`: the target's own colon is the first `: ` (a drive's `C:\` has
        // a backslash after it).
        let Some((_, inputs)) = line.split_once(": ") else {
            continue;
        };
        let mut current = String::new();
        let mut chars = inputs.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&' ') => {
                    current.push(' ');
                    chars.next();
                }
                ' ' => {
                    if !current.is_empty() {
                        files.push(PathBuf::from(std::mem::take(&mut current)));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            files.push(PathBuf::from(current));
        }
    }
    files
}

/// The folder to watch for `files` outside the game's folder: where they all are, or
/// none when that is a drive's root (sources spread over a drive are not watched).
/// Cargo's and rustup's own folders (registry sources, the standard library) never change.
fn outside_root(files: &[PathBuf], game: &str) -> Option<PathBuf> {
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let homes: Vec<String> = [
        std::env::var_os("CARGO_HOME").map(PathBuf::from),
        std::env::var_os("RUSTUP_HOME").map(PathBuf::from),
        profile.as_ref().map(|p| p.join(".cargo")),
        profile.as_ref().map(|p| p.join(".rustup")),
    ]
    .into_iter()
    .flatten()
    .map(|p| key(&p))
    .collect();
    let outside: Vec<&PathBuf> = files
        .iter()
        .filter(|f| {
            let k = key(f);
            !k.starts_with(game) && !homes.iter().any(|h| k.starts_with(h.as_str()))
        })
        .collect();
    let first = outside.first()?;
    let mut common: Vec<_> = first.parent()?.components().collect();
    for f in &outside[1..] {
        let parts: Vec<_> = f.components().collect();
        let same = common
            .iter()
            .zip(&parts)
            .take_while(|(a, b)| {
                a.as_os_str().to_string_lossy().to_lowercase()
                    == b.as_os_str().to_string_lossy().to_lowercase()
            })
            .count();
        common.truncate(same);
    }
    // A prefix and the root itself (`H:`, `\`) are not a folder to watch.
    (common.len() > 2).then(|| common.iter().collect())
}

/// What one line of `cargo build --message-format=json` says.
#[derive(Debug, PartialEq)]
pub(crate) enum CargoLine {
    Error(Problem),
    Warning,
    /// The game's executable: cargo's path, every file it wrote, and whether it was up
    /// to date already.
    Executable {
        exe: PathBuf,
        files: Vec<PathBuf>,
        fresh: bool,
    },
    Finished(bool),
    Other,
}

/// Reads one line of cargo's JSON messages. `game` is the game's folder (relative file
/// names start there) and `manifest` its `Cargo.toml`.
pub(crate) fn parse_cargo_line(line: &str, game: &Path, manifest: &Path) -> CargoLine {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return CargoLine::Other;
    };
    match v["reason"].as_str() {
        Some("compiler-message") => {
            let m = &v["message"];
            match m["level"].as_str() {
                Some("error") => {
                    let message = m["message"].as_str().unwrap_or("").to_owned();
                    if message.starts_with("aborting due to") {
                        return CargoLine::Other;
                    }
                    let spans = m["spans"].as_array();
                    let primary = spans
                        .and_then(|s| s.iter().find(|s| s["is_primary"].as_bool() == Some(true)));
                    let at = primary.and_then(|s| {
                        let file = PathBuf::from(s["file_name"].as_str()?);
                        let file = if file.is_absolute() {
                            file
                        } else {
                            game.join(file)
                        };
                        Some((
                            file,
                            u32::try_from(s["line_start"].as_u64()?).ok()?,
                            u32::try_from(s["column_start"].as_u64()?).ok()?,
                        ))
                    });
                    CargoLine::Error(Problem { at, message })
                }
                Some("warning") => CargoLine::Warning,
                _ => CargoLine::Other,
            }
        }
        Some("compiler-artifact") => {
            let ours = v["manifest_path"]
                .as_str()
                .is_some_and(|m| key(Path::new(m)) == key(manifest));
            let bin = v["target"]["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|k| k.as_str() == Some("bin")));
            match v["executable"].as_str() {
                Some(exe) if ours && bin => CargoLine::Executable {
                    exe: PathBuf::from(exe),
                    files: v["filenames"]
                        .as_array()
                        .map(|f| {
                            f.iter()
                                .filter_map(|f| f.as_str().map(PathBuf::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    fresh: v["fresh"].as_bool().unwrap_or(false),
                },
                _ => CargoLine::Other,
            }
        }
        Some("build-finished") => CargoLine::Finished(v["success"].as_bool().unwrap_or(false)),
        _ => CargoLine::Other,
    }
}

/// Copies an executable and the other files cargo wrote for it (its PDB) into a slot of
/// `run_dir` that the running editor is not in, passing over one whose files are held (a
/// debugger keeps a PDB open). Returns the copy of `exe`.
pub(crate) fn copy_to_slot(
    run_dir: &Path,
    exe: &Path,
    files: &[PathBuf],
    running: &Path,
) -> Result<PathBuf, String> {
    let files: Vec<&PathBuf> = files.iter().filter(|f| f.is_file()).collect();
    let running = key(running);
    let mut last = String::from("no slot");
    for slot in SLOTS {
        let dir = run_dir.join(slot);
        if running.starts_with(&key(&dir)) {
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            last = format!("{}: {e}", dir.display());
            continue;
        }
        let copied = files
            .iter()
            .copied()
            .filter(|f| {
                f.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("pdb"))
            })
            .try_for_each(|f| {
                let to = dir.join(f.file_name().unwrap_or_default());
                std::fs::copy(f, &to)
                    .map(drop)
                    .map_err(|e| format!("{}: {e}", to.display()))
            });
        match copied {
            Ok(()) => return Ok(dir.join(exe.file_name().unwrap_or_default())),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The command a build runs: the game's `editor` feature, the dev profile, messages as
/// JSON, below normal priority, no console window of its own.
pub(crate) fn cargo_build(game: &Path) -> Command {
    let mut command = Command::new("cargo");
    command
        .args([
            "build",
            "--quiet",
            "--features",
            "editor",
            "--message-format=json",
        ])
        .current_dir(game)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // BELOW_NORMAL_PRIORITY_CLASS (rustc and the linker inherit it) | CREATE_NO_WINDOW.
        command.creation_flags(0x0000_4000 | 0x0800_0000);
    }
    command
}

struct Builder {
    game: PathBuf,
    run_dir: PathBuf,
    running: PathBuf,
    sources: Sources,
    watchers: Vec<DirectoryWatcher>,
    watched: Vec<String>,
    sink: mpsc::Sender<Message>,
}

impl Builder {
    /// Watches `folder` and everything under it, once.
    fn watch(&mut self, folder: &Path) -> Result<(), String> {
        let k = key(folder);
        if self.watched.iter().any(|w| k.starts_with(w.as_str())) {
            return Ok(());
        }
        let sink = self.sink.clone();
        let watcher =
            DirectoryWatcher::with_sink(folder, true, move |e| sink.send(Message::File(e)).is_ok())
                .map_err(|e| format!("{}: {e}", folder.display()))?;
        self.watchers.push(watcher);
        self.watched.push(k);
        Ok(())
    }

    /// Learns a build's inputs from cargo's dep-info beside `exe`, and watches the folder
    /// of those outside the game's folder (the engine's sources).
    fn learn(&mut self, exe: &Path) {
        let Ok(text) = std::fs::read_to_string(exe.with_extension("d")) else {
            return;
        };
        let files = dep_info_files(&text);
        self.sources.files = files.iter().map(|f| key(f)).collect();
        // `<target>/<profile>/<exe>`: the target folder's writes are never inputs.
        self.sources.target = exe.parent().and_then(Path::parent).map(key);
        if let Some(root) = outside_root(&files, &self.sources.game)
            && let Err(e) = self.watch(&root)
        {
            log::warn!(target: "engine_editor", "not watched: {e}");
        }
    }

    fn run(mut self, inbox: &mpsc::Receiver<Message>, report: &mpsc::Sender<BuildEvent>) {
        let mut pending = false;
        loop {
            let message = if pending {
                match inbox.recv_timeout(SETTLE) {
                    Ok(m) => m,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        pending = false;
                        if report.send(BuildEvent::Started).is_err() {
                            return;
                        }
                        let built = self.build();
                        if report.send(BuildEvent::Finished(built)).is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match inbox.recv() {
                    Ok(m) => m,
                    Err(_) => return,
                }
            };
            match message {
                Message::Quit => return,
                Message::Build => pending = true,
                Message::File(e) => pending |= self.sources.changed_by(&e),
            }
        }
    }

    /// Runs one build: cargo's JSON read as it comes, the executable copied into a slot.
    fn build(&mut self) -> Built {
        let started = Instant::now();
        let mut built = Built::default();
        let mut child = match cargo_build(&self.game).spawn() {
            Ok(child) => child,
            Err(e) => {
                built.failure = Some(format!("cargo: {e}"));
                return built;
            }
        };
        // Cargo's own errors (a manifest it cannot read), read beside its messages.
        let stderr = child.stderr.take().map(|mut s| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = s.read_to_string(&mut text);
                text
            })
        });
        let manifest = self.game.join("Cargo.toml");
        let mut executable = None;
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                match parse_cargo_line(&line, &self.game, &manifest) {
                    CargoLine::Error(p) => built.errors.push(p),
                    CargoLine::Warning => built.warnings += 1,
                    CargoLine::Executable { exe, files, fresh } => {
                        executable = Some((exe, files, fresh));
                    }
                    CargoLine::Finished(success) => built.success = success,
                    CargoLine::Other => {}
                }
            }
        }
        let status = child.wait();
        let cargo_errors = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
        built.success &= status.is_ok_and(|s| s.success());
        built.took = started.elapsed();
        if !built.success {
            if built.errors.is_empty() {
                let text = cargo_errors.trim();
                built.failure = Some(if text.is_empty() {
                    String::from("cargo failed")
                } else {
                    text.lines().next().unwrap_or(text).to_owned()
                });
            }
            return built;
        }
        if let Some((exe, files, fresh)) = executable {
            self.learn(&exe);
            if !fresh {
                match copy_to_slot(&self.run_dir, &exe, &files, &self.running) {
                    Ok(copy) => built.exe = Some(copy),
                    Err(e) => {
                        built.success = false;
                        built.failure = Some(format!("not copied: {e}"));
                    }
                }
            }
            built.artifact = Some(exe);
        }
        built
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn dep_info_names_every_input_with_spaces_kept() {
        let text = "C:\\games\\my game\\target\\debug\\game.exe: C:\\games\\my\\ game\\src\\main.rs H:\\engine\\crates\\engine_app\\src\\lib.rs\n\nC:\\games\\my\\ game\\src\\main.rs:\n";
        assert_eq!(
            dep_info_files(text),
            [
                PathBuf::from("C:\\games\\my game\\src\\main.rs"),
                PathBuf::from("H:\\engine\\crates\\engine_app\\src\\lib.rs"),
            ]
        );
    }

    #[test]
    fn the_engine_folder_is_watched_beside_the_game() {
        let game = key(Path::new("C:\\games\\mygame"));
        let files = [
            PathBuf::from("C:\\games\\mygame\\src\\main.rs"),
            PathBuf::from("H:\\Desktop\\ofeliaengine\\crates\\engine_app\\src\\lib.rs"),
            PathBuf::from("H:\\Desktop\\ofeliaengine\\crates\\engine_editor\\src\\app.rs"),
        ];
        assert_eq!(
            outside_root(&files, &game),
            Some(PathBuf::from("H:\\Desktop\\ofeliaengine\\crates"))
        );
        // Spread over a whole drive: not watched.
        let spread = [
            PathBuf::from("H:\\one\\lib.rs"),
            PathBuf::from("H:\\two\\lib.rs"),
        ];
        assert_eq!(outside_root(&spread, &game), None);
        assert_eq!(outside_root(&files[..1], &game), None);
    }

    #[test]
    fn changes_to_sources_build_and_cargos_own_writes_do_not() {
        let mut sources = Sources {
            game: key(Path::new("C:\\games\\mygame")),
            files: HashSet::new(),
            target: Some(key(Path::new("C:\\cargo-target\\mygame"))),
        };
        sources.files.insert(key(Path::new(
            "H:\\engine\\crates\\engine_app\\src\\lib.rs",
        )));
        let at = |p: &str| FileEvent {
            path: PathBuf::from(p),
            kind: FileEventKind::Modified,
        };
        assert!(sources.changed_by(&at("C:\\games\\mygame\\src\\main.rs")));
        assert!(sources.changed_by(&at("C:\\games\\mygame\\src\\enemy\\ai.rs")));
        assert!(sources.changed_by(&at("C:\\Games\\MyGame\\Cargo.toml")));
        assert!(sources.changed_by(&at("H:\\engine\\crates\\engine_app\\src\\lib.rs")));
        assert!(!sources.changed_by(&at("H:\\engine\\crates\\engine_app\\README.md")));
        assert!(!sources.changed_by(&at("C:\\games\\mygame\\target\\debug\\game.exe")));
        assert!(!sources.changed_by(&at("C:\\cargo-target\\mygame\\debug\\deps\\x.rs")));
        assert!(!sources.changed_by(&at("C:\\games\\mygame\\.ofelia\\session")));
        assert!(!sources.changed_by(&at("C:\\games\\mygame\\scenes\\main.scene")));
    }

    #[test]
    fn cargo_messages_give_errors_by_line_and_the_executable() {
        let game = Path::new("C:\\games\\mygame");
        let manifest = game.join("Cargo.toml");
        let error = r#"{"reason":"compiler-message","manifest_path":"C:\\games\\mygame\\Cargo.toml","message":{"level":"error","message":"mismatched types","spans":[{"file_name":"src/other.rs","line_start":3,"column_start":1,"is_primary":false},{"file_name":"src\\main.rs","line_start":12,"column_start":5,"is_primary":true}],"rendered":"error[E0308]: mismatched types"}}"#;
        let CargoLine::Error(problem) = parse_cargo_line(error, game, &manifest) else {
            panic!("an error");
        };
        assert_eq!(problem.message, "mismatched types");
        assert_eq!(problem.location(game), "src/main.rs:12:5");
        let aborting = r#"{"reason":"compiler-message","message":{"level":"error","message":"aborting due to 1 previous error","spans":[]}}"#;
        assert_eq!(
            parse_cargo_line(aborting, game, &manifest),
            CargoLine::Other
        );
        let link = r#"{"reason":"compiler-message","message":{"level":"error","message":"linking with `rust-lld.exe` failed","spans":[]}}"#;
        let CargoLine::Error(link) = parse_cargo_line(link, game, &manifest) else {
            panic!("a link error");
        };
        assert_eq!(link.location(game), "the build");
        let warning = r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused","spans":[]}}"#;
        assert_eq!(
            parse_cargo_line(warning, game, &manifest),
            CargoLine::Warning
        );
        let artifact = r#"{"reason":"compiler-artifact","manifest_path":"C:\\games\\mygame\\Cargo.toml","target":{"kind":["bin"],"name":"mygame"},"filenames":["C:\\t\\debug\\mygame.exe","C:\\t\\debug\\mygame.pdb"],"executable":"C:\\t\\debug\\mygame.exe","fresh":false}"#;
        assert_eq!(
            parse_cargo_line(artifact, game, &manifest),
            CargoLine::Executable {
                exe: PathBuf::from("C:\\t\\debug\\mygame.exe"),
                files: vec![
                    PathBuf::from("C:\\t\\debug\\mygame.exe"),
                    PathBuf::from("C:\\t\\debug\\mygame.pdb"),
                ],
                fresh: false,
            }
        );
        // A dependency's artifact is not the game's.
        let dependency = r#"{"reason":"compiler-artifact","manifest_path":"H:\\engine\\crates\\engine_app\\Cargo.toml","target":{"kind":["lib"]},"filenames":[],"executable":null,"fresh":true}"#;
        assert_eq!(
            parse_cargo_line(dependency, game, &manifest),
            CargoLine::Other
        );
        assert_eq!(
            parse_cargo_line(
                r#"{"reason":"build-finished","success":true}"#,
                game,
                &manifest
            ),
            CargoLine::Finished(true)
        );
        assert_eq!(
            parse_cargo_line("not json", game, &manifest),
            CargoLine::Other
        );
    }

    #[test]
    fn a_build_goes_into_a_slot_the_editor_does_not_run_from() {
        let dir = std::env::temp_dir().join(format!("ofelia-slots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let built = dir.join("target");
        std::fs::create_dir_all(&built).unwrap();
        let exe = built.join("game.exe");
        let pdb = built.join("game.pdb");
        std::fs::write(&exe, "exe").unwrap();
        std::fs::write(&pdb, "pdb").unwrap();
        let run = dir.join(".ofelia/run");
        let files = [exe.clone(), pdb.clone(), built.join("game.d")];
        // Running from cargo's file: the first slot.
        let copy = copy_to_slot(&run, &exe, &files, &exe).unwrap();
        assert_eq!(copy, run.join("a").join("game.exe"));
        assert!(run.join("a").join("game.pdb").is_file());
        assert!(
            !run.join("a").join("game.d").exists(),
            "only the exe and its PDB"
        );
        // Running from slot a: the next build goes into b.
        let copy = copy_to_slot(&run, &exe, &files, &run.join("a").join("game.exe")).unwrap();
        assert_eq!(copy, run.join("b").join("game.exe"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
