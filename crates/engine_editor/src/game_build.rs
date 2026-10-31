//! The editor's Build (ADR 0126's G4): the game built into its folder on a thread of its
//! own, by the same library as `ofeliaengine build` (`engine_build`), never blocking the
//! UI. Its steps show in the Tasks panel; cargo's lines and the build's errors go to the
//! Output panel through the log, where a file and line in a row is a link to its cause; the
//! last build's program can be run.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;

use engine_build::{BuildError, BuildFile, Built, Kind, Options, Step};

/// What the build's thread reports.
pub(crate) enum Event {
    /// The step under way, as the Tasks panel says it.
    Step(String),
    /// A line cargo printed.
    Line(String),
    /// The build done.
    Done(Box<Result<Built, BuildError>>),
}

/// A build under way.
pub(crate) struct GameBuild {
    events: Receiver<Event>,
    /// When it started.
    pub(crate) since: Instant,
    /// Its step, for the Tasks panel.
    pub(crate) step: String,
    /// A Build or a Ship.
    pub(crate) kind: Kind,
}

/// What a frame took from a build.
pub(crate) enum Polled {
    /// Still under way.
    Running,
    /// Done: its outcome.
    Done(Box<Result<Built, BuildError>>),
}

impl GameBuild {
    /// The game at `root` built as `kind` (its build file made the first time, from
    /// `start`, the scene open in the editor), on a thread of its own.
    ///
    /// # Errors
    ///
    /// A build file that cannot be read or made (a folder that is not a game's), or no
    /// thread.
    pub(crate) fn start(
        root: &Path,
        kind: Kind,
        start: Option<&str>,
        shaders: &Path,
    ) -> Result<(Self, bool), String> {
        let (file, made) = BuildFile::open_or_make(root, start).map_err(|e| e.to_string())?;
        let (send, events) = channel();
        let (root, shaders) = (root.to_path_buf(), shaders.to_path_buf());
        std::thread::Builder::new()
            .name(String::from("ofelia-game-build"))
            .spawn(move || {
                let options = Options {
                    kind: Some(kind),
                    output: None,
                    engine_shaders: Some(shaders),
                    zip: false,
                };
                let result = engine_build::build(&root, &file, &options, |step| {
                    let event = match step {
                        Step::Planned { files } => Event::Step(format!("{files} files planned")),
                        Step::Cargo(line) => Event::Line(line.to_owned()),
                        Step::Deriving => {
                            Event::Step(String::from("compressing textures, cooking colliders"))
                        }
                        Step::Materials { types } => {
                            Event::Step(format!("compiling {types} material types"))
                        }
                        Step::Writing { done, of } => Event::Step(format!("writing {done}/{of}")),
                    };
                    let _ = send.send(event);
                });
                let _ = send.send(Event::Done(Box::new(result)));
            })
            .map_err(|e| e.to_string())?;
        Ok((
            Self {
                events,
                since: Instant::now(),
                step: String::from("compiling the game"),
                kind,
            },
            made,
        ))
    }

    /// The build's news since the last frame: its step kept, cargo's lines logged
    /// ([`log_line`]); its outcome once it is done.
    pub(crate) fn poll(&mut self) -> Polled {
        loop {
            match self.events.try_recv() {
                Ok(Event::Step(step)) => self.step = step,
                Ok(Event::Line(line)) => log_line(&line),
                Ok(Event::Done(result)) => return Polled::Done(result),
                Err(TryRecvError::Empty) => return Polled::Running,
                Err(TryRecvError::Disconnected) => {
                    return Polled::Done(Box::new(Err(BuildError::from(String::from(
                        "the build's thread ended without an outcome",
                    )))));
                }
            }
        }
    }
}

/// The level a line of cargo's goes to the Output at: its errors and warnings as such
/// (their `--> file:line:col` lines with them, links to their places), the crates it
/// compiles as detail.
pub(crate) fn line_level(line: &str) -> log::Level {
    let trimmed = line.trim_start();
    if trimmed.starts_with("error") {
        log::Level::Error
    } else if trimmed.starts_with("warning") {
        log::Level::Warn
    } else if [
        "Compiling ",
        "Checking ",
        "Fresh ",
        "Building ",
        "Blocking ",
    ]
    .iter()
    .any(|p| trimmed.starts_with(p))
    {
        log::Level::Debug
    } else {
        log::Level::Info
    }
}

fn log_line(line: &str) {
    log::log!(target: "build", line_level(line), "{line}");
}

/// The outcome logged for the Output (missing files each in a row naming the file that
/// names them, a link); what the status line says.
pub(crate) fn report(result: &Result<Built, BuildError>) -> (String, Option<PathBuf>) {
    match result {
        Ok(built) => {
            let said = format!(
                "Built {} ({}) in {:.1} s: {} files, {:.1} MB",
                built.program.display(),
                built.kind.profile(),
                built.seconds,
                built.files,
                (built.bytes + built.derived.bytes) as f64 / 1e6
            );
            log::info!(target: "build", "{said}");
            log::info!(target: "build", "size report: {}", built.report.display());
            (said, Some(built.program.clone()))
        }
        Err(e) => {
            for missing in &e.missing {
                log::error!(target: "build", "{}: names {}, which is not there", missing.from, missing.path);
            }
            let said = format!("Build failed: {}", e.what);
            log::error!(target: "build", "{said}");
            (said, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_lines_go_to_the_output_at_their_levels() {
        assert_eq!(
            line_level("error[E0425]: cannot find value `x`"),
            log::Level::Error
        );
        assert_eq!(line_level("warning: unused variable"), log::Level::Warn);
        assert_eq!(
            line_level("   Compiling engine_core v0.1.0"),
            log::Level::Debug
        );
        assert_eq!(line_level("  --> src/main.rs:5:13"), log::Level::Info);
    }

    /// A folder that is not a game's: said at once, nothing started.
    #[test]
    fn a_folder_without_a_game_is_said_at_once() {
        let dir = std::env::temp_dir().join(format!("ofelia-editor-build-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let e = GameBuild::start(&dir, Kind::Build, None, Path::new("shaders"))
            .err()
            .unwrap();
        assert!(e.contains("not a game's folder"), "{e}");
        assert!(!dir.join(engine_build::BUILD_FILE).exists());
        // A missing file named by another: a row whose cause is the naming file.
        let mut e = BuildError::from(String::from("1 file named but not found"));
        e.missing.push(engine_build::Missing {
            from: String::from("scenes/main.scene"),
            path: String::from("models/lost.glb"),
        });
        let (said, program) = report(&Err(e));
        assert_eq!(said, "Build failed: 1 file named but not found");
        assert!(program.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
