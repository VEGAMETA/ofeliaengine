//! Saves in the player (ADR 0104): quicksave, slots and autosave written behind the frame,
//! loading at a tick boundary with the level's objects back first.
//!
//! The game asks through the world's [`Saves`] resource (from its hooks or systems) or the
//! [`Player`](crate::Player); the player takes the save after the frame's ticks, the
//! writer thread encodes, compresses and swaps the file in.

use engine_ecs::reflect::Reflect;
use engine_save::{Encoding, LoadReport, SaveHeader, SaveImage, SlotInfo, WriteStats};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// The quicksave's slot.
pub const QUICK_SLOT: &str = "quick";

/// How a game's saves work ([`crate::PlayerSettings::saves`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SaveSettings {
    /// The folder of the slots; `None`: `<Saved Games>\<studio>\<title>` (`FOLDERID_SavedGames`;
    /// without a studio, `<Saved Games>\<title>`).
    pub folder: Option<PathBuf>,
    /// The studio's name: the folder above the game's in `Saved Games`.
    pub studio: String,
    /// The game's version, written into every save's header.
    pub game_version: String,
    /// The encoding written; `None`: the build's (text with debug assertions, the binary
    /// without). Both are always read.
    pub encoding: Option<Encoding>,
    /// Autosaves; `None`: none (a game's checkpoints call `autosave_now`).
    pub autosave: Option<Autosave>,
}

/// Autosaves on the game's clock, into slots taken in turn (`autosave 1`, `autosave 2`,
/// …), skipped while one is written or the game says not now
/// (`Game::can_autosave`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Autosave {
    /// Time played between two.
    pub every: Duration,
    /// How many slots they take in turn (at least 1).
    pub slots: u32,
}

impl Default for Autosave {
    /// Every five minutes, three slots.
    fn default() -> Self {
        Self {
            every: Duration::from_secs(300),
            slots: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    Save(String),
    Load(String),
    Autosave,
}

/// What the writer thread reports.
#[derive(Debug, Default)]
pub(crate) struct WriterStatus {
    pub(crate) pending: u32,
    pub(crate) last: Option<Result<(String, WriteStats), String>>,
}

/// The game's saves: a resource of the world, for its hooks and systems.
///
/// ```ignore
/// fn tick(&mut self, world: &mut World, dt: f32) {
///     if pressed(F5) { world.resource_mut::<Saves>().unwrap().quicksave(); }
///     if pressed(F9) { world.resource_mut::<Saves>().unwrap().quickload(); }
/// }
/// ```
pub struct Saves {
    pub(crate) requests: Vec<Request>,
    folder: PathBuf,
    status: Arc<Mutex<WriterStatus>>,
    pub(crate) last_load: Option<Result<LoadReport, String>>,
}

impl Saves {
    pub(crate) fn new(folder: PathBuf, status: Arc<Mutex<WriterStatus>>) -> Self {
        Self {
            requests: Vec::new(),
            folder,
            status,
            last_load: None,
        }
    }

    /// Saves to the quick slot after this frame's ticks.
    pub fn quicksave(&mut self) {
        self.save(QUICK_SLOT);
    }

    /// Loads the quick slot at the next tick boundary.
    pub fn quickload(&mut self) {
        self.load(QUICK_SLOT);
    }

    /// Saves to `slot` after this frame's ticks: taken in the frame, written behind it.
    pub fn save(&mut self, slot: &str) {
        self.requests.push(Request::Save(slot.to_owned()));
    }

    /// Loads `slot` at the next tick boundary.
    pub fn load(&mut self, slot: &str) {
        self.requests.push(Request::Load(slot.to_owned()));
    }

    /// An autosave now (a checkpoint), into the next autosave slot.
    pub fn autosave_now(&mut self) {
        self.requests.push(Request::Autosave);
    }

    /// The saves in the folder with their headers, newest first (reads the folder: for
    /// a menu, not every frame).
    pub fn slots(&self) -> Vec<SlotInfo> {
        engine_save::list_slots(&self.folder)
    }

    /// The folder of the slots.
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Whether a save is being written.
    pub fn writing(&self) -> bool {
        self.status.lock().is_ok_and(|s| s.pending > 0)
    }

    /// The last write's outcome: its slot and what it took, or why it failed.
    pub fn last_write(&self) -> Option<Result<(String, WriteStats), String>> {
        self.status.lock().ok().and_then(|s| s.last.clone())
    }

    /// The last load's outcome: what it did (its notes: what no longer fit), or why it
    /// failed.
    pub fn last_load(&self) -> Option<&Result<LoadReport, String>> {
        self.last_load.as_ref()
    }
}

/// A save on its way to the disk.
pub(crate) struct Job {
    pub(crate) image: SaveImage,
    pub(crate) path: PathBuf,
    pub(crate) slot: String,
    pub(crate) encoding: Encoding,
    pub(crate) header: SaveHeader,
}

/// The thread that writes saves: started with the first, joined when the player drops
/// (a save asked for just before quitting is not lost).
pub(crate) struct Writer {
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    status: Arc<Mutex<WriterStatus>>,
}

impl Writer {
    pub(crate) fn new(status: Arc<Mutex<WriterStatus>>) -> Self {
        Self {
            jobs: None,
            thread: None,
            status,
        }
    }

    pub(crate) fn send(&mut self, job: Job) {
        if self.jobs.is_none() {
            let (tx, rx) = mpsc::channel::<Job>();
            let status = Arc::clone(&self.status);
            let thread = std::thread::Builder::new()
                .name("save writer".to_owned())
                .spawn(move || {
                    for job in rx {
                        let result = engine_save::write_save(
                            &job.path,
                            &job.image,
                            job.encoding,
                            &job.header,
                        )
                        .map(|stats| (job.slot.clone(), stats))
                        .map_err(|e| format!("{}: {e}", job.path.display()));
                        if let Err(e) = &result {
                            log::error!("save: {e}");
                        }
                        if let Ok(mut s) = status.lock() {
                            s.pending = s.pending.saturating_sub(1);
                            s.last = Some(result);
                        }
                    }
                })
                .ok();
            self.thread = thread;
            self.jobs = Some(tx);
        }
        if let Ok(mut s) = self.status.lock() {
            s.pending += 1;
        }
        if let Some(jobs) = &self.jobs
            && let Err(mpsc::SendError(job)) = jobs.send(job)
        {
            // No thread: write here rather than lose it.
            let _ = engine_save::write_save(&job.path, &job.image, job.encoding, &job.header);
            if let Ok(mut s) = self.status.lock() {
                s.pending = s.pending.saturating_sub(1);
            }
        }
    }

    pub(crate) fn pending(&self) -> bool {
        self.status.lock().is_ok_and(|s| s.pending > 0)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The player's clock in a save: the fixed step's remainder and the time played.
#[derive(Debug, Default, Clone, Copy, PartialEq, Reflect)]
pub(crate) struct ClockState {
    pub(crate) remainder: f64,
    pub(crate) play_ms: u64,
}

/// The folder of a game's slots: the settings' own, or `Saved Games` with the studio and
/// title below it (a name's characters Windows refuses replaced).
pub(crate) fn save_folder(settings: &SaveSettings, title: &str) -> PathBuf {
    if let Some(folder) = &settings.folder {
        return folder.clone();
    }
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if "<>:\"/\\|?*".contains(c) || c.is_control() {
                    '_'
                } else {
                    c
                }
            })
            .collect::<String>()
            .trim()
            .trim_end_matches('.')
            .to_owned()
    };
    let base = engine_platform::folders::saved_games()
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Saved Games")))
        .unwrap_or_else(|| PathBuf::from("saves"));
    let mut dir = base;
    let studio = clean(&settings.studio);
    if !studio.is_empty() {
        dir.push(studio);
    }
    let title = clean(title);
    dir.push(if title.is_empty() {
        "game".to_owned()
    } else {
        title
    });
    dir
}
