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

use crate::material::MaterialTypeId;
use engine_rhi::{Device, LazyPipeline, Pipeline};
use engine_shader::{CompileOptions, Compiler, EntryPoint, SlangLibrary, Stage};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
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

/// A type's pipeline standing in for one of the engine's.
enum Derived {
    /// Waiting for the type's SPIR-V of the role's entry point.
    Waiting { role: LazyPipeline, entry: CString },
    /// Described for the warm-up thread (drawn with once it exists). The role is kept so
    /// that its id stays unique while the entry exists.
    Ready {
        _role: LazyPipeline,
        derived: LazyPipeline,
    },
    /// The entry point did not compile (logged); the fallback draws.
    Failed { _role: LazyPipeline },
}

enum Spirv {
    Compiling,
    Ready(Arc<[u32]>),
    Failed,
}

struct TypeEntry {
    desc: MaterialTypeDesc,
    /// By the role's id.
    pipelines: HashMap<usize, Derived>,
    /// By entry point.
    spirv: HashMap<CString, Spirv>,
}

struct Job {
    ty: MaterialTypeId,
    entry: CString,
    module: PathBuf,
    type_name: String,
}

struct Done {
    ty: MaterialTypeId,
    entry: CString,
    spirv: Result<Arc<[u32]>, String>,
}

/// The thread that compiles types' entry points.
struct CompilerThread {
    jobs: Sender<Job>,
    done: Receiver<Done>,
    _thread: std::thread::JoinHandle<()>,
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
                let source = shader_dir.join("forward.slang");
                while let Ok(job) = job_rx.recv() {
                    let spirv = match compiler.as_mut() {
                        None => Err(String::from("no Slang compiler")),
                        Some(compiler) => {
                            let module = job.module.to_string_lossy().replace('\\', "/");
                            let options = CompileOptions {
                                optimize: true,
                                debug_info: false,
                                defines: vec![
                                    (String::from("MATERIAL"), job.type_name.clone()),
                                    (String::from("MATERIAL_FILE"), format!("\"{module}\"")),
                                ],
                                include_dirs: vec![shader_dir.clone()],
                            };
                            let entry =
                                EntryPoint::new(&job.entry.to_string_lossy(), Stage::Fragment);
                            compiler
                                .compile_file(&source, core::slice::from_ref(&entry), &options)
                                .map_err(|e| e.to_string())
                                .and_then(|out| {
                                    out.entries
                                        .into_iter()
                                        .next()
                                        .map(|e| Arc::from(e.spirv))
                                        .ok_or_else(|| String::from("no entry point compiled"))
                                })
                        }
                    };
                    if done_tx
                        .send(Done {
                            ty: job.ty,
                            entry: job.entry,
                            spirv,
                        })
                        .is_err()
                    {
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

/// The registered material types and their pipelines (see the module documentation).
pub(crate) struct MaterialTypes {
    types: Vec<TypeEntry>,
    /// Roles frames drew types with and found no pipeline for: handled by [`Self::update`].
    requests: RefCell<Vec<(MaterialTypeId, LazyPipeline)>>,
    compiler: Option<CompilerThread>,
    shader_dir: PathBuf,
    /// Pipelines waiting for SPIR-V, and entry points compiling.
    waiting: usize,
    compiling: usize,
}

impl core::fmt::Debug for MaterialTypes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MaterialTypes")
            .field("types", &self.types.len())
            .field("waiting", &self.waiting)
            .field("compiling", &self.compiling)
            .finish_non_exhaustive()
    }
}

impl MaterialTypes {
    pub(crate) fn new() -> Self {
        Self {
            types: Vec::new(),
            requests: RefCell::new(Vec::new()),
            compiler: None,
            shader_dir: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/shaders")),
            waiting: 0,
            compiling: 0,
        }
    }

    /// Where `forward.slang` and its modules are (the engine's own shader folder by
    /// default).
    pub(crate) fn set_shader_dir(&mut self, dir: PathBuf) {
        self.shader_dir = dir;
    }

    pub(crate) fn register(&mut self, desc: MaterialTypeDesc) -> MaterialTypeId {
        self.types.push(TypeEntry {
            desc,
            pipelines: HashMap::new(),
            spirv: HashMap::new(),
        });
        MaterialTypeId(self.types.len() as u16)
    }

    pub(crate) fn desc(&self, ty: MaterialTypeId) -> Option<&MaterialTypeDesc> {
        let index = usize::from(ty.0).checked_sub(1)?;
        self.types.get(index).map(|t| &t.desc)
    }

    /// `ty`'s pipeline standing in for `role`, once created; otherwise `None` (the role's
    /// own pipeline draws, with the glTF fallback) and, the first time, a request.
    pub(crate) fn resolve(&self, ty: MaterialTypeId, role: &LazyPipeline) -> Option<&Pipeline> {
        let index = usize::from(ty.0).checked_sub(1)?;
        let entry = self.types.get(index)?;
        match entry.pipelines.get(&role.id()) {
            Some(Derived::Ready { derived, .. }) => derived.get(),
            Some(_) => None,
            None => {
                self.requests.borrow_mut().push((ty, role.clone()));
                None
            }
        }
    }

    /// Requests, compiled entry points and pipelines to describe; once per frame, before
    /// the frame's passes are declared.
    pub(crate) fn update(&mut self, device: &Device) {
        let requests = core::mem::take(&mut *self.requests.borrow_mut());
        let mut changed = false;
        for (ty, role) in requests {
            let Some(entry_name) = role.fragment_entry().map(CString::from) else {
                continue;
            };
            let Some(entry) = self.types.get_mut(usize::from(ty.0) - 1) else {
                continue;
            };
            if entry.pipelines.contains_key(&role.id()) {
                continue;
            }
            if !entry.spirv.contains_key(&entry_name) && !device.adapter().caps.shader_int64 {
                // Types read their parameters at 64-bit addresses (`material_params`).
                if entry.spirv.is_empty() {
                    log::warn!(
                        "material type {}: the GPU has no 64-bit shader integers; its fallback draws",
                        entry.desc.name
                    );
                }
                entry.spirv.insert(entry_name.clone(), Spirv::Failed);
            }
            if !entry.spirv.contains_key(&entry_name) {
                if self.compiler.is_none() {
                    self.compiler = CompilerThread::start(self.shader_dir.clone());
                }
                let sent = self.compiler.as_ref().is_some_and(|c| {
                    c.jobs
                        .send(Job {
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
            self.accept(done);
            changed = true;
        }
        if changed {
            self.describe(device);
        }
    }

    /// A compiled entry point (or its error) from the compiler thread.
    fn accept(&mut self, done: Done) {
        self.compiling = self.compiling.saturating_sub(1);
        let Some(entry) = self.types.get_mut(usize::from(done.ty.0) - 1) else {
            return;
        };
        let state = match done.spirv {
            Ok(spirv) => Spirv::Ready(spirv),
            Err(e) => {
                log::error!(
                    "material type {}: {} did not compile: {e}",
                    entry.desc.name,
                    done.entry.to_string_lossy()
                );
                Spirv::Failed
            }
        };
        entry.spirv.insert(done.entry, state);
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
                                _role: role.clone(),
                                derived: made,
                            },
                            None => Derived::Failed {
                                _role: role.clone(),
                            },
                        }
                    }
                    Some(Spirv::Failed) | None => Derived::Failed {
                        _role: role.clone(),
                    },
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
                        self.accept(done);
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

    /// Pipelines still on their way: waiting for SPIR-V, or described and not created yet
    /// (tests and tools wait for 0).
    pub(crate) fn pending(&self) -> usize {
        let not_created = self
            .types
            .iter()
            .flat_map(|t| t.pipelines.values())
            .filter(|d| matches!(d, Derived::Ready { derived, .. } if !derived.is_settled()))
            .count();
        self.waiting + not_created + self.requests.borrow().len()
    }
}
