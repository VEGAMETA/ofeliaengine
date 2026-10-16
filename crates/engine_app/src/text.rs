//! The game's text (ADR 0102): its [`Localizer`], a resource of the world, so every hook
//! that gets the world reaches it (`world.resource::<Localizer>()`); made from a strings
//! pack or, in development, from a `loc/` folder kept live.
//!
//! A folder is compiled at start, with the pseudo-locales `en-XA` and `ar-XB`, and again on
//! a thread of its own when one of its files changes and has stayed unchanged for 30 ms
//! (editors write in steps); between two frames the localizer's tables are replaced and its
//! `generation()` moves on. A locale whose files have errors keeps its previous table; the
//! errors are logged and kept ([`crate::Player::text_diagnostics`]).

use engine_core::jobs::ThreadPool;
use engine_loc::{Compiled, Diagnostic, Localizer, Strings, compile_sources, read_sources};
use engine_platform::fs_watch::{DirectoryWatcher, FileEventKind};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Where the game's text comes from ([`crate::PlayerSettings::text`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextSource {
    /// A strings pack (`ofeliaengine loc build`): what a shipped game reads.
    Pack(PathBuf),
    /// A `loc/` folder, a subfolder of `.ftl` files per locale, kept live (see the module
    /// documentation): no build step in development.
    Folder {
        /// The folder.
        dir: PathBuf,
        /// The source locale (`en`): complete, the one every other falls back to.
        source: String,
    },
}

/// How long a changed file must stay unchanged before the folder is compiled again.
const QUIET: Duration = Duration::from_millis(30);

/// The folder behind the world's localizer while the game runs.
pub(crate) struct LiveText {
    dir: PathBuf,
    source: String,
    watcher: DirectoryWatcher,
    /// When a file last changed, if one has since the last compile started.
    changed: Option<Instant>,
    compiling: Option<JoinHandle<std::io::Result<Compiled>>>,
    diagnostics: Vec<Diagnostic>,
}

impl LiveText {
    /// What the last compile found.
    pub(crate) fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Once per frame: notes changed files, compiles the folder on a thread once they have
    /// been quiet, and puts a finished compile's tables into `localizer`. Returns whether it
    /// did.
    pub(crate) fn update(&mut self, localizer: &mut Localizer) -> bool {
        while let Some(event) = self.watcher.try_next() {
            let fluent = event
                .path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ftl"));
            // A locale's folder made, renamed or removed has no extension.
            if fluent || event.kind == FileEventKind::Rescan || event.path.extension().is_none() {
                self.changed = Some(Instant::now());
            }
        }
        let mut replaced = false;
        if self.compiling.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.compiling.take()
        {
            match handle.join() {
                Ok(Ok(compiled)) => replaced = self.apply(compiled, localizer),
                Ok(Err(e)) => self.fail(format!("cannot be read: {e}")),
                Err(_) => self.fail(String::from("its compile panicked")),
            }
        }
        if self.compiling.is_none() && self.changed.is_some_and(|t| t.elapsed() >= QUIET) {
            self.changed = None;
            let (dir, source) = (self.dir.clone(), self.source.clone());
            match std::thread::Builder::new()
                .name(String::from("engine-loc-reload"))
                .spawn(move || compile_folder(&dir, &source))
            {
                Ok(handle) => self.compiling = Some(handle),
                Err(e) => self.fail(format!("no thread to compile it on: {e}")),
            }
        }
        replaced
    }

    /// A finished compile's tables into `localizer`, a broken locale's previous table kept.
    fn apply(&mut self, compiled: Compiled, localizer: &mut Localizer) -> bool {
        engine_profiler::zone!("app::text::apply");
        report(&compiled.diagnostics);
        let Compiled {
            tables,
            diagnostics,
            broken,
        } = compiled;
        self.diagnostics = diagnostics;
        let tables: Vec<Strings> = tables
            .into_iter()
            .map(|t| {
                if broken.contains(&t.locale) {
                    localizer.table(&t.locale).cloned().unwrap_or(t)
                } else {
                    t
                }
            })
            .collect();
        if tables.is_empty() {
            // The source locale's folder is gone: the text stays as it was.
            return false;
        }
        match localizer.replace_all(tables) {
            Ok(()) => true,
            Err(e) => {
                self.fail(e.to_string());
                false
            }
        }
    }

    fn fail(&mut self, message: String) {
        let d = Diagnostic {
            file: self.dir.display().to_string(),
            line: 0,
            column: 0,
            message,
            error: true,
        };
        report(core::slice::from_ref(&d));
        self.diagnostics = vec![d];
    }
}

/// The localizer of `source`, its locale the first of `languages` it serves; for a folder,
/// what keeps it live.
pub(crate) fn load_text(
    source: &TextSource,
    languages: &[String],
    jobs: &ThreadPool,
) -> Result<(Localizer, Option<LiveText>), String> {
    engine_profiler::zone!("app::text::load");
    let preferred: Vec<&str> = languages.iter().map(String::as_str).collect();
    match source {
        TextSource::Pack(path) => engine_assets::strings::load_localizer(path, jobs, &preferred)
            .map(|l| (l, None))
            .map_err(|e| format!("{}: {e}", path.display())),
        TextSource::Folder { dir, source } => {
            // Watched before it is read: a change made meanwhile is seen.
            let watcher =
                DirectoryWatcher::new(dir, true).map_err(|e| format!("{}: {e}", dir.display()))?;
            let compiled =
                compile_folder(dir, source).map_err(|e| format!("{}: {e}", dir.display()))?;
            report(&compiled.diagnostics);
            if compiled.tables.is_empty() {
                let errors: Vec<String> = compiled
                    .diagnostics
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                return Err(errors.join("; "));
            }
            let mut localizer = Localizer::new(compiled.tables, source)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
            localizer.set_locale(&preferred);
            let live = LiveText {
                dir: dir.clone(),
                source: source.clone(),
                watcher,
                changed: None,
                compiling: None,
                diagnostics: compiled.diagnostics,
            };
            Ok((localizer, Some(live)))
        }
    }
}

fn compile_folder(dir: &Path, source: &str) -> std::io::Result<Compiled> {
    let sources = read_sources(dir)?;
    Ok(compile_sources(&sources, source, true))
}

/// Diagnostics into the log.
fn report(diagnostics: &[Diagnostic]) {
    for d in diagnostics {
        if d.error {
            log::error!(target: "engine_app::text", "{d}");
        } else {
            log::warn!(target: "engine_app::text", "{d}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_loc::{Args, loc};

    fn write(dir: &Path, locale: &str, text: &str) {
        std::fs::create_dir_all(dir.join(locale)).unwrap();
        std::fs::write(dir.join(locale).join("hud.ftl"), text).unwrap();
    }

    /// Updates, a frame every 5 ms, until `done` or five seconds pass.
    fn until(
        live: &mut LiveText,
        l: &mut Localizer,
        done: impl Fn(&LiveText, &Localizer) -> bool,
    ) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            live.update(l);
            if done(live, l) {
                return true;
            }
            #[allow(clippy::disallowed_methods)] // a test's pause: no precision needed
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_folder_is_kept_live() {
        let dir = std::env::temp_dir().join(format!("engine_app_text_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "en", "ammo = Ammo: { $n }\nplay = Play\n");
        write(&dir, "ru", "ammo = Патроны: { $n }\n");
        let jobs = ThreadPool::builder().workers(1).build().unwrap();
        let source = TextSource::Folder {
            dir: dir.clone(),
            source: String::from("en"),
        };
        let (mut l, live) = load_text(&source, &[String::from("ru-RU")], &jobs).unwrap();
        let mut live = live.unwrap();
        let ammo = |l: &Localizer| l.text(loc!("ammo"), &Args::new().with("n", 1200));
        assert_eq!(l.locale(), "ru");
        assert_eq!(ammo(&l), "Патроны: 1\u{a0}200");
        // The pseudo-locales are there to switch to.
        assert!(l.locales().any(|x| x == "en-XA"));
        // An edit shows.
        write(&dir, "ru", "ammo = Боезапас: { $n }\nplay = Играть\n");
        assert!(
            until(&mut live, &mut l, |_, l| ammo(l).starts_with("Боезапас")),
            "{}",
            ammo(&l)
        );
        assert_eq!(l.text(loc!("play"), &Args::new()), "Играть");
        assert_eq!(l.locale(), "ru");
        // A broken edit keeps the previous table and says where.
        let generation = l.generation();
        write(&dir, "ru", "ammo = { $n ->\n");
        assert!(until(&mut live, &mut l, |live, _| live
            .diagnostics()
            .iter()
            .any(|d| d.error && d.file.ends_with("hud.ftl"))));
        assert!(ammo(&l).starts_with("Боезапас"), "{}", ammo(&l));
        assert!(l.generation() > generation);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
