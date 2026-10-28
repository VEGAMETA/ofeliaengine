//! Material types (ADR 0092): Slang types implementing `IMaterial`, drawn by the engine's
//! passes specialized for them.
//!
//! A type's pipeline is the engine's pipeline (the "role" it stands in for: the forward
//! shading with ambient occlusion, the masked shadow pass, ...) with the type's fragment
//! module: `forward.slang` compiled with `-D MATERIAL=<type> -D MATERIAL_FILE="<module>"`
//! for that entry point only. So every feature's pipeline set serves every type.
//!
//! Nothing waits for a type. A frame that draws a type's batches with a role whose
//! pipeline is not made yet draws them with the engine's pipeline and the material's glTF
//! fallback, and asks for it; [`MaterialTypes::update`] (once per frame, before the frame's
//! passes are declared) hands the entry point to a compiler thread, and once its SPIR-V is
//! back describes the pipeline for the device's warm-up thread, which creates it. Later
//! frames draw with it once it exists.
//!
//! Hot reload ([`MaterialTypes::set_hot_reload`]): the folders of the files a type's
//! compiles read (Slang's dependencies: its module, `forward.slang` and what they include)
//! are watched. When one changes and stays unchanged for [`RELOAD_QUIET`], the entry
//! points the type is drawn with compile again in one request. A tool that writes a module
//! itself says so ([`MaterialTypes::changed`], E10-3g-1): the reload is due at once, not
//! when the watcher's event comes. The pipelines in use keep
//! drawing until their successors are made; then they are retired, and destroyed once the
//! frames that may use them are done ([`MaterialTypes::collect`]). A module that does not
//! compile keeps the previous version drawing; its error is kept for tools
//! ([`MaterialTypes::error`]).
//!
//! A type's parameters' layout ([`MaterialTypes::params`], ADR 0120's E10-1b) is reflected
//! on the same thread when first asked for, and again with every reload, so what lays a
//! material's parameters out at run time (the player's material files) never compiles on
//! the frame's thread.

use crate::material::MaterialTypeId;
use engine_platform::fs_watch::{FileEventKind, WatchSet};
use engine_rhi::{Device, LazyPipeline, Pipeline};
use engine_shader::{
    CompileOptions, Compiler, EntryPoint, ShaderError, SlangLibrary, Stage, StructLayout, path_key,
};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::ffi::CString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

/// A material type: a Slang module defining a type that implements `IMaterial`
/// (`forward.slang`), and the type's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialTypeDesc {
    /// A name for logs and tools.
    pub name: String,
    /// The module (a `.slang` file) that defines the type.
    pub module: PathBuf,
    /// The type's name in the module.
    pub type_name: String,
}

/// The engine's own shader folder: `forward.slang` and its modules (types compile from
/// it at run time; shipped games will carry precompiled SPIR-V, ADR 0092).
pub const ENGINE_SHADER_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/shaders");

/// How long a changed file must stay unchanged before its types compile again: editors
/// write a file in several steps (truncate, write, rename).
pub const RELOAD_QUIET: Duration = Duration::from_millis(30);

/// What a material type compiles with (`engine_shader::material_types`).
pub fn material_type_options(shader_dir: &Path, desc: &MaterialTypeDesc) -> CompileOptions {
    engine_shader::material_types::material_type_options(shader_dir, &desc.module, &desc.type_name)
}

/// The layout of a material type's parameters (`<type>.Params`) as its shaders read them,
/// through Slang's reflection (`engine_shader::material_types`).
pub fn reflect_material_params(
    compiler: &mut Compiler,
    shader_dir: &Path,
    desc: &MaterialTypeDesc,
) -> Result<StructLayout, ShaderError> {
    engine_shader::material_types::reflect_material_params(
        compiler,
        shader_dir,
        &desc.module,
        &desc.type_name,
    )
}

/// A type's pipeline standing in for one of the engine's. The role is kept so that its id
/// stays unique while the entry exists, and to make the pipeline again on a reload.
enum Derived {
    /// Waiting for the type's SPIR-V of the role's entry point.
    Waiting { role: LazyPipeline, entry: CString },
    /// Described for the warm-up thread (drawn with once it exists); until then the
    /// previous version, after a reload, draws.
    Ready {
        role: LazyPipeline,
        derived: LazyPipeline,
        previous: Option<LazyPipeline>,
    },
    /// The entry point did not compile (logged); the fallback draws.
    Failed { role: LazyPipeline },
}

enum Spirv {
    Compiling,
    Ready(Arc<[u32]>),
    Failed,
}

/// Where a type's hot reload stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reload {
    Idle,
    /// A file it reads changed at this time.
    Changed(Instant),
    /// Its entry points are compiling again; `again`: a file changed meanwhile.
    Compiling {
        generation: u32,
        again: bool,
    },
}

impl Reload {
    /// A file the type reads changed at `now`: due once the files are quiet; compiling,
    /// to compile again after.
    fn changed(self, now: Instant) -> Self {
        match self {
            Self::Compiling { generation, .. } => Self::Compiling {
                generation,
                again: true,
            },
            _ => Self::Changed(now),
        }
    }
}

struct TypeEntry {
    desc: MaterialTypeDesc,
    /// By the role's id.
    pipelines: HashMap<usize, Derived>,
    /// By entry point.
    spirv: HashMap<CString, Spirv>,
    /// Files its compiles read ([`path_key`]s): a change to one reloads it.
    dependencies: Vec<PathBuf>,
    reload: Reload,
    /// Of the latest reload sent.
    generation: u32,
    /// The latest compile error (the previous version draws).
    error: Option<String>,
    /// Registered with a cooked pack's SPIR-V: an entry point missing from it means the
    /// pack was cooked with a PSO record that lacks it (warned once).
    precompiled: bool,
    /// The entry points frames drew the type with (PSO records, ADR 0092).
    drawn: BTreeSet<CString>,
    /// Its parameters' layout ([`MaterialTypes::params`]), its revision (moved by each
    /// reflection that arrives), and the generation of the latest reflection sent.
    params: Params,
    params_revision: u32,
    params_generation: u32,
}

/// Where a type's parameters' layout stands.
enum Params {
    NotAsked,
    /// Sent to the compiler's thread; nothing reflected yet.
    Asked,
    Ready(StructLayout),
    Failed(String),
}

/// An entry point's SPIR-V by name, and the files the compile read.
type Compiled = (Vec<(CString, Arc<[u32]>)>, Vec<PathBuf>);

enum Job {
    /// A role's entry point, the first time a frame draws the type with it.
    Entry {
        ty: MaterialTypeId,
        entry: CString,
        module: PathBuf,
        type_name: String,
    },
    /// The entry points a type is drawn with, again after a change.
    Reload {
        ty: MaterialTypeId,
        generation: u32,
        entries: Vec<CString>,
        module: PathBuf,
        type_name: String,
    },
    /// A type's parameters' layout (`<type>.Params`).
    Params {
        ty: MaterialTypeId,
        generation: u32,
        module: PathBuf,
        type_name: String,
    },
}

enum Done {
    Entry {
        ty: MaterialTypeId,
        entry: CString,
        result: Result<Compiled, String>,
    },
    Reload {
        ty: MaterialTypeId,
        generation: u32,
        result: Result<Compiled, String>,
    },
    Params {
        ty: MaterialTypeId,
        generation: u32,
        result: Result<StructLayout, String>,
    },
}

/// The thread that compiles types' entry points.
struct CompilerThread {
    jobs: Sender<Job>,
    done: Receiver<Done>,
    _thread: std::thread::JoinHandle<()>,
}

/// `entries` of `type_name` in `module` compiled in one request.
fn compile(
    compiler: Option<&mut Compiler>,
    shader_dir: &Path,
    module: &Path,
    type_name: &str,
    entries: &[CString],
) -> Result<Compiled, String> {
    let compiler = compiler.ok_or_else(|| String::from("no Slang compiler"))?;
    let options =
        engine_shader::material_types::material_type_options(shader_dir, module, type_name);
    let points: Vec<EntryPoint> = entries
        .iter()
        .map(|e| EntryPoint::new(&e.to_string_lossy(), Stage::Fragment))
        .collect();
    let out = compiler
        .compile_file(&shader_dir.join("forward.slang"), &points, &options)
        .map_err(|e| e.to_string())?;
    if out.entries.len() != entries.len() {
        return Err(String::from("not every entry point compiled"));
    }
    let spirv = entries
        .iter()
        .cloned()
        .zip(out.entries.into_iter().map(|e| Arc::from(e.spirv)))
        .collect();
    Ok((spirv, out.dependencies))
}

impl CompilerThread {
    fn start(shader_dir: PathBuf) -> Option<Self> {
        let (jobs, job_rx) = channel::<Job>();
        let (done_tx, done) = channel::<Done>();
        let thread = std::thread::Builder::new()
            .name(String::from("material compiler"))
            .spawn(move || {
                let compiler = SlangLibrary::load().and_then(Compiler::new);
                let mut compiler = match compiler {
                    Ok(c) => Some(c),
                    Err(e) => {
                        log::error!(
                            "material types: no Slang compiler ({e}); their fallbacks draw"
                        );
                        None
                    }
                };
                while let Ok(job) = job_rx.recv() {
                    let done = match job {
                        Job::Entry {
                            ty,
                            entry,
                            module,
                            type_name,
                        } => Done::Entry {
                            result: compile(
                                compiler.as_mut(),
                                &shader_dir,
                                &module,
                                &type_name,
                                core::slice::from_ref(&entry),
                            ),
                            ty,
                            entry,
                        },
                        Job::Reload {
                            ty,
                            generation,
                            entries,
                            module,
                            type_name,
                        } => Done::Reload {
                            result: compile(
                                compiler.as_mut(),
                                &shader_dir,
                                &module,
                                &type_name,
                                &entries,
                            ),
                            ty,
                            generation,
                        },
                        Job::Params {
                            ty,
                            generation,
                            module,
                            type_name,
                        } => Done::Params {
                            result: match compiler.as_mut() {
                                Some(compiler) => {
                                    engine_shader::material_types::reflect_material_params(
                                        compiler,
                                        &shader_dir,
                                        &module,
                                        &type_name,
                                    )
                                    .map_err(|e| e.to_string())
                                }
                                None => Err(String::from("no Slang compiler")),
                            },
                            ty,
                            generation,
                        },
                    };
                    if done_tx.send(done).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            jobs,
            done,
            _thread: thread,
        })
    }
}

/// Hot reload's watchers: the folders of the files types compile from.
struct HotReload {
    watch: WatchSet,
}

impl HotReload {
    /// Watches the folder of `file` (a [`path_key`]; once).
    fn watch(&mut self, file: &Path) {
        let Some(folder) = file.parent() else {
            return;
        };
        if let Err(e) = self.watch.watch(folder) {
            log::warn!(
                "material types: cannot watch {} for hot reload: {e}",
                folder.display()
            );
        }
    }
}

/// The registered material types and their pipelines (see the module documentation).
pub(crate) struct MaterialTypes {
    types: Vec<TypeEntry>,
    /// Roles frames drew types with and found no pipeline for: handled by [`Self::update`].
    requests: RefCell<Vec<(MaterialTypeId, LazyPipeline)>>,
    /// Types whose parameters' layout was asked for ([`Self::params`]): sent by
    /// [`Self::update`].
    params_requests: RefCell<Vec<MaterialTypeId>>,
    compiler: Option<CompilerThread>,
    shader_dir: PathBuf,
    /// Pipelines waiting for SPIR-V, and compiles in flight.
    waiting: usize,
    compiling: usize,
    hot_reload: Option<HotReload>,
    /// Pipelines replaced by a reload, with the last frame that may use them.
    retired: Vec<(u64, LazyPipeline)>,
    /// Entry points frames drew a precompiled type with that its pack lacks: type name and
    /// entry point (the pack's PSO record lacked them).
    pack_misses: Vec<(String, String)>,
    /// The frame being prepared ([`Self::collect`]).
    frame: u64,
}

impl core::fmt::Debug for MaterialTypes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MaterialTypes")
            .field("types", &self.types.len())
            .field("waiting", &self.waiting)
            .field("compiling", &self.compiling)
            .field("hot_reload", &self.hot_reload.is_some())
            .finish_non_exhaustive()
    }
}

impl MaterialTypes {
    pub(crate) fn new() -> Self {
        Self {
            types: Vec::new(),
            requests: RefCell::new(Vec::new()),
            params_requests: RefCell::new(Vec::new()),
            compiler: None,
            shader_dir: PathBuf::from(ENGINE_SHADER_DIR),
            waiting: 0,
            compiling: 0,
            hot_reload: None,
            retired: Vec::new(),
            pack_misses: Vec::new(),
            frame: 0,
        }
    }

    /// Where `forward.slang` and its modules are (the engine's own shader folder by
    /// default).
    pub(crate) fn set_shader_dir(&mut self, dir: PathBuf) {
        self.shader_dir = dir;
    }

    /// Watches the files types compile from, and compiles them again when one changes
    /// (see the module documentation). Off by default: development builds turn it on.
    pub(crate) fn set_hot_reload(&mut self, enabled: bool) {
        if !enabled {
            self.hot_reload = None;
            // Reloads due go with it: nothing waits out their quiet any more.
            for t in &mut self.types {
                if matches!(t.reload, Reload::Changed(_)) {
                    t.reload = Reload::Idle;
                }
            }
            return;
        }
        if self.hot_reload.is_some() {
            return;
        }
        let mut hot = HotReload {
            watch: WatchSet::new(),
        };
        for t in &self.types {
            for file in &t.dependencies {
                hot.watch(file);
            }
        }
        self.hot_reload = Some(hot);
    }

    /// A type's id: the one it was given if the same module and type were registered
    /// before (a material file per material does not make a type per material).
    pub(crate) fn register(&mut self, desc: MaterialTypeDesc) -> MaterialTypeId {
        if let Some(k) = self
            .types
            .iter()
            .position(|t| t.desc.module == desc.module && t.desc.type_name == desc.type_name)
        {
            return MaterialTypeId(k as u16 + 1);
        }
        let dependencies = vec![
            path_key(&desc.module),
            path_key(&self.shader_dir.join("forward.slang")),
        ];
        if let Some(hot) = &mut self.hot_reload {
            for file in &dependencies {
                hot.watch(file);
            }
        }
        self.types.push(TypeEntry {
            desc,
            pipelines: HashMap::new(),
            spirv: HashMap::new(),
            dependencies,
            reload: Reload::Idle,
            generation: 0,
            error: None,
            precompiled: false,
            drawn: BTreeSet::new(),
            params: Params::NotAsked,
            params_revision: 0,
            params_generation: 0,
        });
        MaterialTypeId(self.types.len() as u16)
    }

    /// [`Self::register`] with the type's entry points compiled ahead (a cooked pack's):
    /// pipelines are made from them without compiling.
    pub(crate) fn register_precompiled(
        &mut self,
        desc: MaterialTypeDesc,
        entries: &[(String, Vec<u32>)],
    ) -> MaterialTypeId {
        let ty = self.register(desc);
        let entry = &mut self.types[usize::from(ty.0) - 1];
        entry.precompiled = true;
        for (name, spirv) in entries {
            let Ok(name) = CString::new(name.as_str()) else {
                continue;
            };
            entry
                .spirv
                .entry(name)
                .or_insert_with(|| Spirv::Ready(Arc::from(spirv.as_slice())));
        }
        ty
    }

    /// Every type's entry points frames drew it with so far, sorted: what a PSO record
    /// keeps (ADR 0092), so that a cook compiles only those.
    pub(crate) fn drawn(&self) -> Vec<(&MaterialTypeDesc, Vec<String>)> {
        self.types
            .iter()
            .filter(|t| !t.drawn.is_empty())
            .map(|t| {
                let entries = t
                    .drawn
                    .iter()
                    .map(|e| e.to_string_lossy().into_owned())
                    .collect();
                (&t.desc, entries)
            })
            .collect()
    }

    /// What [`Self::pack_misses`] collected.
    pub(crate) fn pack_misses(&self) -> &[(String, String)] {
        &self.pack_misses
    }

    pub(crate) fn desc(&self, ty: MaterialTypeId) -> Option<&MaterialTypeDesc> {
        let index = usize::from(ty.0).checked_sub(1)?;
        self.types.get(index).map(|t| &t.desc)
    }

    /// The latest compile error of `ty` (its previous version, or its fallback, draws).
    pub(crate) fn error(&self, ty: MaterialTypeId) -> Option<&str> {
        let index = usize::from(ty.0).checked_sub(1)?;
        self.types.get(index)?.error.as_deref()
    }

    /// `ty`'s parameters' layout, or why it could not be reflected, and its revision;
    /// `None` while it is on its way (asked for the first time here, sent by the next
    /// [`Self::update`]). A reload of the type reflects it again: the revision moves when
    /// that arrives, the previous layout kept until then (and when the reflection fails).
    pub(crate) fn params(&self, ty: MaterialTypeId) -> Option<(Result<&StructLayout, &str>, u32)> {
        let index = usize::from(ty.0).checked_sub(1)?;
        let entry = self.types.get(index)?;
        match &entry.params {
            Params::NotAsked => {
                self.params_requests.borrow_mut().push(ty);
                None
            }
            Params::Asked => None,
            Params::Ready(layout) => Some((Ok(layout), entry.params_revision)),
            Params::Failed(why) => Some((Err(why.as_str()), entry.params_revision)),
        }
    }

    /// Sends a reflection of `ty`'s parameters to the compiler's thread (started if need
    /// be). Whether it went.
    fn send_params(&mut self, ty: MaterialTypeId) -> bool {
        let Some(entry) = self.types.get_mut(usize::from(ty.0) - 1) else {
            return false;
        };
        if self.compiler.is_none() {
            self.compiler = CompilerThread::start(self.shader_dir.clone());
        }
        entry.params_generation = entry.params_generation.wrapping_add(1);
        let sent = self.compiler.as_ref().is_some_and(|c| {
            c.jobs
                .send(Job::Params {
                    ty,
                    generation: entry.params_generation,
                    module: entry.desc.module.clone(),
                    type_name: entry.desc.type_name.clone(),
                })
                .is_ok()
        });
        if sent {
            self.compiling += 1;
        }
        sent
    }

    /// `ty`'s pipeline standing in for `role`, once created (or its previous version
    /// while a reload's is being made); otherwise `None` (the role's own pipeline draws,
    /// with the glTF fallback) and, the first time, a request.
    pub(crate) fn resolve(&self, ty: MaterialTypeId, role: &LazyPipeline) -> Option<&Pipeline> {
        let index = usize::from(ty.0).checked_sub(1)?;
        let entry = self.types.get(index)?;
        match entry.pipelines.get(&role.id()) {
            Some(Derived::Ready {
                derived, previous, ..
            }) => derived
                .get()
                .or_else(|| previous.as_ref().and_then(LazyPipeline::get)),
            Some(_) => None,
            None => {
                self.requests.borrow_mut().push((ty, role.clone()));
                None
            }
        }
    }

    /// Starts frame `frame` and destroys the retired pipelines the GPU is done with (the
    /// frames up to `completed` finished).
    pub(crate) fn collect(&mut self, frame: u64, completed: u64) {
        self.frame = frame;
        self.retired.retain(|(last, _)| *last > completed);
    }

    /// Requests, compiled entry points, changed files and pipelines to describe; once per
    /// frame, before the frame's passes are declared.
    pub(crate) fn update(&mut self, device: &Device) {
        if !self.params_requests.borrow().is_empty() {
            let asked = core::mem::take(&mut *self.params_requests.borrow_mut());
            for ty in asked {
                let index = usize::from(ty.0) - 1;
                if !matches!(
                    self.types.get(index).map(|t| &t.params),
                    Some(Params::NotAsked)
                ) {
                    continue;
                }
                let state = if self.send_params(ty) {
                    Params::Asked
                } else {
                    Params::Failed(String::from("no Slang compiler"))
                };
                let entry = &mut self.types[index];
                if matches!(state, Params::Failed(_)) {
                    entry.params_revision = entry.params_revision.wrapping_add(1);
                }
                entry.params = state;
            }
        }
        let requests = core::mem::take(&mut *self.requests.borrow_mut());
        let mut changed = false;
        for (ty, role) in requests {
            let Some(entry_name) = role.fragment_entry().map(CString::from) else {
                continue;
            };
            let Some(entry) = self.types.get_mut(usize::from(ty.0) - 1) else {
                continue;
            };
            if entry.drawn.insert(entry_name.clone())
                && entry.precompiled
                && !entry.spirv.contains_key(&entry_name)
            {
                log::warn!(
                    "material type {}: its pack has no `{}` (cooked with a PSO record that \
                     lacks it): its fallback draws until it compiles; record PSOs with this \
                     view's features and cook again",
                    entry.desc.name,
                    entry_name.to_string_lossy()
                );
                self.pack_misses.push((
                    entry.desc.type_name.clone(),
                    entry_name.to_string_lossy().into_owned(),
                ));
            }
            if entry.pipelines.contains_key(&role.id()) {
                continue;
            }
            if !device.adapter().caps.shader_int64
                && !matches!(entry.spirv.get(&entry_name), Some(Spirv::Failed))
            {
                // Types read their parameters at 64-bit addresses (`material_params`):
                // neither compiled nor precompiled code runs without them.
                log::warn!(
                    "material type {}: the GPU has no 64-bit shader integers; its fallback draws",
                    entry.desc.name
                );
                entry.spirv.insert(entry_name.clone(), Spirv::Failed);
            }
            if !entry.spirv.contains_key(&entry_name) {
                if self.compiler.is_none() {
                    self.compiler = CompilerThread::start(self.shader_dir.clone());
                }
                let sent = self.compiler.as_ref().is_some_and(|c| {
                    c.jobs
                        .send(Job::Entry {
                            ty,
                            entry: entry_name.clone(),
                            module: entry.desc.module.clone(),
                            type_name: entry.desc.type_name.clone(),
                        })
                        .is_ok()
                });
                if sent {
                    self.compiling += 1;
                    entry.spirv.insert(entry_name.clone(), Spirv::Compiling);
                } else {
                    entry.spirv.insert(entry_name.clone(), Spirv::Failed);
                }
            }
            entry.pipelines.insert(
                role.id(),
                Derived::Waiting {
                    role,
                    entry: entry_name,
                },
            );
            self.waiting += 1;
            changed = true;
        }
        while let Some(done) = self.compiler.as_ref().and_then(|c| c.done.try_recv().ok()) {
            self.accept(done, device);
            changed = true;
        }
        if self.hot_reload.is_some() {
            self.watch_files(device);
        }
        if changed {
            self.describe(device);
        }
        self.retire();
    }

    /// File `path` written anew by the caller (the editor's graph module, E10-3g-1): the
    /// types that compile from it reload as when the watcher sees it, but due at once, so
    /// what waits for [`Self::pending`] sees the reload on its way before the watcher's
    /// event comes (its event within the quiet merges with it). Nothing with hot reload
    /// off.
    pub(crate) fn changed(&mut self, path: &Path) {
        if self.hot_reload.is_none() {
            return;
        }
        let key = path_key(path);
        let now = Instant::now();
        for t in self
            .types
            .iter_mut()
            .filter(|t| t.dependencies.contains(&key))
        {
            t.reload = t.reload.changed(now);
        }
    }

    /// Changed files: their types reload once the files are quiet.
    fn watch_files(&mut self, device: &Device) {
        let Some(hot) = &self.hot_reload else {
            return;
        };
        let now = Instant::now();
        while let Some(event) = hot.watch.try_next() {
            let key = path_key(&event.path);
            for t in &mut self.types {
                if event.kind == FileEventKind::Rescan || t.dependencies.contains(&key) {
                    t.reload = t.reload.changed(now);
                }
            }
        }
        let int64 = device.adapter().caps.shader_int64;
        let mut reflect = Vec::new();
        for (k, t) in self.types.iter_mut().enumerate() {
            let Reload::Changed(at) = t.reload else {
                continue;
            };
            if now.duration_since(at) < RELOAD_QUIET {
                continue;
            }
            // The entry points it is drawn with compile again; the others are forgotten
            // (compiled from the new source when first drawn).
            let mut entries: Vec<CString> = t
                .pipelines
                .values()
                .filter_map(|d| match d {
                    Derived::Waiting { entry, .. } => Some(entry.clone()),
                    Derived::Ready { role, .. } | Derived::Failed { role } => {
                        role.fragment_entry().map(CString::from)
                    }
                })
                .collect();
            entries.sort();
            entries.dedup();
            if entries.is_empty() || !int64 {
                t.spirv.retain(|_, s| matches!(s, Spirv::Compiling));
                t.reload = Reload::Idle;
                // Nothing drawn with it, its parameters reflected again all the same: a
                // type whose module never compiled (its materials never made, so never
                // drawn) is fixed by the edit (E10-3g-1).
                if !matches!(t.params, Params::NotAsked) {
                    reflect.push(MaterialTypeId(k as u16 + 1));
                }
                continue;
            }
            if self.compiler.is_none() {
                self.compiler = CompilerThread::start(self.shader_dir.clone());
            }
            let generation = t.generation.wrapping_add(1);
            let sent = self.compiler.as_ref().is_some_and(|c| {
                c.jobs
                    .send(Job::Reload {
                        ty: MaterialTypeId(k as u16 + 1),
                        generation,
                        entries,
                        module: t.desc.module.clone(),
                        type_name: t.desc.type_name.clone(),
                    })
                    .is_ok()
            });
            if sent {
                t.generation = generation;
                t.reload = Reload::Compiling {
                    generation,
                    again: false,
                };
                self.compiling += 1;
                // Its parameters may have changed too: reflected again, if they were asked
                // for (the previous layout stands until the new one arrives).
                if !matches!(t.params, Params::NotAsked) {
                    reflect.push(MaterialTypeId(k as u16 + 1));
                }
            } else {
                t.reload = Reload::Idle;
            }
        }
        for ty in reflect {
            if self.send_params(ty)
                && let Some(t) = self.types.get_mut(usize::from(ty.0) - 1)
                && matches!(t.params, Params::Failed(_))
            {
                t.params = Params::Asked;
            }
        }
    }

    /// Previous versions whose successors exist: retired, destroyed after this frame.
    fn retire(&mut self) {
        let frame = self.frame;
        for t in &mut self.types {
            for d in t.pipelines.values_mut() {
                if let Derived::Ready {
                    derived, previous, ..
                } = d
                    && previous.is_some()
                    && derived.get().is_some()
                    && let Some(old) = previous.take()
                {
                    self.retired.push((frame, old));
                }
            }
        }
    }

    /// The files a type's compile read, watched from now on.
    fn add_dependencies(&mut self, index: usize, files: &[PathBuf]) {
        let Some(t) = self.types.get_mut(index) else {
            return;
        };
        for file in files {
            let key = path_key(file);
            if !t.dependencies.contains(&key) {
                if let Some(hot) = &mut self.hot_reload {
                    hot.watch(&key);
                }
                t.dependencies.push(key);
            }
        }
    }

    /// Compiled entry points (or errors) from the compiler thread.
    fn accept(&mut self, done: Done, device: &Device) {
        self.compiling = self.compiling.saturating_sub(1);
        match done {
            Done::Entry { ty, entry, result } => {
                let index = usize::from(ty.0) - 1;
                let state = match result {
                    Ok((mut spirv, dependencies)) => {
                        self.add_dependencies(index, &dependencies);
                        match spirv.pop() {
                            Some((_, code)) => Spirv::Ready(code),
                            None => Spirv::Failed,
                        }
                    }
                    Err(e) => {
                        if let Some(t) = self.types.get_mut(index) {
                            log::error!(
                                "material type {}: {} did not compile: {e}",
                                t.desc.name,
                                entry.to_string_lossy()
                            );
                            t.error = Some(e);
                        }
                        Spirv::Failed
                    }
                };
                if let Some(t) = self.types.get_mut(index) {
                    // A reload may have forgotten it meanwhile: still the entry's code.
                    t.spirv.insert(entry, state);
                }
            }
            Done::Reload {
                ty,
                generation,
                result,
            } => {
                let index = usize::from(ty.0) - 1;
                let hot = self.hot_reload.is_some();
                let Some(t) = self.types.get_mut(index) else {
                    return;
                };
                if t.generation != generation {
                    return;
                }
                // Changed again meanwhile: due again, while files are watched.
                let again = hot && matches!(t.reload, Reload::Compiling { again: true, .. });
                t.reload = if again {
                    Reload::Changed(Instant::now())
                } else {
                    Reload::Idle
                };
                let (spirv, dependencies) = match result {
                    Ok(compiled) => compiled,
                    Err(e) => {
                        log::error!(
                            "material type {}: did not compile, the previous version draws: {e}",
                            t.desc.name
                        );
                        t.error = Some(e);
                        return;
                    }
                };
                t.error = None;
                t.spirv.retain(|_, s| matches!(s, Spirv::Compiling));
                for (name, code) in spirv {
                    t.spirv.insert(name, Spirv::Ready(code));
                }
                for d in t.pipelines.values_mut() {
                    let (role, current, previous) = match d {
                        Derived::Ready {
                            role,
                            derived,
                            previous,
                        } => (role, Some(derived), previous.take()),
                        Derived::Failed { role } => (role, None, None),
                        Derived::Waiting { .. } => continue,
                    };
                    let code = role.fragment_entry().map(CString::from).and_then(|name| {
                        match t.spirv.get(&name) {
                            Some(Spirv::Ready(code)) => Some(Arc::clone(code)),
                            _ => None,
                        }
                    });
                    let Some(made) = code.and_then(|code| role.with_fragment(device, code)) else {
                        if let Derived::Ready { previous: p, .. } = d {
                            *p = previous;
                        }
                        continue;
                    };
                    // What draws until the new one exists: the current version if it was
                    // made, otherwise the one before it.
                    let keep = match current {
                        Some(current) if current.get().is_some() => Some(current.clone()),
                        _ => previous,
                    };
                    *d = Derived::Ready {
                        role: role.clone(),
                        derived: made,
                        previous: keep,
                    };
                }
                self.add_dependencies(index, &dependencies);
            }
            Done::Params {
                ty,
                generation,
                result,
            } => {
                let Some(t) = self.types.get_mut(usize::from(ty.0) - 1) else {
                    return;
                };
                if t.params_generation != generation {
                    return;
                }
                match result {
                    Ok(layout) => t.params = Params::Ready(layout),
                    Err(e) => {
                        log::error!("material type {}: its parameters: {e}", t.desc.name);
                        // A reload's failure keeps the layout the type had.
                        if matches!(t.params, Params::Ready(_)) {
                            return;
                        }
                        t.params = Params::Failed(e);
                    }
                }
                t.params_revision = t.params_revision.wrapping_add(1);
            }
        }
    }

    /// Describes the pipelines whose SPIR-V arrived for the warm-up thread.
    fn describe(&mut self, device: &Device) {
        if self.waiting == 0 {
            return;
        }
        for entry in &mut self.types {
            for derived in entry.pipelines.values_mut() {
                let Derived::Waiting { role, entry: name } = derived else {
                    continue;
                };
                let next = match entry.spirv.get(name.as_c_str()) {
                    Some(Spirv::Ready(spirv)) => {
                        match role.with_fragment(device, Arc::clone(spirv)) {
                            Some(made) => Derived::Ready {
                                role: role.clone(),
                                derived: made,
                                previous: None,
                            },
                            None => Derived::Failed { role: role.clone() },
                        }
                    }
                    Some(Spirv::Failed) | None => Derived::Failed { role: role.clone() },
                    Some(Spirv::Compiling) => continue,
                };
                *derived = next;
                self.waiting -= 1;
            }
        }
    }

    /// Blocks until [`Self::pending`] is 0, or `timeout` passes (then `false`): on the
    /// compiler's results, then on the warm-up thread. For tests and tools.
    pub(crate) fn wait(&mut self, device: &Device, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.update(device);
            if self.pending() == 0 {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            if self.compiling > 0 {
                let received = match &self.compiler {
                    Some(compiler) => compiler.done.recv_timeout(left),
                    None => Err(RecvTimeoutError::Disconnected),
                };
                match received {
                    Ok(done) => {
                        self.accept(done, device);
                        self.describe(device);
                    }
                    Err(RecvTimeoutError::Timeout) => return false,
                    // The compiler thread is gone: nothing more will arrive.
                    Err(RecvTimeoutError::Disconnected) => return false,
                }
            } else {
                device.wait_for_pipeline_warm_up();
            }
        }
    }

    /// Pipelines still on their way: waiting for SPIR-V, or described and not created yet,
    /// compiles in flight, and reloads due (a file a type reads changed, its quiet not
    /// over; tests and tools wait for 0).
    pub(crate) fn pending(&self) -> usize {
        let not_created = self
            .types
            .iter()
            .flat_map(|t| t.pipelines.values())
            .filter(|d| matches!(d, Derived::Ready { derived, .. } if !derived.is_settled()))
            .count();
        let due = self
            .types
            .iter()
            .filter(|t| matches!(t.reload, Reload::Changed(_)))
            .count();
        self.waiting
            + not_created
            + due
            + self.compiling
            + self.requests.borrow().len()
            + self.params_requests.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E10-3g-1: a module written by the caller makes the types compiling from it due at
    /// once (counted pending before any watcher's event), not the others; with hot reload
    /// off, nothing.
    #[test]
    fn a_module_written_is_due_at_once() {
        let dir =
            std::env::temp_dir().join(format!("material_types_changed_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (one, two) = (dir.join("one.slang"), dir.join("two.slang"));
        std::fs::write(&one, "").unwrap();
        std::fs::write(&two, "").unwrap();
        let mut types = MaterialTypes::new();
        let desc = |module: &Path| MaterialTypeDesc {
            name: String::from("T"),
            module: module.to_owned(),
            type_name: String::from("T"),
        };
        types.register(desc(&one));
        types.register(desc(&two));
        types.changed(&one);
        assert_eq!(types.pending(), 0, "hot reload off");
        types.set_hot_reload(true);
        types.changed(&one);
        assert_eq!(types.pending(), 1);
        assert!(matches!(types.types[0].reload, Reload::Changed(_)));
        assert_eq!(types.types[1].reload, Reload::Idle);
        // Compiling: compiled again after.
        types.types[1].reload = Reload::Compiling {
            generation: 1,
            again: false,
        };
        types.changed(&two);
        assert_eq!(
            types.types[1].reload,
            Reload::Compiling {
                generation: 1,
                again: true
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
