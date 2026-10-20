//! The editor's own files beside the project (ADR 0107 §4, `docs/research/ux/sessions.md`):
//! `.ofelia/` holds the session and the scenes' autosaves, never the user's files.
//!
//! - [`Lock`]: `.ofelia/session.lock`, held open without sharing while an editor runs. A
//!   lock left behind that nobody holds means the last run did not exit cleanly.
//! - [`Writer`]: one thread that writes files whole (beside the target, flushed, renamed
//!   over it), so the UI thread only makes their text.
//! - [`SessionFile`]: the scenes the editor had open, what their files were and where
//!   their unsaved changes are.
//! - [`Base`]: what a scene file was when it was loaded or saved, to tell whether it
//!   changed on disk since.

use engine_platform::time::HighResTimer;
use engine_scene::text::{quote_into, split_assignment, split_items, unquote};
use engine_scene::{AssetId, SceneId};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

/// The project's folder for the editor's own files: never committed (`ofeliaengine new`
/// ignores it).
pub(crate) const DIR: &str = ".ofelia";

/// How long unsaved changes wait before they are autosaved.
pub(crate) const AUTOSAVE_AFTER: Duration = Duration::from_secs(30);

/// An autosave waits for this long without input…
pub(crate) const QUIET: Duration = Duration::from_secs(1);

/// …but no longer than this past its time, once no gesture runs.
pub(crate) const PATIENCE: Duration = Duration::from_secs(30);

/// How long the lock says `restoring` after a session was brought back: a crash in that
/// time makes the next start open the files as saved.
pub(crate) const RESTORING: Duration = Duration::from_secs(10);

/// Older autosaves kept beside a scene's latest (`.1`, `.2`).
pub(crate) const OLDER_AUTOSAVES: u32 = 2;

/// How often a view that changed (the camera, the selection, the tree) is written into
/// the session, at a quiet moment.
pub(crate) const VIEW_EVERY: Duration = Duration::from_secs(10);

/// Recently run commands the session keeps.
pub(crate) const RECENT: usize = 50;

/// Whether an autosave due at `due` is written at `now`: never during a gesture (a drag,
/// typing, a held button); once due, at a quiet moment, or at the latest [`PATIENCE`]
/// after it.
pub(crate) fn autosave_now(now: Instant, due: Instant, last_input: Instant, gesture: bool) -> bool {
    !gesture
        && now >= due
        && (now.saturating_duration_since(last_input) >= QUIET || now >= due + PATIENCE)
}

/// How the last editor on the project ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LastRun {
    /// It exited cleanly, or none ran.
    Clean,
    /// It did not exit cleanly: a crash, or the process was ended.
    Crashed,
    /// It stopped while it brought a session back: this start opens the files as saved.
    CrashedRestoring,
}

/// `.ofelia/session.lock`, held while an editor runs on the project.
pub(crate) struct Lock {
    file: File,
    path: PathBuf,
}

impl Lock {
    /// Takes the lock of the project whose editor folder is `dir`, and tells how the last
    /// run ended.
    ///
    /// # Errors
    ///
    /// When another editor holds it, or the folder cannot be made.
    pub(crate) fn take(dir: &Path) -> Result<(Self, LastRun), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join("session.lock");
        let existed = path.exists();
        let mut file = open_exclusive(&path).map_err(|e| {
            // ERROR_SHARING_VIOLATION: another process has it open.
            if e.raw_os_error() == Some(32) {
                String::from("another editor has this project open")
            } else {
                format!("{}: {e}", path.display())
            }
        })?;
        let mut state = String::new();
        let _ = file.read_to_string(&mut state);
        let last = match (existed, state.trim()) {
            (false, _) => LastRun::Clean,
            (true, "restoring") => LastRun::CrashedRestoring,
            (true, _) => LastRun::Crashed,
        };
        let mut lock = Self { file, path };
        lock.mark("running")?;
        Ok((lock, last))
    }

    /// Writes the editor's state into the lock: `restoring` while a session comes back,
    /// `running` after.
    pub(crate) fn mark(&mut self, state: &str) -> Result<(), String> {
        let f = &mut self.file;
        f.set_len(0)
            .and_then(|()| f.rewind())
            .and_then(|()| f.write_all(state.as_bytes()))
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }

    /// A clean exit: the lock goes. Called after the session's last writes landed.
    pub(crate) fn release(self) {
        drop(self.file);
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
}

#[cfg(not(windows))]
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// What a file was: its size, last write time and a hash of its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Base {
    pub(crate) size: u64,
    /// The last write, in nanoseconds since 1970.
    pub(crate) time: u64,
    pub(crate) hash: u64,
}

impl Base {
    /// The file at `path` as it is now; none when it cannot be read.
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        Some(Self::of_bytes(&bytes, modified(path)?))
    }

    /// A file of `bytes` last written at `time`.
    pub(crate) fn of_bytes(bytes: &[u8], time: u64) -> Self {
        Self {
            size: bytes.len() as u64,
            time,
            hash: engine_core::hash::state_hash64(bytes),
        }
    }

    /// Whether the file at `path` is still this one: the same size and write time, or
    /// else the same bytes. `None` when there is no such file.
    pub(crate) fn matches(&self, path: &Path) -> Option<bool> {
        let meta = std::fs::metadata(path).ok()?;
        if meta.len() == self.size && modified(path) == Some(self.time) {
            return Some(true);
        }
        let bytes = std::fs::read(path).ok()?;
        Some(Self::of_bytes(&bytes, self.time) == *self)
    }
}

/// The last write time of `path`, in nanoseconds since 1970.
pub(crate) fn modified(path: &Path) -> Option<u64> {
    let time = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = time.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    u64::try_from(since.as_nanos()).ok()
}

/// How a scene was looked at: the editor's camera, the selection, the tree.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct View {
    /// The orbit camera's target, yaw, pitch and distance.
    pub(crate) camera: Option<([f32; 3], f32, f32, f32)>,
    /// The selection by scene id, in the order picked: the active one last.
    pub(crate) selection: Vec<SceneId>,
    /// Where a Shift range starts.
    pub(crate) anchor: Option<SceneId>,
    /// The tree's closed rows.
    pub(crate) collapsed: Vec<SceneId>,
    pub(crate) tree_filter: String,
    pub(crate) inspector_filter: String,
    /// The tool bar's tool (0: select).
    pub(crate) tool: usize,
}

/// A scene the session had open.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Document {
    /// Its file, relative to the project; none for a scene never saved.
    pub(crate) path: Option<PathBuf>,
    /// The scene's own id.
    pub(crate) id: AssetId,
    /// What its file was when it was loaded or last saved.
    pub(crate) base: Option<Base>,
    /// Its unsaved changes, relative to `.ofelia/`.
    pub(crate) autosave: Option<PathBuf>,
    /// How it was looked at.
    pub(crate) view: View,
}

/// The editor's own state, the same for every scene.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EditorState {
    /// Snapping by default, and its steps: position, scale, angle, other numbers.
    pub(crate) snapping: Option<(bool, [f64; 4])>,
    /// The commands run, by their stable names, the longest ago first.
    pub(crate) recent: Vec<String>,
    /// What a new build of the game's code does (`background`, `always`, `ask`).
    pub(crate) restart: Option<String>,
}

/// `.ofelia/session`: what the editor brings back at its next start.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SessionFile {
    pub(crate) editor: EditorState,
    pub(crate) documents: Vec<Document>,
}

/// The session file's version. Keys and sections it does not know are skipped, so a
/// later editor's file still reads.
const VERSION: u32 = 1;

impl SessionFile {
    /// The file's text: a header, the editor's block, then a block per document.
    pub(crate) fn to_text(&self) -> String {
        let mut out = format!("[session version={VERSION}]\n\n[editor]\n");
        if let Some((on, [position, scale, angle, number])) = self.editor.snapping {
            let _ = writeln!(
                out,
                "snapping = ({on}, {position}, {scale}, {angle}, {number})"
            );
        }
        if !self.editor.recent.is_empty() {
            out.push_str("recent = [");
            for (i, name) in self.editor.recent.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                quote_into(&mut out, name);
            }
            out.push_str("]\n");
        }
        if let Some(restart) = &self.editor.restart {
            out.push_str("restart = ");
            quote_into(&mut out, restart);
            out.push('\n');
        }
        for d in &self.documents {
            out.push_str("\n[document]\n");
            if let Some(path) = &d.path {
                out.push_str("path = ");
                quote_into(&mut out, &slashes(path));
                out.push('\n');
            }
            let _ = writeln!(out, "id = {}", d.id);
            if let Some(b) = d.base {
                let _ = writeln!(out, "base = ({}, {}, {:016x})", b.size, b.time, b.hash);
            }
            if let Some(path) = &d.autosave {
                out.push_str("autosave = ");
                quote_into(&mut out, &slashes(path));
                out.push('\n');
            }
            let v = &d.view;
            if let Some(([x, y, z], yaw, pitch, distance)) = v.camera {
                let _ = writeln!(out, "camera = ({x}, {y}, {z}, {yaw}, {pitch}, {distance})");
            }
            write_ids(&mut out, "selection", &v.selection);
            if let Some(anchor) = v.anchor {
                let _ = writeln!(out, "anchor = @{anchor}");
            }
            write_ids(&mut out, "collapsed", &v.collapsed);
            for (key, text) in [
                ("tree_filter", &v.tree_filter),
                ("inspector_filter", &v.inspector_filter),
            ] {
                if !text.is_empty() {
                    let _ = write!(out, "{key} = ");
                    quote_into(&mut out, text);
                    out.push('\n');
                }
            }
            if v.tool != 0 {
                let _ = writeln!(out, "tool = {}", v.tool);
            }
        }
        out
    }

    /// Reads a session file's text.
    ///
    /// # Errors
    ///
    /// Another version, or a line the format cannot read (with its number).
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        enum Section {
            Editor,
            Document,
            /// A section of a later editor: skipped.
            Other,
        }
        let mut lines = text.lines().enumerate();
        let header = lines.next().map(|(_, l)| l.trim());
        let expected = format!("[session version={VERSION}]");
        if header != Some(expected.as_str()) {
            return Err(format!(
                "not a session file of version {VERSION}: `{}`",
                header.unwrap_or("")
            ));
        }
        let mut file = Self::default();
        let mut section = Section::Other;
        for (n, line) in lines {
            let line = line.trim();
            let at = |e: String| format!("line {}: {e}", n + 1);
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                section = match line {
                    "[editor]" => Section::Editor,
                    "[document]" => {
                        file.documents.push(Document {
                            path: None,
                            id: AssetId::NONE,
                            base: None,
                            autosave: None,
                            view: View::default(),
                        });
                        Section::Document
                    }
                    _ => Section::Other,
                };
                continue;
            }
            let (key, value) = split_assignment(line)
                .ok_or_else(|| at(format!("`{line}` is not `key = value`")))?;
            match section {
                Section::Other => {}
                Section::Editor => match key {
                    "snapping" => {
                        let parts = items(value, '(', ')').map_err(at)?;
                        let [on, position, scale, angle, number] = parts[..] else {
                            return Err(at(format!("`{value}` is not (on, 4 steps)")));
                        };
                        let on = match on {
                            "true" => true,
                            "false" => false,
                            other => return Err(at(format!("`{other}` is not true or false"))),
                        };
                        file.editor.snapping = Some((
                            on,
                            [
                                parse_number(position).map_err(at)?,
                                parse_number(scale).map_err(at)?,
                                parse_number(angle).map_err(at)?,
                                parse_number(number).map_err(at)?,
                            ],
                        ));
                    }
                    "recent" => {
                        file.editor.recent = items(value, '[', ']')
                            .and_then(|names| names.into_iter().map(unquote).collect())
                            .map_err(at)?;
                    }
                    "restart" => file.editor.restart = Some(unquote(value).map_err(at)?),
                    _ => {}
                },
                Section::Document => {
                    let Some(doc) = file.documents.last_mut() else {
                        continue;
                    };
                    read_document_key(doc, key, value).map_err(at)?;
                }
            }
        }
        Ok(file)
    }
}

/// One `key = value` of a `[document]` block; keys of a later editor are skipped.
fn read_document_key(doc: &mut Document, key: &str, value: &str) -> Result<(), String> {
    let view = &mut doc.view;
    match key {
        "path" => doc.path = Some(PathBuf::from(unquote(value)?)),
        "autosave" => doc.autosave = Some(PathBuf::from(unquote(value)?)),
        "id" => {
            doc.id =
                AssetId::parse(value).ok_or_else(|| format!("`{value}` is not an asset id"))?;
        }
        "base" => {
            let parts = items(value, '(', ')')?;
            let [size, time, hash] = parts[..] else {
                return Err(format!("`{value}` is not (size, time, hash)"));
            };
            doc.base = Some(Base {
                size: parse_number(size)?,
                time: parse_number(time)?,
                hash: u64::from_str_radix(hash, 16)
                    .map_err(|_| format!("`{hash}` is not a hex hash"))?,
            });
        }
        "camera" => {
            let parts = items(value, '(', ')')?;
            let [x, y, z, yaw, pitch, distance] = parts[..] else {
                return Err(format!("`{value}` is not (x, y, z, yaw, pitch, distance)"));
            };
            view.camera = Some((
                [parse_number(x)?, parse_number(y)?, parse_number(z)?],
                parse_number(yaw)?,
                parse_number(pitch)?,
                parse_number(distance)?,
            ));
        }
        "selection" => view.selection = scene_ids(value)?,
        "collapsed" => view.collapsed = scene_ids(value)?,
        "anchor" => view.anchor = Some(scene_id(value)?),
        "tree_filter" => view.tree_filter = unquote(value)?,
        "inspector_filter" => view.inspector_filter = unquote(value)?,
        "tool" => view.tool = parse_number(value)?,
        _ => {}
    }
    Ok(())
}

/// Writes `key = [@id, …]` unless `ids` is empty.
fn write_ids(out: &mut String, key: &str, ids: &[SceneId]) {
    if ids.is_empty() {
        return;
    }
    let _ = write!(out, "{key} = [");
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "@{id}");
    }
    out.push_str("]\n");
}

/// The items of `value` written between `open` and `close`: `(a, b)` or `[a, b]`.
fn items(value: &str, open: char, close: char) -> Result<Vec<&str>, String> {
    value
        .strip_prefix(open)
        .and_then(|v| v.strip_suffix(close))
        .map(split_items)
        .ok_or_else(|| format!("`{value}` is not {open}…{close}"))
}

fn parse_number<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("`{s}` is not a number"))
}

fn scene_id(s: &str) -> Result<SceneId, String> {
    s.trim()
        .strip_prefix('@')
        .and_then(SceneId::parse)
        .ok_or_else(|| format!("`{s}` is not @<scene id>"))
}

fn scene_ids(value: &str) -> Result<Vec<SceneId>, String> {
    items(value, '[', ']')?.into_iter().map(scene_id).collect()
}

/// A path with forward slashes, as the project's other files write paths.
fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Where a scene's unsaved changes go, relative to `.ofelia/`: by its file's stem and its
/// id, so two `main.scene` in different folders stay apart and a renamed file keeps its
/// autosaves.
pub(crate) fn autosave_name(scene: Option<&Path>, id: AssetId) -> PathBuf {
    let stem = scene.and_then(Path::file_stem).map_or_else(
        || String::from("untitled"),
        |s| s.to_string_lossy().into_owned(),
    );
    PathBuf::from(format!("autosave/{stem}-{id}.scene"))
}

/// The `n`-th older copy of `path`: `main-….scene` → `main-….1.scene`.
pub(crate) fn older(path: &Path, n: u32) -> PathBuf {
    match path.extension() {
        Some(ext) => path.with_extension(format!("{n}.{}", ext.to_string_lossy())),
        None => path.with_extension(n.to_string()),
    }
}

enum Job {
    /// `text` into `path` whole; with `keep`, the file it replaces becomes the newest of
    /// `keep` older copies when it is at least `apart` old.
    Write {
        path: PathBuf,
        text: String,
        keep: u32,
        apart: Duration,
    },
    Remove(Vec<PathBuf>),
    Rename(PathBuf, PathBuf),
    Flush(mpsc::Sender<()>),
}

/// A write the writer finished.
#[derive(Debug)]
pub(crate) struct Written {
    pub(crate) path: PathBuf,
    pub(crate) result: Result<(), String>,
}

/// One thread that writes the editor's files, in the order they were sent.
pub(crate) struct Writer {
    jobs: Option<mpsc::Sender<Job>>,
    done: mpsc::Receiver<Written>,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    /// Starts the writer's thread.
    ///
    /// # Errors
    ///
    /// When the thread cannot start.
    pub(crate) fn new() -> Result<Self, String> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (finished, done) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(String::from("editor writer"))
            .spawn(move || {
                for job in queue {
                    match job {
                        Job::Write {
                            path,
                            text,
                            keep,
                            apart,
                        } => {
                            let result = write_whole(&path, text.as_bytes(), keep, apart);
                            let _ = finished.send(Written { path, result });
                        }
                        Job::Remove(paths) => {
                            for path in paths {
                                let _ = std::fs::remove_file(path);
                            }
                        }
                        Job::Rename(from, to) => {
                            let result = rename_retrying(&from, &to);
                            let _ = finished.send(Written { path: to, result });
                        }
                        Job::Flush(reply) => {
                            let _ = reply.send(());
                        }
                    }
                }
            })
            .map_err(|e| format!("the editor's writer: {e}"))?;
        Ok(Self {
            jobs: Some(jobs),
            done,
            thread: Some(thread),
        })
    }

    fn send(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
        }
    }

    /// Writes `text` into `path` whole, keeping `keep` older copies at least `apart` in
    /// age from each other.
    pub(crate) fn write(&self, path: PathBuf, text: String, keep: u32, apart: Duration) {
        self.send(Job::Write {
            path,
            text,
            keep,
            apart,
        });
    }

    /// Deletes the files (missing ones are fine).
    pub(crate) fn remove(&self, paths: Vec<PathBuf>) {
        self.send(Job::Remove(paths));
    }

    /// Renames a file, after everything sent before it.
    pub(crate) fn rename(&self, from: PathBuf, to: PathBuf) {
        self.send(Job::Rename(from, to));
    }

    /// Waits until everything sent before is written.
    pub(crate) fn flush(&self) {
        let (reply, wait) = mpsc::channel();
        self.send(Job::Flush(reply));
        let _ = wait.recv();
    }

    /// The writes finished since the last call.
    pub(crate) fn finished(&self) -> impl Iterator<Item = Written> + '_ {
        self.done.try_iter()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // The queue ends, the thread writes what is left and returns.
        drop(self.jobs.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Writes `bytes` into `path` whole: beside it first, flushed to the disk, then renamed
/// over it, so a crash leaves the old file or the new one, never half of one. With `keep`,
/// the file it replaces becomes `.1` (and `.1` becomes `.2`, up to `keep`) when it is at
/// least `apart` old; a younger one is replaced.
fn write_whole(path: &Path, bytes: &[u8], keep: u32, apart: Duration) -> Result<(), String> {
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(at)?;
    }
    let partial = path.with_extension("partial");
    let mut f = File::create(&partial).map_err(at)?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(at)?;
    drop(f);
    let age = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    if keep > 0 && age.is_some_and(|a| a >= apart) {
        for n in (1..keep).rev() {
            let from = older(path, n);
            if from.exists() {
                rename_retrying(&from, &older(path, n + 1))?;
            }
        }
        rename_retrying(path, &older(path, 1))?;
    }
    rename_retrying(&partial, path)
}

/// Renames `from` over `to`, again a few times while another process (an indexer, an
/// antivirus) holds one of them.
fn rename_retrying(from: &Path, to: &Path) -> Result<(), String> {
    let mut tries = 0;
    let mut timer = None;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && tries < 10 => {
                tries += 1;
                // The writer's own thread: waiting here holds up nothing else.
                if timer.is_none() {
                    timer = HighResTimer::new().ok();
                }
                if let Some(t) = &timer {
                    let _ = t.sleep(Duration::from_millis(20));
                }
            }
            Err(e) => return Err(format!("{} → {}: {e}", from.display(), to.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ofelia-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_autosave_waits_for_a_quiet_moment_but_not_forever() {
        let t0 = Instant::now();
        let due = t0 + AUTOSAVE_AFTER;
        let s = Duration::from_secs;
        // Not yet due; due but typing goes on; due and a second quiet.
        assert!(!autosave_now(t0 + s(10), due, t0, false));
        assert!(!autosave_now(due, due, due, false));
        assert!(autosave_now(due + s(1), due, due, false));
        // Never during a gesture, however late.
        assert!(!autosave_now(due + s(60), due, due + s(60), true));
        // Input that never pauses: at the latest PATIENCE after it was due.
        assert!(!autosave_now(due + s(29), due, due + s(29), false));
        assert!(autosave_now(due + PATIENCE, due, due + PATIENCE, false));
    }

    #[test]
    fn the_lock_tells_a_crash_from_a_clean_exit_and_keeps_a_second_editor_out() {
        let dir = folder("lock").join(DIR);
        let (lock, last) = Lock::take(&dir).unwrap();
        assert_eq!(last, LastRun::Clean);
        let second = Lock::take(&dir);
        assert!(
            second.as_ref().is_err_and(|e| e.contains("another editor")),
            "{:?}",
            second.err()
        );
        lock.release();
        assert!(!dir.join("session.lock").exists());

        // A run that ends without releasing (a crash) leaves the lock behind.
        let (lock, _) = Lock::take(&dir).unwrap();
        drop(lock);
        let (mut lock, last) = Lock::take(&dir).unwrap();
        assert_eq!(last, LastRun::Crashed);
        // One that stops while it brings a session back.
        lock.mark("restoring").unwrap();
        drop(lock);
        let (lock, last) = Lock::take(&dir).unwrap();
        assert_eq!(last, LastRun::CrashedRestoring);
        lock.release();
    }

    #[test]
    fn the_session_file_reads_back_what_it_wrote() {
        let file = SessionFile {
            documents: vec![
                Document {
                    path: Some(PathBuf::from("scenes/main.scene")),
                    id: AssetId::from_u128(0x41bd_9902_e927_1812_fecd_6cad_dd89_12bf),
                    base: Some(Base {
                        size: 1442,
                        time: 1_760_000_000_123_456_789,
                        hash: 0x0123_4567_89ab_cdef,
                    }),
                    autosave: Some(PathBuf::from(
                        "autosave/main-41bd9902e9271812fecd6caddd8912bf.scene",
                    )),
                    view: View {
                        camera: Some(([0.5, -1.25, 3.0], 0.6, 0.45, 8.0)),
                        selection: vec![SceneId::from_u128(2), SceneId::from_u128(3)],
                        anchor: Some(SceneId::from_u128(2)),
                        collapsed: vec![SceneId::from_u128(4)],
                        tree_filter: String::from("cu\"be"),
                        inspector_filter: String::new(),
                        tool: 2,
                    },
                },
                Document {
                    path: None,
                    id: AssetId::from_u128(7),
                    base: None,
                    autosave: None,
                    view: View::default(),
                },
            ],
            editor: EditorState {
                snapping: Some((true, [0.1, 0.25, 45.0, 0.1])),
                recent: vec![String::from("edit.undo"), String::from("scene.duplicate")],
                restart: Some(String::from("ask")),
            },
        };
        let text = file.to_text();
        assert!(text.starts_with("[session version=1]\n"), "{text}");
        assert!(
            text.contains("base = (1442, 1760000000123456789, 0123456789abcdef)"),
            "{text}"
        );
        assert_eq!(SessionFile::parse(&text).unwrap(), file);
        assert!(SessionFile::parse("[session version=2]\n").is_err());
        // A later editor's keys and sections are skipped; a value that does not read is
        // an error on its line.
        let later = SessionFile::parse(
            "[session version=1]\n[panels]\nleft = 300\n[document]\nspeed = 3\ntool = 1\n",
        )
        .unwrap();
        assert_eq!(later.documents[0].view.tool, 1);
        let bad = SessionFile::parse("[session version=1]\n[document]\ntool = fast\n");
        assert!(bad.is_err_and(|e| e.starts_with("line 3:")));
    }

    #[test]
    fn a_base_tells_a_changed_file_from_a_touched_one() {
        let dir = folder("base");
        let path = dir.join("main.scene");
        std::fs::write(&path, "[scene version=3]\n").unwrap();
        let base = Base::of(&path).unwrap();
        assert_eq!(base.matches(&path), Some(true));
        // Written again with the same bytes: another time, the same file.
        std::fs::write(&path, "[scene version=3]\n").unwrap();
        let touched = Base {
            time: base.time.wrapping_sub(1),
            ..base
        };
        assert_eq!(touched.matches(&path), Some(true));
        // Other bytes.
        std::fs::write(&path, "[scene version=3]\n\n").unwrap();
        assert_eq!(base.matches(&path), Some(false));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(base.matches(&path), None);
    }

    #[test]
    fn the_writer_writes_whole_files_and_keeps_older_copies() {
        let dir = folder("writer");
        let path = dir.join("autosave/main-1.scene");
        let writer = Writer::new().unwrap();
        // Younger than `apart`: replaced, no copy.
        writer.write(
            path.clone(),
            String::from("one"),
            2,
            Duration::from_secs(30),
        );
        writer.write(
            path.clone(),
            String::from("two"),
            2,
            Duration::from_secs(30),
        );
        writer.flush();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert!(!older(&path, 1).exists());
        // Old enough: each write pushes the one before it down.
        for text in ["three", "four", "five"] {
            writer.write(path.clone(), String::from(text), 2, Duration::ZERO);
        }
        writer.flush();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "five");
        assert_eq!(std::fs::read_to_string(older(&path, 1)).unwrap(), "four");
        assert_eq!(std::fs::read_to_string(older(&path, 2)).unwrap(), "three");
        assert!(!older(&path, 3).exists(), "two older copies at most");
        assert!(!path.with_extension("partial").exists());
        let written: Vec<Written> = writer.finished().collect();
        assert_eq!(written.len(), 5);
        assert!(written.iter().all(|w| w.result.is_ok()));
        writer.remove(vec![path.clone(), older(&path, 1), older(&path, 2)]);
        writer.flush();
        assert!(!path.exists() && !older(&path, 1).exists() && !older(&path, 2).exists());
        assert_eq!(
            autosave_name(Some(Path::new("scenes/main.scene")), AssetId::from_u128(1)),
            PathBuf::from("autosave/main-00000000000000000000000000000001.scene")
        );
        assert_eq!(
            older(Path::new("autosave/main-1.scene"), 2),
            PathBuf::from("autosave/main-1.2.scene")
        );
    }
}
