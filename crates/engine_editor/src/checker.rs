//! The code editor's checker (ADR 0107 E8-3b): Luau's analysis (`engine_luau_analysis`) on
//! a thread of its own, so a check never holds up a frame. The editor hands it a script's
//! text a moment after the text changes; the findings come back with the text's version,
//! so findings made for older text are known as such.

use engine_luau_analysis::{Analysis, Diagnostic, Solver};
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
    done: mpsc::Receiver<Checked>,
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
                        if finished.send(checked).is_err() {
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

    /// Module `name` is the folder's again.
    pub(crate) fn forget(&self, name: &str) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job::Forget(name.to_owned()));
        }
    }

    /// The checks finished since this was last asked.
    pub(crate) fn finished(&self) -> impl Iterator<Item = Checked> + '_ {
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
