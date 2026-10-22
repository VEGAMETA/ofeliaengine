//! The code editor's checkers (ADR 0107 E8-3b, E10-2a): a language's analysis on a thread
//! of its own, so a check never holds up a frame. Luau's is `engine_luau_analysis`;
//! Slang's, its front end (`crate::slang_check`). The editor hands one a tab's text a
//! moment after the text changes; the findings come back with the text's version, so
//! findings made for older text are known as such.

use engine_luau_analysis::{Analysis, Completion, Diagnostic, Solver};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What the checker is asked.
enum Job {
    /// Module `name` holds `text` (its tab's generation and version): check it.
    Check {
        name: String,
        text: String,
        stamp: Stamp,
    },
    /// Module `name` is read from the project's folder again (its tab closed).
    Forget(String),
    /// The types every module sees are these definitions from now on (E7-2: the
    /// engine's scripting API).
    Definitions(String),
    /// What may be written at a place in module `name` holding `text` (Ctrl+Space).
    Complete {
        name: String,
        text: String,
        stamp: Stamp,
        at: (u32, u32),
    },
    /// What is at a place in it (the mouse resting there).
    Hover {
        name: String,
        text: String,
        stamp: Stamp,
        at: (u32, u32),
    },
    /// The signature of the call a place in it is inside (typing its arguments).
    Signature {
        name: String,
        text: String,
        stamp: Stamp,
        at: (u32, u32),
    },
    /// Where what is at a place in it is defined (F12, Ctrl+click).
    Definition {
        name: String,
        text: String,
        at: (u32, u32),
    },
}

/// What the checker answers.
pub(crate) enum Answer {
    /// A check's findings.
    Checked(Checked),
    /// What may be written at `at` (a line and a byte) in the text of `stamp`.
    Completed {
        stamp: Stamp,
        at: (u32, u32),
        items: Vec<Completion>,
    },
    /// What is at `at`: its type, as Luau writes it.
    Hovered {
        stamp: Stamp,
        at: (u32, u32),
        text: Option<String>,
    },
    /// Where it is defined: a module, a line and a byte (none for Luau's own).
    Defined(Option<(String, u32, u32)>),
    /// The signature there and the byte range of the argument being written.
    Signature {
        stamp: Stamp,
        found: Option<(String, (usize, usize))>,
    },
}

/// Which text a check was of: its tab's generation (a tab opened or read again anew is
/// another) and the text's version in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub(crate) generation: u64,
    pub(crate) version: u64,
}

/// What a check found.
pub(crate) struct Checked {
    /// The module.
    pub(crate) name: String,
    /// Which text of it.
    pub(crate) stamp: Stamp,
    /// What was found in it.
    pub(crate) found: Vec<Diagnostic>,
    /// How long the check took.
    pub(crate) took: Duration,
}

/// What a checker's thread runs a language's analysis through. Modules are named by their
/// paths in the project, with `/`; places are a line and a byte in it, from 0.
pub(crate) trait Analyzer {
    /// Module `name` holds `text` from now on (its tab's, unsaved).
    fn set_source(&mut self, name: &str, text: &str);
    /// Module `name` is its file's again (its tab closed).
    fn forget(&mut self, name: &str);
    /// What a check of module `name` finds.
    fn check(&mut self, name: &str) -> Vec<Diagnostic>;
    /// What may be written at a place in it.
    fn complete(&mut self, _name: &str, _line: u32, _byte: u32) -> Vec<Completion> {
        Vec::new()
    }
    /// What is at a place in it, as the language writes its type.
    fn hover(&mut self, _name: &str, _line: u32, _byte: u32) -> Option<String> {
        None
    }
    /// The signature of the call a place is inside, and the byte range of its argument
    /// there.
    fn signature(
        &mut self,
        _name: &str,
        _line: u32,
        _byte: u32,
    ) -> Option<(String, (usize, usize))> {
        None
    }
    /// Where what is at a place is defined: a module, a line and a byte.
    fn definition(&mut self, _name: &str, _line: u32, _byte: u32) -> Option<(String, u32, u32)> {
        None
    }
    /// The types every module sees are `text`'s definitions from now on.
    fn set_definitions(&mut self, _text: &str) {}
}

/// Luau's analysis with the engine's scripting API (E7-2): the old solver, which types an
/// un-annotated hook's `self` from `component`'s declaration (the new one of Luau 0.736
/// does not), and the definitions made again in a new analysis when they change (they
/// cannot be taken back from one), the tabs' texts given to it again.
pub(crate) struct LuauAnalyzer {
    root: PathBuf,
    analysis: Analysis,
    /// The tabs' texts, for a new analysis.
    sources: HashMap<String, String>,
    /// Whether the definitions loaded without a problem.
    loaded: bool,
}

impl LuauAnalyzer {
    /// The analysis of the project in `root`, with `definitions` (none when empty).
    pub(crate) fn new(root: &Path, definitions: &str) -> Option<Self> {
        let mut analysis = Analysis::new(root, Solver::Old)?;
        let loaded = load_definitions(&mut analysis, definitions);
        Some(Self {
            root: root.to_path_buf(),
            analysis,
            sources: HashMap::new(),
            loaded,
        })
    }

    /// Whether the definitions loaded without a problem (none given counts).
    #[cfg(test)]
    pub(crate) fn definitions_loaded(&self) -> bool {
        self.loaded
    }
}

/// The engine's definitions into `analysis`, whether without a problem; a problem in them
/// goes to the log.
fn load_definitions(analysis: &mut Analysis, text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    match analysis.add_definitions("@ofelia", text) {
        Ok(()) => true,
        Err((first, problems)) => {
            log::error!(
                target: "engine_editor",
                "the engine's Luau types: {problems} problems, the first: {first}"
            );
            false
        }
    }
}

impl Analyzer for LuauAnalyzer {
    fn set_source(&mut self, name: &str, text: &str) {
        self.sources.insert(name.to_owned(), text.to_owned());
        self.analysis.set_source(name, text);
    }

    fn forget(&mut self, name: &str) {
        self.sources.remove(name);
        self.analysis.forget_source(name);
    }

    fn check(&mut self, name: &str) -> Vec<Diagnostic> {
        self.analysis.check(name)
    }

    fn complete(&mut self, name: &str, line: u32, byte: u32) -> Vec<Completion> {
        self.analysis.complete(name, line, byte)
    }

    fn hover(&mut self, name: &str, line: u32, byte: u32) -> Option<String> {
        self.analysis.type_at(name, line, byte)
    }

    fn signature(&mut self, name: &str, line: u32, byte: u32) -> Option<(String, (usize, usize))> {
        self.analysis.signature(name, line, byte)
    }

    fn definition(&mut self, name: &str, line: u32, byte: u32) -> Option<(String, u32, u32)> {
        self.analysis.definition(name, line, byte)
    }

    fn set_definitions(&mut self, text: &str) {
        let Some(mut analysis) = Analysis::new(&self.root, Solver::Old) else {
            return;
        };
        self.loaded = load_definitions(&mut analysis, text);
        for (name, source) in &self.sources {
            analysis.set_source(name, source);
        }
        self.analysis = analysis;
    }
}

/// A checker's thread and its two queues.
pub(crate) struct Checker {
    jobs: Option<mpsc::Sender<Job>>,
    done: mpsc::Receiver<Answer>,
    thread: Option<JoinHandle<()>>,
}

impl Checker {
    /// Luau's checker of the project in folder `root`, with the engine's types
    /// `definitions` (E7-2); `None` when its thread cannot start.
    pub(crate) fn luau(root: &Path, definitions: String) -> Option<Self> {
        let root: PathBuf = root.to_path_buf();
        Self::start("luau checker", move || {
            let analysis = LuauAnalyzer::new(&root, &definitions);
            if analysis.is_none() {
                log::error!(target: "engine_editor", "Luau's analysis could not start");
            }
            analysis.map(|a| Box::new(a) as Box<dyn Analyzer>)
        })
    }

    /// Slang's checker of the project in folder `root` (E10-2a); `None` when its thread
    /// cannot start. Without Slang's library it says so once and checks nothing.
    pub(crate) fn slang(root: &Path) -> Option<Self> {
        let root: PathBuf = root.to_path_buf();
        Self::start("slang checker", move || {
            crate::slang_check::SlangAnalyzer::new(&root).map(|a| Box::new(a) as Box<dyn Analyzer>)
        })
    }

    /// A checker whose thread `named` runs the analysis `make` gives it.
    fn start(
        named: &str,
        make: impl FnOnce() -> Option<Box<dyn Analyzer>> + Send + 'static,
    ) -> Option<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (finished, done) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(named.to_owned())
            .spawn(move || {
                let Some(mut analysis) = make() else {
                    return;
                };
                // Each module's latest text only: jobs queued while one was checked are
                // taken together.
                let mut pending: Vec<(String, String, Stamp)> = Vec::new();
                while let Ok(job) = queue.recv() {
                    let mut next = Some(job);
                    while let Some(job) = next.take().or_else(|| queue.try_recv().ok()) {
                        match job {
                            Job::Check { name, text, stamp } => {
                                pending.retain(|p| p.0 != name);
                                pending.push((name, text, stamp));
                            }
                            Job::Forget(name) => {
                                pending.retain(|p| p.0 != name);
                                analysis.forget(&name);
                            }
                            Job::Definitions(text) => analysis.set_definitions(&text),
                            // Asked now, answered now: the text as it is then.
                            Job::Complete {
                                name,
                                text,
                                stamp,
                                at,
                            } => {
                                analysis.set_source(&name, &text);
                                let items = analysis.complete(&name, at.0, at.1);
                                let answer = Answer::Completed { stamp, at, items };
                                if finished.send(answer).is_err() {
                                    return;
                                }
                            }
                            Job::Signature {
                                name,
                                text,
                                stamp,
                                at,
                            } => {
                                analysis.set_source(&name, &text);
                                let found = analysis.signature(&name, at.0, at.1);
                                if finished.send(Answer::Signature { stamp, found }).is_err() {
                                    return;
                                }
                            }
                            Job::Definition { name, text, at } => {
                                analysis.set_source(&name, &text);
                                let found = analysis.definition(&name, at.0, at.1);
                                if finished.send(Answer::Defined(found)).is_err() {
                                    return;
                                }
                            }
                            Job::Hover {
                                name,
                                text,
                                stamp,
                                at,
                            } => {
                                analysis.set_source(&name, &text);
                                let text = analysis.hover(&name, at.0, at.1);
                                let answer = Answer::Hovered { stamp, at, text };
                                if finished.send(answer).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    for (name, text, stamp) in pending.drain(..) {
                        let t = Instant::now();
                        analysis.set_source(&name, &text);
                        let found = analysis.check(&name);
                        let checked = Checked {
                            name,
                            stamp,
                            found,
                            took: t.elapsed(),
                        };
                        if finished.send(Answer::Checked(checked)).is_err() {
                            return;
                        }
                    }
                }
            })
            .ok()?;
        Some(Self {
            jobs: Some(jobs),
            done,
            thread: Some(thread),
        })
    }

    /// The types every module sees are `text`'s definitions from now on: the editor
    /// checks its tabs again after.
    pub(crate) fn set_definitions(&self, text: String) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job::Definitions(text));
        }
    }

    /// Checks module `name`'s `text`, of `stamp`.
    pub(crate) fn check(&self, name: &str, text: String, stamp: Stamp) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job::Check {
                name: name.to_owned(),
                text,
                stamp,
            });
        }
    }

    /// What may be written at `at` in module `name`'s `text`, of `stamp`.
    pub(crate) fn complete(&self, name: &str, text: String, stamp: Stamp, at: (u32, u32)) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job::Complete {
                name: name.to_owned(),
                text,
                stamp,
                at,
            });
        }
    }

    /// What is at `at` in module `name`'s `text`, of `stamp`.
    pub(crate) fn hover(&self, name: &str, text: String, stamp: Stamp, at: (u32, u32)) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job::Hover {
                name: name.to_owned(),
                text,
                stamp,
                at,
            });
        }
    }

    /// The signature of the call `at` is inside in module `name`'s `text`, of `stamp`.
    pub(crate) fn signature(&self, name: &str, text: String, stamp: Stamp, at: (u32, u32)) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job::Signature {
                name: name.to_owned(),
                text,
                stamp,
                at,
            });
        }
    }

    /// Where what is at `at` in module `name`'s `text` is defined.
    pub(crate) fn definition(&self, name: &str, text: String, at: (u32, u32)) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job::Definition {
                name: name.to_owned(),
                text,
                at,
            });
        }
    }

    /// Module `name` is the folder's again.
    pub(crate) fn forget(&self, name: &str) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job::Forget(name.to_owned()));
        }
    }

    /// The answers since this was last asked.
    pub(crate) fn finished(&self) -> impl Iterator<Item = Answer> + '_ {
        self.done.try_iter()
    }
}

impl Drop for Checker {
    fn drop(&mut self) {
        // The queue closed, the thread ends after its check.
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
