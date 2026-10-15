//! Pipelines described up front and created the first time they are needed, and the
//! device's thread that creates the others in the background (ADR 0079).
//!
//! A renderer describes every pipeline it may draw with when it is made, which costs a
//! description, not a compile. The driver compiles one when a frame first needs it
//! ([`LazyPipeline::create_all`], [`LazyPipeline::get_or_create`]) or when the device's
//! warm-up thread reaches it, whichever comes first: a first frame waits only for the
//! pipelines it draws with, and a variant a later frame needs for the first time has
//! been made by then.

use crate::device::Device;
use crate::error::RhiError;
use crate::pipeline::{
    GraphicsPipelineDesc, Pipeline, PipelineContext, PipelineDesc, ShaderStageDesc,
    SharedPipelineLayout,
};
use ash::vk;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

/// A pipeline described up front and created the first time it is needed: by a thread
/// that draws with it, or by its device's warm-up thread, whichever comes first. A
/// thread that needs it while another creates it waits for that one. Clones share the
/// pipeline. Like every object made from a device, it is dropped before the device.
#[derive(Clone)]
pub struct LazyPipeline(Arc<Slot>);

impl core::fmt::Debug for LazyPipeline {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LazyPipeline")
            .field("created", &self.get().map(Pipeline::raw))
            .finish_non_exhaustive()
    }
}

struct Slot {
    context: PipelineContext,
    /// Kept alive until the pipeline is made (or the description dropped).
    layout: SharedPipelineLayout,
    recipe: Recipe,
    /// Set once: the pipeline, or why it could not be made (kept for every later request).
    created: OnceLock<Result<Pipeline, Failure>>,
}

#[derive(Debug, Clone, Copy)]
struct Failure {
    call: &'static str,
    result: vk::Result,
}

impl From<RhiError> for Failure {
    /// Logged here, once per pipeline: later requests return the failure kept.
    fn from(error: RhiError) -> Self {
        log::error!("a pipeline described for first use could not be made: {error}");
        match error {
            RhiError::Vulkan { call, result } => Self { call, result },
            _ => Self {
                call: "pipeline creation",
                result: vk::Result::ERROR_UNKNOWN,
            },
        }
    }
}

impl From<Failure> for RhiError {
    fn from(failure: Failure) -> Self {
        Self::Vulkan {
            call: failure.call,
            result: failure.result,
        }
    }
}

/// An owned [`PipelineDesc`].
enum Recipe {
    Graphics {
        /// The fixed-function state; its stages, formats and constants are the fields below.
        state: GraphicsPipelineDesc<'static>,
        vertex: Stage,
        fragment: Option<Stage>,
        color_formats: Box<[vk::Format]>,
        specialization: Box<[u32]>,
    },
    Compute {
        stage: Stage,
        specialization: Box<[u32]>,
    },
}

struct Stage {
    /// Shared by the pipelines described together that use the module.
    spirv: Arc<[u32]>,
    entry: CString,
}

impl Stage {
    fn desc(&self) -> ShaderStageDesc<'_> {
        ShaderStageDesc {
            spirv: &self.spirv,
            entry: &self.entry,
        }
    }
}

/// The SPIR-V of pipelines described together, one copy per module.
#[derive(Default)]
struct Modules(HashMap<(usize, usize), Arc<[u32]>>);

impl Modules {
    fn stage(&mut self, stage: &ShaderStageDesc<'_>) -> Stage {
        let key = (stage.spirv.as_ptr() as usize, stage.spirv.len());
        let spirv = Arc::clone(self.0.entry(key).or_insert_with(|| Arc::from(stage.spirv)));
        Stage {
            spirv,
            entry: stage.entry.to_owned(),
        }
    }
}

const NO_STAGE: ShaderStageDesc<'static> = ShaderStageDesc {
    spirv: &[],
    entry: c"",
};

impl Recipe {
    fn new(modules: &mut Modules, desc: &PipelineDesc<'_>) -> Self {
        match desc {
            PipelineDesc::Graphics(desc) => Self::Graphics {
                // Field by field: a field added to the description must be carried over
                // here too.
                state: GraphicsPipelineDesc {
                    vertex: NO_STAGE,
                    fragment: None,
                    color_formats: &[],
                    depth_format: desc.depth_format,
                    topology: desc.topology,
                    cull_mode: desc.cull_mode,
                    front_face: desc.front_face,
                    dynamic_culling: desc.dynamic_culling,
                    depth_test: desc.depth_test,
                    depth_write: desc.depth_write,
                    depth_compare: desc.depth_compare,
                    blend: desc.blend,
                    depth_bias: desc.depth_bias,
                    samples: desc.samples,
                    alpha_to_coverage: desc.alpha_to_coverage,
                    specialization: &[],
                    viewports: desc.viewports,
                },
                vertex: modules.stage(&desc.vertex),
                fragment: desc.fragment.as_ref().map(|f| modules.stage(f)),
                color_formats: desc.color_formats.into(),
                specialization: desc.specialization.into(),
            },
            PipelineDesc::Compute {
                stage,
                specialization,
            } => Self::Compute {
                stage: modules.stage(stage),
                specialization: (*specialization).into(),
            },
        }
    }

    fn desc(&self) -> PipelineDesc<'_> {
        match self {
            Self::Graphics {
                state,
                vertex,
                fragment,
                color_formats,
                specialization,
            } => PipelineDesc::Graphics(GraphicsPipelineDesc {
                vertex: vertex.desc(),
                fragment: fragment.as_ref().map(Stage::desc),
                color_formats,
                specialization,
                ..*state
            }),
            Self::Compute {
                stage,
                specialization,
            } => PipelineDesc::Compute {
                stage: stage.desc(),
                specialization,
            },
        }
    }
}

impl Slot {
    fn create(&self, cached_only: bool) -> Result<Option<Pipeline>, RhiError> {
        Pipeline::create(
            &self.context,
            self.layout.raw(),
            &self.recipe.desc(),
            cached_only,
        )
    }
}

impl LazyPipeline {
    /// Describes a pipeline of `device` with `layout` (normally
    /// [`crate::Bindless::shared_pipeline_layout`]) without creating it, and queues it for
    /// the device's warm-up ([`Device::set_pipeline_warm_up`]).
    pub fn describe(
        device: &Device,
        layout: &SharedPipelineLayout,
        desc: &PipelineDesc<'_>,
    ) -> Self {
        Self::describe_many(device, layout, core::slice::from_ref(desc))
            .pop()
            .unwrap_or_else(|| unreachable!("one pipeline described"))
    }

    /// Describes pipelines of `device` with `layout` without creating any, in order, and
    /// queues them for the device's warm-up. A shader module several of them use is kept
    /// once.
    pub fn describe_many(
        device: &Device,
        layout: &SharedPipelineLayout,
        descs: &[PipelineDesc<'_>],
    ) -> Vec<Self> {
        let context = device.pipeline_context();
        let mut modules = Modules::default();
        let described: Vec<Self> = descs
            .iter()
            .map(|desc| {
                Self(Arc::new(Slot {
                    context: context.clone(),
                    layout: layout.clone(),
                    recipe: Recipe::new(&mut modules, desc),
                    created: OnceLock::new(),
                }))
            })
            .collect();
        if let Some(warm_up) = device.pipeline_warm_up() {
            warm_up.add(&described);
        }
        described
    }

    /// A number naming this pipeline (its clones share it) for as long as it is alive:
    /// what a renderer keys pipelines derived from it by ([`Self::with_fragment`]).
    pub fn id(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    /// The entry point of its fragment stage, if it has one.
    pub fn fragment_entry(&self) -> Option<&core::ffi::CStr> {
        match &self.0.recipe {
            Recipe::Graphics { fragment, .. } => fragment.as_ref().map(|f| f.entry.as_c_str()),
            Recipe::Compute { .. } => None,
        }
    }

    /// The same pipeline with another fragment module (`spirv`, the same entry point):
    /// described, not created, and queued for `device`'s warm-up thread, which creates it
    /// in the background (a material type's variant of an engine pipeline, ADR 0092).
    /// Without the warm-up thread ([`Device::set_pipeline_warm_up`]) it is created now:
    /// such a variant is described when a frame first needs it, and nothing else would
    /// make it. `None` for a compute pipeline or one without a fragment stage.
    pub fn with_fragment(&self, device: &Device, spirv: Arc<[u32]>) -> Option<Self> {
        self.derive_fragment(device, spirv, None)
    }

    /// [`Self::with_fragment`] with another entry point too: `entry` in `spirv` (a
    /// feature's variant of an engine pipeline, such as ambient occlusion's bent normals).
    pub fn with_fragment_entry(
        &self,
        device: &Device,
        spirv: Arc<[u32]>,
        entry: &core::ffi::CStr,
    ) -> Option<Self> {
        self.derive_fragment(device, spirv, Some(entry))
    }

    fn derive_fragment(
        &self,
        device: &Device,
        spirv: Arc<[u32]>,
        entry: Option<&core::ffi::CStr>,
    ) -> Option<Self> {
        let Recipe::Graphics {
            state,
            vertex,
            fragment: Some(fragment),
            color_formats,
            specialization,
        } = &self.0.recipe
        else {
            return None;
        };
        let derived = Self(Arc::new(Slot {
            context: self.0.context.clone(),
            layout: self.0.layout.clone(),
            recipe: Recipe::Graphics {
                state: *state,
                vertex: Stage {
                    spirv: Arc::clone(&vertex.spirv),
                    entry: vertex.entry.clone(),
                },
                fragment: Some(Stage {
                    spirv,
                    entry: entry.map_or_else(|| fragment.entry.clone(), CString::from),
                }),
                color_formats: color_formats.clone(),
                specialization: specialization.clone(),
            },
            created: OnceLock::new(),
        }));
        if let Some(warm_up) = device.pipeline_warm_up() {
            warm_up.add(core::slice::from_ref(&derived));
        } else {
            // A failure is logged and kept by the pipeline (settled, never drawn with).
            let _ = derived.get_or_create();
        }
        Some(derived)
    }

    /// The pipeline, once created.
    pub fn get(&self) -> Option<&Pipeline> {
        self.0.created.get().and_then(|r| r.as_ref().ok())
    }

    /// Whether creating it was finished: it exists, or it failed.
    pub fn is_settled(&self) -> bool {
        self.0.created.get().is_some()
    }

    /// Whether `self` and `other` are the same pipeline (clones of one description).
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The pipeline, created now on this thread if nobody has; if another thread is
    /// creating it, waits for that. A failure is kept: every later call returns it.
    ///
    /// The pipeline cache is asked first (ADR 0055): on NVIDIA a pipeline it holds comes
    /// back in 0.1 ms that way, against 1.1 ms through a plain create.
    pub fn get_or_create(&self) -> Result<&Pipeline, RhiError> {
        self.0
            .created
            .get_or_init(|| {
                let created = match self.0.create(true) {
                    Ok(None) => self.0.create(false),
                    found => found,
                };
                created
                    .and_then(|created| {
                        created.ok_or(RhiError::Vulkan {
                            call: "vkCreate*Pipelines",
                            result: vk::Result::PIPELINE_COMPILE_REQUIRED,
                        })
                    })
                    .map_err(Failure::from)
            })
            .as_ref()
            .map_err(|failure| RhiError::from(*failure))
    }

    /// Creates those of `pipelines` nobody has created yet, as
    /// [`Pipeline::create_many`] does (ADR 0055): those the pipeline cache holds one after
    /// another on this thread (drivers serialize lookups), the others compiled in
    /// parallel. Returns the first failure among `pipelines`. Repeats and pipelines another
    /// thread is creating are fine: each is created once.
    pub fn create_all(pipelines: &[&Self]) -> Result<(), RhiError> {
        if pipelines.iter().all(|p| p.is_settled()) {
            return pipelines
                .iter()
                .try_for_each(|p| p.get_or_create().map(|_| ()));
        }
        engine_profiler::zone!("rhi::LazyPipeline::create_all");
        let mut compile: Vec<&Self> = Vec::new();
        for (i, &pipeline) in pipelines.iter().enumerate() {
            if pipeline.is_settled() || pipelines[..i].iter().any(|p| p.same(pipeline)) {
                continue;
            }
            match pipeline.0.create(true) {
                // Another thread may have finished it meanwhile: then this one is dropped.
                Ok(Some(created)) => {
                    let _ = pipeline.0.created.set(Ok(created));
                }
                Ok(None) => compile.push(pipeline),
                Err(error) => {
                    let _ = pipeline.0.created.set(Err(Failure::from(error)));
                }
            }
        }
        if let Some(first) = compile.first() {
            first
                .0
                .context
                .create_parallel(compile.len(), |k| compile[k].get_or_create().map(|_| ()))?;
        }
        pipelines
            .iter()
            .try_for_each(|p| p.get_or_create().map(|_| ()))
    }
}

/// The device's thread that creates [`LazyPipeline`]s in the background, one after
/// another below normal priority (frames come first), in the order they were described:
/// a variant a frame has not drawn with yet is ready before one does (ADR 0079). One a
/// frame needs while the thread creates it is waited for; one it needs before the
/// thread reaches it is created by the frame; one whose descriptions were all dropped is
/// skipped. Dropping it stops the thread after the pipeline it is creating.
pub(crate) struct PipelineWarmUp {
    shared: Arc<WarmUp>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct WarmUp {
    state: Mutex<WarmUpState>,
    /// Wakes the thread: pipelines added, or stop.
    wake: Condvar,
    /// Wakes [`PipelineWarmUp::wait`]: the queue ran empty.
    idle: Condvar,
}

#[derive(Default)]
struct WarmUpState {
    queue: VecDeque<Weak<Slot>>,
    /// The thread is creating one it took from the queue.
    creating: bool,
    stop: bool,
}

impl WarmUp {
    fn lock(&self) -> MutexGuard<'_, WarmUpState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PipelineWarmUp {
    /// Starts the thread.
    pub(crate) fn start() -> Result<Self, RhiError> {
        let shared = Arc::new(WarmUp {
            state: Mutex::new(WarmUpState::default()),
            wake: Condvar::new(),
            idle: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name(String::from("engine-pipeline-warm-up"))
            .spawn(move || warm_up(&worker))
            .map_err(|error| RhiError::Io {
                what: "starting the pipeline warm-up thread",
                error,
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Queues those of `pipelines` not created yet, after the pipelines queued before.
    pub(crate) fn add(&self, pipelines: &[LazyPipeline]) {
        let mut state = self.shared.lock();
        let before = state.queue.len();
        state.queue.extend(
            pipelines
                .iter()
                .filter(|p| !p.is_settled())
                .map(|p| Arc::downgrade(&p.0)),
        );
        if state.queue.len() > before {
            self.shared.wake.notify_one();
        }
    }

    /// Pipelines queued and not created yet, the one being created included.
    pub(crate) fn pending(&self) -> usize {
        let state = self.shared.lock();
        state.queue.len() + usize::from(state.creating)
    }

    /// Waits until every queued pipeline is created.
    pub(crate) fn wait(&self) {
        let mut state = self.shared.lock();
        while !state.stop && (state.creating || !state.queue.is_empty()) {
            state = self
                .shared
                .idle
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for PipelineWarmUp {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The warm-up thread: takes pipelines from the queue until stopped.
fn warm_up(shared: &WarmUp) {
    lower_priority();
    loop {
        let next = {
            let mut state = shared.lock();
            loop {
                if state.stop {
                    return;
                }
                if let Some(next) = state.queue.pop_front() {
                    state.creating = true;
                    break next;
                }
                state.creating = false;
                shared.idle.notify_all();
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        // A failure stays in the pipeline: whoever draws with it reports it.
        if let Some(slot) = next.upgrade() {
            let _ = LazyPipeline(slot).get_or_create();
        }
    }
}

/// The warm-up works below normal priority: frames come first.
fn lower_priority() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // SAFETY: the pseudo-handle of the calling thread and a valid priority.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
}
