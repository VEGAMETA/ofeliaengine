//! The code editor's checker (ADR 0107 E8-3b): Luau's analysis (`engine_luau_analysis`) on
//! a thread of its own, so a check never holds up a frame. The editor hands it a script's
//! text a moment after the text changes; the findings come back with the text's version,
//! so findings made for older text are known as such.

use engine_luau_analysis::{Analysis, Completion, Diagnostic, Solver};
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

/// The checker's thread and its two queues.
pub(crate) struct Checker {
    jobs: Option<mpsc::Sender<Job>>,
    done: mpsc::Receiver<Answer>,
    thread: Option<JoinHandle<()>>,
}

impl Checker {
    /// A checker of the project in folder `root`; `None` when its thread cannot start.
    pub(crate) fn new(root: &Path) -> Option<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (finished, done) = mpsc::channel();
        let root: PathBuf = root.to_path_buf();
        let thread = std::thread::Builder::new()
            .name(String::from("luau checker"))
            .spawn(move || {
                let Some(mut analysis) = Analysis::new(&root, Solver::New) else {
                    log::error!(target: "engine_editor", "Luau's analysis could not start");
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
                                analysis.forget_source(&name);
                            }
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
                                let text = analysis.type_at(&name, at.0, at.1);
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
