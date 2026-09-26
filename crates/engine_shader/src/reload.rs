//! Shader hot reload (CLAUDE.md §9): file save → recompile the affected shaders → new
//! SPIR-V to the renderer; on errors the previous version keeps running.
//!
//! - [`ShaderSet`] is the bookkeeping, independent of how changes are detected: the
//!   tracked sources, the files each one read (as reported by Slang), and the last good
//!   output. Given changed paths it recompiles exactly the affected sources.
//! - [`ShaderHotReload`] runs a `ShaderSet` on a background thread fed by directory
//!   watchers (Windows), so nothing blocks the caller; results arrive as
//!   [`ShaderEvent`]s through a channel polled once per frame.

use crate::ShaderError;
use crate::build::{UnitResult, compile_all_streaming};
use crate::cache::{CacheOutcome, ShaderCache};
use crate::compiler::{CompileOptions, CompileOutput};
use crate::pool::CompilerPool;
use crate::slang::SlangLibrary;
use crate::sources::{
    declares_entry_points, find_shader_sources, is_slang_file, normalize, path_key,
};
use engine_core::jobs::ThreadPool;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A change in the compiled shaders.
#[derive(Debug)]
pub enum ShaderEvent {
    /// New SPIR-V for every entry point of `path`; replaces all earlier output for it.
    /// Sent when a source is first built and whenever its SPIR-V changes (not for edits
    /// that produce identical code).
    Compiled {
        /// Source file.
        path: PathBuf,
        /// Entry points, dependencies, warnings.
        output: Arc<CompileOutput>,
        /// Whether it came from the cache.
        outcome: CacheOutcome,
        /// Time spent on this file.
        time: Duration,
    },
    /// Compilation failed. The last [`ShaderEvent::Compiled`] output for `path`, if any,
    /// remains the one to use.
    Failed {
        /// Source file.
        path: PathBuf,
        /// The error, with file:line diagnostics.
        error: Arc<ShaderError>,
        /// Time spent on this file.
        time: Duration,
    },
    /// `path` was deleted or no longer declares entry points.
    Removed {
        /// Source file.
        path: PathBuf,
    },
}

impl ShaderEvent {
    /// The source file the event is about.
    pub fn path(&self) -> &Path {
        match self {
            Self::Compiled { path, .. } | Self::Failed { path, .. } | Self::Removed { path } => {
                path
            }
        }
    }
}

#[derive(Debug)]
struct Unit {
    /// Display form (normalized, original case).
    path: PathBuf,
    /// Keys of every file the last compilation read, the source included.
    dependencies: Vec<PathBuf>,
    last_good: Option<Arc<CompileOutput>>,
    failed: bool,
    dirty: bool,
}

/// The shader sources under a set of roots and their build state.
#[derive(Debug)]
pub struct ShaderSet {
    roots: Vec<PathBuf>,
    root_keys: Vec<PathBuf>,
    options: CompileOptions,
    /// By [`path_key`].
    units: BTreeMap<PathBuf, Unit>,
    rescan: bool,
    removed: Vec<PathBuf>,
}

impl ShaderSet {
    /// Tracks every shader source under `roots`. Nothing happens until [`Self::rebuild`],
    /// which scans the roots and builds everything.
    pub fn new(roots: Vec<PathBuf>, options: CompileOptions) -> Self {
        let root_keys = roots.iter().map(|r| path_key(r)).collect();
        Self {
            roots,
            root_keys,
            options,
            units: BTreeMap::new(),
            rescan: true,
            removed: Vec::new(),
        }
    }

    /// The watched roots.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Compile options used for every source.
    pub fn options(&self) -> &CompileOptions {
        &self.options
    }

    /// Number of tracked sources.
    pub fn len(&self) -> usize {
        self.units.len()
    }

    /// Whether no source is tracked.
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// The last successful output for `path`.
    pub fn output(&self, path: &Path) -> Option<&Arc<CompileOutput>> {
        self.units.get(&path_key(path))?.last_good.as_ref()
    }

    /// Whether [`Self::rebuild`] has anything to do.
    pub fn has_pending_work(&self) -> bool {
        self.rescan || !self.removed.is_empty() || self.units.values().any(|u| u.dirty)
    }

    /// Re-scan the roots on the next rebuild (notifications were lost).
    pub fn request_rescan(&mut self) {
        self.rescan = true;
    }

    /// Records that `path` was created, modified, renamed or deleted.
    pub fn changed(&mut self, path: &Path) {
        let key = path_key(path);
        let slang = is_slang_file(path);
        if slang {
            let has_entries =
                std::fs::read_to_string(path).is_ok_and(|t| declares_entry_points(&t));
            match (self.units.contains_key(&key), has_entries) {
                (true, false) => {
                    if let Some(unit) = self.units.remove(&key) {
                        self.removed.push(unit.path);
                    }
                }
                (false, true) if self.root_keys.iter().any(|r| key.starts_with(r)) => {
                    self.units.insert(
                        key.clone(),
                        Unit {
                            path: normalize(path),
                            dependencies: vec![key.clone()],
                            last_good: None,
                            failed: false,
                            dirty: true,
                        },
                    );
                }
                _ => {}
            }
        }
        let mut affected = false;
        for unit in self.units.values_mut() {
            let depends = unit.dependencies.contains(&key);
            // A new or edited module may be the import a failed shader was missing.
            if depends || (slang && unit.failed) {
                unit.dirty = true;
                affected |= depends;
            }
        }
        // A directory appeared, vanished or was renamed: its contents are unknown.
        if !slang && !affected && (path.is_dir() || self.units.keys().any(|k| k.starts_with(&key)))
        {
            self.rescan = true;
        }
    }

    /// Recompiles everything affected by the changes recorded since the last call and
    /// returns what changed: removals first, then one event per recompiled source whose
    /// output changed or that failed, in source order.
    pub fn rebuild(
        &mut self,
        jobs: &ThreadPool,
        compilers: &CompilerPool,
        cache: &ShaderCache,
        max_parallel: usize,
    ) -> Vec<ShaderEvent> {
        let events = Mutex::new(Vec::new());
        self.rebuild_streaming(jobs, compilers, cache, max_parallel, |event| {
            events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(event);
        });
        let mut events = events.into_inner().unwrap_or_else(PoisonError::into_inner);
        // Removals first (they are emitted first), then by path.
        events.sort_by(|a, b| {
            let rank = |e: &ShaderEvent| !matches!(e, ShaderEvent::Removed { .. });
            (rank(a), a.path()).cmp(&(rank(b), b.path()))
        });
        events
    }

    /// [`Self::rebuild`] that hands every event to `emit` as soon as it is known (on a
    /// worker thread, in completion order), so the first recompiled shader can be used
    /// while the others still compile.
    pub fn rebuild_streaming(
        &mut self,
        jobs: &ThreadPool,
        compilers: &CompilerPool,
        cache: &ShaderCache,
        max_parallel: usize,
        emit: impl Fn(ShaderEvent) + Sync,
    ) {
        engine_profiler::zone!("shader::ShaderSet::rebuild");
        if self.rescan {
            self.rescan = false;
            self.scan();
        }
        for path in self.removed.drain(..) {
            emit(ShaderEvent::Removed { path });
        }
        let dirty: Vec<PathBuf> = self
            .units
            .iter()
            .filter(|(_, u)| u.dirty)
            .map(|(k, _)| k.clone())
            .collect();
        if dirty.is_empty() {
            return;
        }
        let sources: Vec<PathBuf> = dirty.iter().map(|k| self.units[k].path.clone()).collect();
        let units = &self.units;
        let decide = |index: usize, result: &UnitResult| -> Option<ShaderEvent> {
            let unit = &units[&dirty[index]];
            match &result.result {
                Ok((output, outcome)) => {
                    let unchanged = !unit.failed
                        && unit
                            .last_good
                            .as_ref()
                            .is_some_and(|last| last.entries == output.entries);
                    (!unchanged).then(|| ShaderEvent::Compiled {
                        path: result.path.clone(),
                        output: Arc::clone(output),
                        outcome: *outcome,
                        time: result.time,
                    })
                }
                Err(error) => Some(ShaderEvent::Failed {
                    path: result.path.clone(),
                    error: Arc::clone(error),
                    time: result.time,
                }),
            }
        };
        let report = compile_all_streaming(
            jobs,
            compilers,
            cache,
            &sources,
            &self.options,
            max_parallel,
            |index, result| {
                if let Some(event) = decide(index, result) {
                    emit(event);
                }
            },
        );
        for (key, result) in dirty.into_iter().zip(report.units) {
            let Some(unit) = self.units.get_mut(&key) else {
                continue;
            };
            unit.dirty = false;
            match result.result {
                Ok((output, _)) => {
                    let mut dependencies: Vec<PathBuf> =
                        output.dependencies.iter().map(|d| path_key(d)).collect();
                    dependencies.push(key);
                    dependencies.sort();
                    dependencies.dedup();
                    unit.dependencies = dependencies;
                    unit.failed = false;
                    // Same decision as `decide`: keep the old allocation when unchanged.
                    if unit
                        .last_good
                        .as_ref()
                        .is_none_or(|last| last.entries != output.entries)
                    {
                        unit.last_good = Some(output);
                    }
                }
                Err(_) => {
                    // Keep the old dependencies so that fixing any of them retries.
                    unit.failed = true;
                }
            }
        }
    }

    /// Syncs the tracked sources with the disk; everything left is marked dirty (the
    /// cache makes unchanged ones cheap).
    fn scan(&mut self) {
        let Ok(found) = find_shader_sources(&self.roots) else {
            // A root is unreadable (for example, being replaced by a checkout): keep what
            // is tracked and retry every source.
            self.units.values_mut().for_each(|u| u.dirty = true);
            return;
        };
        let mut found: BTreeMap<PathBuf, PathBuf> =
            found.into_iter().map(|p| (path_key(&p), p)).collect();
        let gone: Vec<PathBuf> = self
            .units
            .keys()
            .filter(|k| !found.contains_key(*k))
            .cloned()
            .collect();
        for key in gone {
            if let Some(unit) = self.units.remove(&key) {
                self.removed.push(unit.path);
            }
        }
        for unit in self.units.values_mut() {
            unit.dirty = true;
        }
        let known: Vec<PathBuf> = found
            .keys()
            .filter(|k| self.units.contains_key(*k))
            .cloned()
            .collect();
        for key in known {
            found.remove(&key);
        }
        for (key, path) in found {
            self.units.insert(
                key.clone(),
                Unit {
                    path,
                    dependencies: vec![key],
                    last_good: None,
                    failed: false,
                    dirty: true,
                },
            );
        }
    }
}

/// Settings for [`ShaderHotReload`].
#[derive(Debug, Clone)]
pub struct HotReloadConfig {
    /// Directories to scan and watch (recursively).
    pub roots: Vec<PathBuf>,
    /// Compile options for every source. Its include directories are watched too.
    pub options: CompileOptions,
    /// Shader cache directory.
    pub cache_dir: PathBuf,
    /// Quiet time that ends a burst of file events (editors often write a file in
    /// several steps). Default 10 ms.
    pub settle: Duration,
    /// Most files compiled at once (each needs a ~150 MB Slang session). Default 4.
    pub max_parallel: usize,
    /// Slang sessions kept alive between rebuilds so the next edit does not pay
    /// ~100–250 ms to create one. Default 1.
    pub warm_compilers: usize,
    /// Start OS directory watchers (Windows). With `false`, only
    /// [`ShaderHotReload::notify_changed`] reports changes. Default `true`.
    pub watch: bool,
}

impl HotReloadConfig {
    /// Defaults for the given roots, caching in `cache_dir`.
    pub fn new(roots: Vec<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            roots,
            options: CompileOptions::default(),
            cache_dir: cache_dir.into(),
            settle: Duration::from_millis(10),
            max_parallel: 4,
            warm_compilers: 1,
            watch: true,
        }
    }
}

enum Command {
    Changed(PathBuf),
    Rescan,
    Stop,
}

/// Background shader rebuilding on file changes.
///
/// On start it builds every source under the roots (one [`ShaderEvent::Compiled`] or
/// [`ShaderEvent::Failed`] per source), then rebuilds on every change. Poll
/// [`Self::try_next`] once per frame; it never blocks.
///
/// # Examples
///
/// ```no_run
/// use engine_core::jobs::ThreadPool;
/// use engine_shader::{HotReloadConfig, ShaderEvent, ShaderHotReload, SlangLibrary};
/// use std::sync::Arc;
///
/// let jobs = Arc::new(ThreadPool::builder().build().unwrap());
/// let config = HotReloadConfig::new(vec!["shaders".into()], "target/shader-cache");
/// let reload = ShaderHotReload::start(SlangLibrary::load().unwrap(), jobs, config).unwrap();
/// // Once per frame:
/// while let Some(event) = reload.try_next() {
///     match event {
///         ShaderEvent::Compiled { path, output, .. } => { /* swap pipelines */ }
///         ShaderEvent::Failed { error, .. } => eprintln!("{error}"),
///         ShaderEvent::Removed { path } => { /* drop pipelines */ }
///     }
/// }
/// ```
#[derive(Debug)]
pub struct ShaderHotReload {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<ShaderEvent>,
    thread: Option<JoinHandle<()>>,
    // Dropped after the thread is joined (fields drop after `Drop::drop`).
    #[cfg(windows)]
    _watchers: Vec<engine_platform::fs_watch::DirectoryWatcher>,
}

impl ShaderHotReload {
    /// Starts the reload thread (and, with `config.watch`, the directory watchers).
    pub fn start(
        library: Arc<SlangLibrary>,
        jobs: Arc<ThreadPool>,
        config: HotReloadConfig,
    ) -> Result<Self, ShaderError> {
        let cache = ShaderCache::new(&config.cache_dir)?;
        let (commands, command_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();

        #[cfg(windows)]
        let watchers = if config.watch {
            start_watchers(&config, &commands)?
        } else {
            Vec::new()
        };

        let set = ShaderSet::new(config.roots.clone(), config.options.clone());
        let compilers = CompilerPool::new(library);
        let thread = std::thread::Builder::new()
            .name(String::from("engine-shader-reload"))
            .spawn(move || {
                engine_profiler::set_thread_name("engine-shader-reload");
                run(
                    set,
                    &jobs,
                    &compilers,
                    &cache,
                    &config,
                    &command_rx,
                    &event_tx,
                );
            })?;
        Ok(Self {
            commands,
            events,
            thread: Some(thread),
            #[cfg(windows)]
            _watchers: watchers,
        })
    }

    /// Reports a change the watchers may not see (or on hosts without watchers).
    pub fn notify_changed(&self, path: &Path) {
        let _ = self.commands.send(Command::Changed(path.to_owned()));
    }

    /// Re-scans the roots and re-checks every source.
    pub fn request_rescan(&self) {
        let _ = self.commands.send(Command::Rescan);
    }

    /// The next event, if any. Never blocks.
    pub fn try_next(&self) -> Option<ShaderEvent> {
        self.events.try_recv().ok()
    }

    /// Waits up to `timeout` for the next event (tools and tests).
    pub fn next_timeout(&self, timeout: Duration) -> Option<ShaderEvent> {
        self.events.recv_timeout(timeout).ok()
    }
}

impl Drop for ShaderHotReload {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            // A compile in progress finishes first (Slang cannot be interrupted).
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
fn start_watchers(
    config: &HotReloadConfig,
    commands: &mpsc::Sender<Command>,
) -> Result<Vec<engine_platform::fs_watch::DirectoryWatcher>, ShaderError> {
    use engine_platform::fs_watch::{DirectoryWatcher, FileEventKind};
    let mut dirs: Vec<PathBuf> = config.roots.clone();
    dirs.extend(config.options.include_dirs.iter().cloned());
    let mut watchers = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let tx = commands.clone();
        let watcher = DirectoryWatcher::with_sink(&dir, true, move |event| {
            let command = match event.kind {
                FileEventKind::Rescan => Command::Rescan,
                _ => Command::Changed(event.path),
            };
            tx.send(command).is_ok()
        })?;
        watchers.push(watcher);
    }
    Ok(watchers)
}

fn run(
    mut set: ShaderSet,
    jobs: &ThreadPool,
    compilers: &CompilerPool,
    cache: &ShaderCache,
    config: &HotReloadConfig,
    commands: &mpsc::Receiver<Command>,
    events: &mpsc::Sender<ShaderEvent>,
) {
    // Returns false on stop.
    let apply = |set: &mut ShaderSet, command: Command| match command {
        Command::Changed(path) => {
            set.changed(&path);
            true
        }
        Command::Rescan => {
            set.request_rescan();
            true
        }
        Command::Stop => false,
    };
    loop {
        if set.has_pending_work() {
            // A send fails only when the owner is gone, and then `Stop` is queued too.
            set.rebuild_streaming(jobs, compilers, cache, config.max_parallel, |event| {
                let _ = events.send(event);
            });
            compilers.trim(config.warm_compilers);
            // Also after an all-cache-hit build, so the first edit finds a session. A
            // failure here resurfaces on the next compile that needs a session.
            let _ = compilers.warm_up(config.warm_compilers);
        }
        let Ok(first) = commands.recv() else { return };
        if !apply(&mut set, first) {
            return;
        }
        // Absorb the rest of the burst, but not forever (a busy directory never settles).
        let give_up = Instant::now() + config.settle * 10;
        loop {
            match commands.recv_timeout(config.settle) {
                Ok(command) => {
                    if !apply(&mut set, command) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if Instant::now() >= give_up {
                break;
            }
        }
    }
}
