//! NVIDIA DLSS Super Resolution and DLAA through NVIDIA Streamline (CLAUDE.md §6, ADR 0063),
//! from DLLs the user supplies: the engine ships and commits no NVIDIA binary.
//!
//! Streamline is process-wide and must be initialized before the Vulkan device exists, so
//! DLSS takes part in device creation:
//!
//! ```text
//! // 1. Before creating the instance: find, verify and load the DLLs.
//! let streamline = Streamline::load(&StreamlineDesc::default()); // Err = no DLSS, no crash
//! let extra_instance = streamline.as_ref().map(|s| s.instance_extensions()).unwrap_or_default();
//! let instance = Instance::with_extensions(&desc, &extra_instance)?;
//! let adapter = instance.select_adapter(&preference)?;
//! // 2. Is DLSS supported on this adapter (an NVIDIA RTX GPU, a recent driver)?
//! let dlss_ok = streamline.as_ref().is_ok_and(|s| s.check_adapter(&instance, &adapter).is_ok());
//! let extra_device = if dlss_ok { streamline.device_extensions() } else { vec![] };
//! let device = Device::with_extensions(&instance, &adapter, &extra_device)?;
//! // 3. Hand the device to Streamline; hook presents so it can collect its garbage.
//! streamline.attach(&device)?;
//! unsafe { swapchain.set_present_function(streamline.present_function()) };
//! // 4. Per view:
//! let mut dlss = Dlss::new(&streamline, frames_in_flight);
//! let render = dlss.render_size(Quality::Quality, output);
//! temporal.set_jitter_phases(Dlss::jitter_phases(render, output));
//! let upscaled = dlss.add_pass(&mut fg, input, output, frame.number)?;
//! ```
//!
//! [`Dlss`] implements `engine_upscale::Upscaler`, so a view can switch between the
//! engine's TAAU, FSR 3.1 and DLSS behind one interface.
//!
//! Drop every [`Dlss`] and the [`Streamline`] before the device: Streamline's shutdown
//! releases Vulkan objects it created on it.

#![deny(unsafe_op_in_unsafe_fn)]

mod framegen;
#[cfg(windows)]
mod loader;
mod sl;

pub use framegen::{
    DlssFrameGeneration, GenerationCamera, GenerationInput, GenerationState, StreamlineLatency,
};

use core::ffi::{CStr, c_char, c_void};
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use engine_render::graph::{Access, FrameGraph, ShaderStages, Texture, TextureDesc};
use engine_rhi::ash::vk;
use engine_rhi::{Adapter, Device, ExtraQueues, Instance, RhiError, VulkanHooks};
use engine_upscale::{Quality, UpscaleError, UpscaleInput, UpscalerKind};
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Format of the upscaled image (HDR), as for the other upscalers.
pub const OUTPUT_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// The NGX project id the engine passes when a game sets none: NVIDIA asks every
/// application without an NVIDIA-assigned id for a GUID of its own
/// ([`StreamlineDesc::project_id`]); games should set theirs.
pub const ENGINE_PROJECT_ID: &str = "8f0a3c5e-6b1d-4e2f-9a7c-0d4b1e6f3a29";

/// Something the player can do to make DLSS frame generation available
/// ([`Streamline::frame_generation_hint`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameGenerationHint {
    /// Turn on Windows' hardware-accelerated GPU scheduling (Settings → System → Display →
    /// Graphics → Change default graphics settings) and restart.
    TurnOnGpuScheduling,
    /// Install a newer NVIDIA driver.
    UpdateDriver,
    /// Update Windows.
    UpdateWindows,
}

impl FrameGenerationHint {
    /// A sentence for a settings menu (English; games translate it).
    pub fn text(self) -> &'static str {
        match self {
            Self::TurnOnGpuScheduling => {
                "DLSS Frame Generation needs hardware-accelerated GPU scheduling: turn it on in                  Windows Settings > System > Display > Graphics, then restart."
            }
            Self::UpdateDriver => "DLSS Frame Generation needs a newer NVIDIA driver.",
            Self::UpdateWindows => "DLSS Frame Generation needs a newer version of Windows.",
        }
    }
}

/// How much Streamline logs (through the engine's callback, [`Streamline::log`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamlineLog {
    /// Nothing.
    Off,
    /// Warnings and errors, and a little information (the default).
    #[default]
    Default,
    /// Everything: for debugging an integration.
    Verbose,
}

/// How to find and start Streamline ([`Streamline::load`]).
#[derive(Debug, Clone)]
pub struct StreamlineDesc {
    /// The folder with the DLLs. `None` searches `ENGINE_STREAMLINE_DIR`, then
    /// `plugins/nvidia/` next to the executable, then the executable's folder.
    pub plugin_dir: Option<PathBuf>,
    /// Check that NVIDIA signed `sl.interposer.dll` (Authenticode, `WinVerifyTrust`)
    /// before loading it; the interposer checks the plugins it loads itself. Only the
    /// unsigned development builds of Streamline need `false`.
    pub verify_signature: bool,
    /// The game's NGX project id, a GUID ([`ENGINE_PROJECT_ID`] by default).
    pub project_id: String,
    /// An application id NVIDIA assigned to the game, 0 for none.
    pub application_id: u32,
    /// Streamline's log level.
    pub log: StreamlineLog,
    /// A folder for Streamline's log file; `None` writes no file.
    pub log_dir: Option<PathBuf>,
    /// Load DLSS frame generation too (ADR 0091), with Reflex and the PC latency markers it
    /// requires, when their DLLs are there ([`Streamline::frame_generation`] says whether
    /// they loaded). Streamline then takes over presentation: the surface and the
    /// swapchain go through its hooks ([`Streamline::vulkan_hooks`]), and the device has
    /// its queues ([`Streamline::extra_queues`]). Decide it before the device, for the
    /// whole session.
    pub frame_generation: bool,
}

impl Default for StreamlineDesc {
    fn default() -> Self {
        Self {
            plugin_dir: None,
            verify_signature: true,
            project_id: String::from(ENGINE_PROJECT_ID),
            application_id: 0,
            log: StreamlineLog::Default,
            log_dir: None,
            frame_generation: false,
        }
    }
}

/// Why DLSS is unavailable, or a Streamline call failed. None of these is fatal: the game
/// hides the option and uses another upscaler.
#[derive(Debug)]
pub enum DlssError {
    /// No folder with `sl.interposer.dll` was found.
    NotFound {
        /// Where it was looked for.
        searched: Vec<PathBuf>,
    },
    /// The interposer was found without the other files DLSS needs.
    MissingFiles {
        /// The folder.
        dir: PathBuf,
        /// The missing file names.
        missing: Vec<&'static str>,
    },
    /// A DLL is not signed by NVIDIA.
    Unsigned {
        /// The file.
        file: PathBuf,
        /// What the check found.
        reason: String,
    },
    /// Windows could not load a DLL.
    Load {
        /// The file.
        file: PathBuf,
        /// `GetLastError`.
        code: u32,
    },
    /// The interposer lacks a function this integration uses (an incompatible version).
    MissingExport(&'static CStr),
    /// Another [`Streamline`] is alive: Streamline is process-wide.
    AlreadyLoaded,
    /// This platform has no Streamline (not Windows).
    UnsupportedPlatform,
    /// A Streamline call failed.
    Streamline {
        /// The call.
        call: &'static str,
        /// The `sl::Result`.
        result: i32,
    },
    /// Streamline asks for something the engine's device does not have.
    MissingRequirement(String),
    /// No device was given to Streamline yet ([`Streamline::attach`]).
    NotAttached,
    /// A Vulkan error.
    Rhi(RhiError),
}

/// The name of an `sl::Result` value.
pub fn result_name(result: i32) -> &'static str {
    usize::try_from(result)
        .ok()
        .and_then(|i| sl::RESULT_NAMES.get(i))
        .copied()
        .unwrap_or("unknown sl::Result")
}

impl fmt::Display for DlssError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { searched } => {
                write!(f, "Streamline DLLs not found (looked in")?;
                for dir in searched {
                    write!(f, " {}", dir.display())?;
                }
                write!(f, ")")
            }
            Self::MissingFiles { dir, missing } => {
                write!(f, "{} lacks {}", dir.display(), missing.join(", "))
            }
            Self::Unsigned { file, reason } => {
                write!(f, "{}: not signed by NVIDIA ({reason})", file.display())
            }
            Self::Load { file, code } => {
                write!(f, "cannot load {}: error {code}", file.display())
            }
            Self::MissingExport(name) => {
                write!(f, "sl.interposer.dll does not export {name:?}")
            }
            Self::AlreadyLoaded => write!(f, "Streamline is already loaded in this process"),
            Self::UnsupportedPlatform => write!(f, "Streamline needs Windows"),
            Self::Streamline { call, result } => {
                write!(f, "{call} failed: {} ({result})", result_name(*result))
            }
            Self::MissingRequirement(what) => write!(f, "DLSS needs {what}"),
            Self::NotAttached => write!(f, "no device was attached to Streamline"),
            Self::Rhi(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DlssError {}

impl From<RhiError> for DlssError {
    fn from(e: RhiError) -> Self {
        Self::Rhi(e)
    }
}

impl From<DlssError> for UpscaleError {
    fn from(e: DlssError) -> Self {
        match e {
            DlssError::Rhi(e) => Self::Rhi(e),
            DlssError::Streamline { call, result } => Self::Library {
                upscaler: UpscalerKind::Dlss,
                call,
                code: result,
            },
            _ => Self::Library {
                upscaler: UpscalerKind::Dlss,
                call: "Streamline",
                code: -1,
            },
        }
    }
}

/// What Streamline logged since it was loaded ([`Streamline::log`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamlineLogReport {
    /// Warnings.
    pub warnings: u32,
    /// Errors.
    pub errors: u32,
    /// The first few errors' text.
    pub first_errors: Vec<String>,
}

static WARNINGS: AtomicU32 = AtomicU32::new(0);
static ERRORS: AtomicU32 = AtomicU32::new(0);
// Streamline logs from its own threads, rarely; never on the engine's hot paths.
static FIRST_ERRORS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Evaluations Streamline refused, and the last refusal's `sl::Result`.
static EVALUATE_FAILURES: AtomicU32 = AtomicU32::new(0);
static LAST_EVALUATE_RESULT: AtomicI32 = AtomicI32::new(0);
/// A [`Streamline`] is alive.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Streamline's log callback (`sl::PFun_LogMessageCallback`).
unsafe extern "C" fn log_message(kind: u32, message: *const c_char) {
    if kind != sl::LOG_WARN && kind != sl::LOG_ERROR {
        return;
    }
    // Borrowed unless the text is not UTF-8: logging allocates only the errors it keeps.
    let text = if message.is_null() {
        std::borrow::Cow::Borrowed("")
    } else {
        // SAFETY: Streamline passes a NUL-terminated string valid for the call.
        unsafe { CStr::from_ptr(message) }.to_string_lossy()
    };
    let text = text.trim_end();
    if kind == sl::LOG_ERROR {
        ERRORS.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut first) = FIRST_ERRORS.lock()
            && first.len() < 8
        {
            first.push(text.to_owned());
        }
    } else {
        WARNINGS.fetch_add(1, Ordering::Relaxed);
    }
    engine_profiler::message(text);
    if cfg!(debug_assertions) {
        #[allow(clippy::print_stderr)] // until the engine has a logger
        {
            eprintln!(
                "[streamline {}] {text}",
                if kind == sl::LOG_ERROR {
                    "error"
                } else {
                    "warning"
                }
            );
        }
    }
}

/// Streamline's functions, resolved from the interposer.
#[derive(Clone, Copy)]
struct Functions {
    init: sl::PfnInit,
    shutdown: sl::PfnShutdown,
    is_feature_supported: sl::PfnIsFeatureSupported,
    get_feature_requirements: sl::PfnGetFeatureRequirements,
    get_feature_version: sl::PfnGetFeatureVersion,
    set_vulkan_info: sl::PfnSetVulkanInfo,
    get_new_frame_token: sl::PfnGetNewFrameToken,
    set_constants: sl::PfnSetConstants,
    evaluate_feature: sl::PfnEvaluateFeature,
    free_resources: sl::PfnFreeResources,
    get_feature_function: sl::PfnGetFeatureFunction,
    set_tag_for_frame: sl::PfnSetTagForFrame,
    set_feature_loaded: sl::PfnSetFeatureLoaded,
    get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
}

/// The functions of DLSS frame generation, Reflex and the PC latency markers
/// (`sl_dlss_g.h`, `sl_reflex.h`, `sl_pcl.h`), once a device is attached.
#[derive(Clone, Copy)]
pub(crate) struct FrameGenerationFunctions {
    pub(crate) set_options: sl::PfnDlssgSetOptions,
    pub(crate) get_state: sl::PfnDlssgGetState,
    pub(crate) reflex_set_options: sl::PfnReflexSetOptions,
    pub(crate) reflex_sleep: sl::PfnReflexSleep,
    pub(crate) pcl_set_marker: sl::PfnPclSetMarker,
}

/// The DLSS plugin's functions (`sl_dlss.h`), available once a device is attached.
#[derive(Clone, Copy)]
struct DlssFunctions {
    get_optimal_settings: sl::PfnDlssGetOptimalSettings,
    get_state: sl::PfnDlssGetState,
    set_options: sl::PfnDlssSetOptions,
}

/// The interposer's functions, loaded once per process (the DLL stays loaded).
#[cfg(windows)]
static LIBRARY: OnceLock<(PathBuf, Functions)> = OnceLock::new();

#[cfg(windows)]
fn load_functions(file: &std::path::Path) -> Result<Functions, DlssError> {
    let library = loader::Library::load(file)?;
    macro_rules! get {
        ($name:literal, $ty:ty) => {{
            let f = library.symbol($name)?;
            // SAFETY: the export of this name has this signature (`sl_core_api.h`,
            // `sl_helpers_vk.h`; `vkGetDeviceProcAddr` is Vulkan's).
            unsafe { core::mem::transmute::<*const (), $ty>(f) }
        }};
    }
    Ok(Functions {
        init: get!(c"slInit", sl::PfnInit),
        shutdown: get!(c"slShutdown", sl::PfnShutdown),
        is_feature_supported: get!(c"slIsFeatureSupported", sl::PfnIsFeatureSupported),
        get_feature_requirements: get!(c"slGetFeatureRequirements", sl::PfnGetFeatureRequirements),
        get_feature_version: get!(c"slGetFeatureVersion", sl::PfnGetFeatureVersion),
        set_vulkan_info: get!(c"slSetVulkanInfo", sl::PfnSetVulkanInfo),
        get_new_frame_token: get!(c"slGetNewFrameToken", sl::PfnGetNewFrameToken),
        set_constants: get!(c"slSetConstants", sl::PfnSetConstants),
        evaluate_feature: get!(c"slEvaluateFeature", sl::PfnEvaluateFeature),
        free_resources: get!(c"slFreeResources", sl::PfnFreeResources),
        get_feature_function: get!(c"slGetFeatureFunction", sl::PfnGetFeatureFunction),
        set_tag_for_frame: get!(c"slSetTagForFrame", sl::PfnSetTagForFrame),
        set_feature_loaded: get!(c"slSetFeatureLoaded", sl::PfnSetFeatureLoaded),
        get_instance_proc_addr: get!(c"vkGetInstanceProcAddr", vk::PFN_vkGetInstanceProcAddr),
        get_device_proc_addr: get!(c"vkGetDeviceProcAddr", vk::PFN_vkGetDeviceProcAddr),
    })
}

/// A Vulkan 1.2 or 1.3 feature Streamline asks for that the engine's device always
/// enables (`engine_rhi::Device`, ADR 0006).
const ENGINE_FEATURES: [&str; 8] = [
    "timelineSemaphore",
    "descriptorIndexing",
    "bufferDeviceAddress",
    "scalarBlockLayout",
    "runtimeDescriptorArray",
    "dynamicRendering",
    "synchronization2",
    "maintenance4",
];

/// `VK_EXT_buffer_device_address`: Streamline lists it for DLSS, but the engine enables
/// the core 1.2 `bufferDeviceAddress` feature, and a device may not have both
/// (`VUID-VkDeviceCreateInfo-pNext-04748`). NGX works with the core feature.
const EXT_BUFFER_DEVICE_ADDRESS: &CStr = c"VK_EXT_buffer_device_address";

/// `VK_NV_optical_flow`: frame generation's native optical flow needs a queue of its own
/// family and the extension's feature; the engine leaves it out, and Streamline runs the
/// optical flow in its "interop" mode instead (Streamline's manual hooking guide §5.2).
const EXT_OPTICAL_FLOW: &CStr = c"VK_NV_optical_flow";

struct Shared {
    functions: Functions,
    dir: PathBuf,
    instance_extensions: Vec<CString>,
    device_extensions: Vec<CString>,
    /// DLSS frame generation, Reflex and PCL were loaded ([`StreamlineDesc::frame_generation`]).
    frame_generation: bool,
    /// The queues frame generation creates itself, after the engine's.
    extra_queues: ExtraQueues,
    /// Set by [`Streamline::attach`].
    attached: OnceLock<Attached>,
}

struct Attached {
    dlss: DlssFunctions,
    present: Option<vk::PFN_vkQueuePresentKHR>,
    /// Frame generation's functions, or the call that refused it and its `sl::Result`
    /// (`None` when it was not loaded).
    frame_generation: Option<Result<FrameGenerationFunctions, (&'static str, i32)>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: Streamline was initialized by `Streamline::load`; every `Dlss` holding
        // this state is gone, and the caller keeps the device alive until now.
        unsafe { (self.functions.shutdown)() };
        ACTIVE.store(false, Ordering::Release);
    }
}

/// Streamline, loaded and initialized for DLSS Super Resolution (see the crate
/// documentation). One per process.
#[derive(Clone)]
pub struct Streamline {
    shared: Arc<Shared>,
}

impl fmt::Debug for Streamline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Streamline")
            .field("dir", &self.shared.dir)
            .field("attached", &self.shared.attached.get().is_some())
            .finish_non_exhaustive()
    }
}

/// Versions of Streamline's DLSS plugin and of NVIDIA's DLSS library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DlssVersions {
    /// `sl.dlss.dll` (Streamline).
    pub streamline: [u32; 3],
    /// `nvngx_dlss.dll` (NGX), zero before a device is attached.
    pub ngx: [u32; 3],
}

impl Streamline {
    /// Finds the DLLs ([`StreamlineDesc::plugin_dir`]), checks NVIDIA's signature, loads
    /// the interposer and initializes Streamline for DLSS on Vulkan with manual hooking.
    /// Call it before creating the Vulkan instance. Any failure means "no DLSS": missing
    /// or unsigned files, no NVIDIA GPU or driver, another instance alive.
    pub fn load(desc: &StreamlineDesc) -> Result<Self, DlssError> {
        engine_profiler::zone!("dlss::Streamline::load");
        #[cfg(not(windows))]
        {
            let _ = desc;
            Err(DlssError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            if ACTIVE.swap(true, Ordering::Acquire) {
                return Err(DlssError::AlreadyLoaded);
            }
            let result = Self::load_inner(desc);
            if result.is_err() {
                ACTIVE.store(false, Ordering::Release);
            }
            result
        }
    }

    #[cfg(windows)]
    fn load_inner(desc: &StreamlineDesc) -> Result<Self, DlssError> {
        let (dir, functions) = match LIBRARY.get() {
            Some((dir, functions)) => (dir.clone(), *functions),
            None => {
                let dir = loader::find(&loader::candidate_dirs(desc.plugin_dir.as_deref()))?;
                let interposer = dir.join(loader::REQUIRED_FILES[0]);
                if desc.verify_signature {
                    loader::verify_signature(&interposer)?;
                }
                let functions = load_functions(&interposer)?;
                let (dir, functions) = LIBRARY.get_or_init(|| (dir, functions));
                (dir.clone(), *functions)
            }
        };

        let plugin_dir = loader::wide(&dir);
        let plugin_dirs = [plugin_dir.as_ptr()];
        let log_dir = desc.log_dir.as_deref().map(loader::wide);
        // Frame generation when asked for and its DLLs are there; it needs Reflex and PCL.
        let frame_generation = desc.frame_generation
            && loader::FRAME_GENERATION_FILES
                .iter()
                .all(|f| dir.join(f).is_file());
        let all_features = [
            sl::FEATURE_DLSS,
            sl::FEATURE_DLSS_G,
            sl::FEATURE_REFLEX,
            sl::FEATURE_PCL,
        ];
        let features: &[sl::Feature] = if frame_generation {
            &all_features
        } else {
            &all_features[..1]
        };
        let project_id = CString::new(desc.project_id.replace('\0', "")).unwrap_or_default();
        let mut preferences = sl::Preferences::new();
        preferences.log_level = match desc.log {
            StreamlineLog::Off => sl::LOG_LEVEL_OFF,
            StreamlineLog::Default => sl::LOG_LEVEL_DEFAULT,
            StreamlineLog::Verbose => sl::LOG_LEVEL_VERBOSE,
        };
        preferences.paths_to_plugins = plugin_dirs.as_ptr();
        preferences.num_paths_to_plugins = 1;
        preferences.path_to_logs_and_data =
            log_dir.as_ref().map_or(core::ptr::null(), |d| d.as_ptr());
        preferences.log_message_callback = Some(log_message);
        // No over-the-air updates: the engine does not download code behind the user's
        // back; the DLLs the user supplied are the ones that run.
        preferences.flags = sl::PREFERENCE_DISABLE_CL_STATE_TRACKING
            | sl::PREFERENCE_DISABLE_DEBUG_TEXT
            | sl::PREFERENCE_USE_MANUAL_HOOKING
            | sl::PREFERENCE_FRAME_BASED_TAGGING;
        preferences.features_to_load = features.as_ptr();
        preferences.num_features_to_load = features.len() as u32;
        preferences.application_id = desc.application_id;
        preferences.engine = sl::ENGINE_CUSTOM;
        preferences.engine_version = c"ofeliaengine 0.0".as_ptr();
        preferences.project_id = project_id.as_ptr();
        preferences.render_api = sl::RENDER_API_VULKAN;
        // SAFETY: every pointer in `preferences` refers to locals that outlive the call
        // (Streamline copies what it keeps).
        let result = unsafe { (functions.init)(&preferences, sl::SDK_VERSION) };
        if result != sl::OK {
            return Err(DlssError::Streamline {
                call: "slInit",
                result,
            });
        }
        // From here on, dropping `shared` shuts Streamline down.
        let mut shared = Shared {
            functions,
            dir,
            instance_extensions: Vec::new(),
            device_extensions: Vec::new(),
            frame_generation,
            extra_queues: ExtraQueues::default(),
            attached: OnceLock::new(),
        };
        let strings = |count: u32, list: *const *const c_char| -> Vec<CString> {
            if list.is_null() {
                return Vec::new();
            }
            (0..count as usize)
                .map(|i| {
                    // SAFETY: Streamline returns arrays of NUL-terminated strings of these
                    // lengths, valid while it is initialized; they are copied now.
                    unsafe { CStr::from_ptr(*list.add(i)) }.to_owned()
                })
                .collect()
        };
        for &feature in features {
            let mut requirements = sl::FeatureRequirements::new();
            // SAFETY: Streamline is initialized; `requirements` is a version-2 structure.
            let result =
                unsafe { (functions.get_feature_requirements)(feature, &mut requirements) };
            if result != sl::OK {
                return Err(DlssError::Streamline {
                    call: "slGetFeatureRequirements",
                    result,
                });
            }
            if requirements.flags & sl::REQUIREMENT_VULKAN == 0 {
                return Err(DlssError::MissingRequirement(String::from(
                    "a Streamline plugin with Vulkan support",
                )));
            }
            if feature == sl::FEATURE_DLSS
                && (requirements.vk_num_graphics_queues_required > 0
                    || requirements.vk_num_compute_queues_required > 0)
            {
                return Err(DlssError::MissingRequirement(String::from(
                    "extra queues for DLSS Super Resolution",
                )));
            }
            // Frame generation's own queues (its optical flow runs on the compute queues:
            // no native optical flow queue is created, Streamline's "interop" mode).
            shared.extra_queues.graphics += requirements.vk_num_graphics_queues_required;
            shared.extra_queues.compute += requirements.vk_num_compute_queues_required;
            for name in strings(
                requirements.vk_num_instance_extensions,
                requirements.vk_instance_extensions,
            ) {
                if !shared.instance_extensions.contains(&name) {
                    shared.instance_extensions.push(name);
                }
            }
            for name in strings(
                requirements.vk_num_device_extensions,
                requirements.vk_device_extensions,
            ) {
                if name.as_c_str() != EXT_BUFFER_DEVICE_ADDRESS
                    && name.as_c_str() != EXT_OPTICAL_FLOW
                    && !shared.device_extensions.contains(&name)
                {
                    shared.device_extensions.push(name);
                }
            }
            for name in strings(requirements.vk_num_features12, requirements.vk_features12)
                .into_iter()
                .chain(strings(
                    requirements.vk_num_features13,
                    requirements.vk_features13,
                ))
            {
                let name = name.to_string_lossy();
                if !ENGINE_FEATURES.contains(&name.as_ref()) {
                    return Err(DlssError::MissingRequirement(format!(
                        "the Vulkan feature {name}"
                    )));
                }
            }
        }
        Ok(Self {
            shared: Arc::new(shared),
        })
    }

    /// Whether DLSS frame generation, Reflex and the PC latency markers were loaded
    /// ([`StreamlineDesc::frame_generation`] and their DLLs). Whether it runs on the
    /// device is [`Self::frame_generation_supported`].
    pub fn frame_generation(&self) -> bool {
        self.shared.frame_generation
    }

    /// The queues frame generation asks for besides the engine's: create the device with
    /// them (`Device::with_extras`).
    pub fn extra_queues(&self) -> ExtraQueues {
        self.shared.extra_queues
    }

    /// The interposer's lookups, for the surface and the swapchain
    /// (`Surface::from_win32_hooked`, `Swapchain::new_hooked`) when frame generation is
    /// loaded: Streamline then presents.
    pub fn vulkan_hooks(&self) -> VulkanHooks {
        VulkanHooks {
            get_instance_proc_addr: self.shared.functions.get_instance_proc_addr,
        }
    }

    /// Whether frame generation runs on the attached device (after [`Self::attach`]).
    pub fn frame_generation_supported(&self) -> bool {
        self.frame_generation_functions().is_some()
    }

    /// Why frame generation does not run on the attached device: the Streamline call
    /// that refused it and its result (for instance `eErrorOSDisabledHWS`: Windows'
    /// hardware-accelerated GPU scheduling is off, which DLSS frame generation needs).
    /// `None` when it runs, or was not loaded.
    pub fn frame_generation_error(&self) -> Option<DlssError> {
        match self.shared.attached.get()?.frame_generation? {
            Ok(_) => None,
            Err((call, result)) => Some(DlssError::Streamline { call, result }),
        }
    }

    /// What a settings menu tells the player when DLSS frame generation cannot run on the
    /// attached device and they can fix it (show the option disabled with the hint, the
    /// engine's FSR frame generation still offered): `None` when it runs, was not loaded,
    /// or the GPU cannot run it at all.
    pub fn frame_generation_hint(&self) -> Option<FrameGenerationHint> {
        let Err((_, result)) = self.shared.attached.get()?.frame_generation? else {
            return None;
        };
        match result {
            sl::RESULT_DRIVER_OUT_OF_DATE => Some(FrameGenerationHint::UpdateDriver),
            sl::RESULT_OS_OUT_OF_DATE => Some(FrameGenerationHint::UpdateWindows),
            sl::RESULT_OS_DISABLED_HWS => Some(FrameGenerationHint::TurnOnGpuScheduling),
            _ => None,
        }
    }

    pub(crate) fn frame_generation_functions(&self) -> Option<FrameGenerationFunctions> {
        self.shared
            .attached
            .get()
            .and_then(|a| a.frame_generation)
            .and_then(Result::ok)
    }

    pub(crate) fn core_functions(
        &self,
    ) -> (
        sl::PfnGetNewFrameToken,
        sl::PfnSetConstants,
        sl::PfnSetTagForFrame,
        sl::PfnFreeResources,
    ) {
        let f = &self.shared.functions;
        (
            f.get_new_frame_token,
            f.set_constants,
            f.set_tag_for_frame,
            f.free_resources,
        )
    }

    /// Loads or unloads DLSS frame generation (`slSetFeatureLoaded`): a swapchain created
    /// while it is loaded goes through it (it copies every frame, even with frame
    /// generation off). Unload it and recreate the swapchain when the player turns frame
    /// generation off for good (DLSS-FG guide §18).
    pub fn set_frame_generation_loaded(&self, loaded: bool) -> Result<(), DlssError> {
        if !self.shared.frame_generation {
            return Ok(());
        }
        // SAFETY: Streamline is initialized with the feature requested.
        let result =
            unsafe { (self.shared.functions.set_feature_loaded)(sl::FEATURE_DLSS_G, loaded) };
        if result != sl::OK {
            return Err(DlssError::Streamline {
                call: "slSetFeatureLoaded",
                result,
            });
        }
        Ok(())
    }

    /// The folder the DLLs were loaded from.
    pub fn dir(&self) -> &std::path::Path {
        &self.shared.dir
    }

    /// Instance extensions DLSS needs: pass them to `Instance::with_extensions`.
    pub fn instance_extensions(&self) -> Vec<&CStr> {
        self.shared
            .instance_extensions
            .iter()
            .map(CString::as_c_str)
            .collect()
    }

    /// Device extensions DLSS needs: pass them to `Device::with_extensions` when
    /// [`Self::check_adapter`] accepts the adapter.
    pub fn device_extensions(&self) -> Vec<&CStr> {
        self.shared
            .device_extensions
            .iter()
            .map(CString::as_c_str)
            .collect()
    }

    /// Whether DLSS runs on `adapter`: an NVIDIA GPU Streamline supports (RTX, a recent
    /// enough driver), with every device extension DLSS needs. Call it before creating
    /// the device.
    pub fn check_adapter(&self, instance: &Instance, adapter: &Adapter) -> Result<(), DlssError> {
        engine_profiler::zone!("dlss::Streamline::check_adapter");
        for name in &self.shared.instance_extensions {
            if !instance.extra_extension_enabled(name) {
                return Err(DlssError::MissingRequirement(format!(
                    "the instance extension {}",
                    name.to_string_lossy()
                )));
            }
        }
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        // SAFETY: the adapter belongs to `instance`; the chain holds a core 1.1 structure.
        unsafe {
            instance
                .raw()
                .get_physical_device_properties2(adapter.physical(), &mut properties);
        }
        let mut info = sl::AdapterInfo::new();
        if id.device_luid_valid == vk::TRUE {
            info.device_luid = id.device_luid.as_ptr();
            info.device_luid_size = id.device_luid.len() as u32;
        }
        info.vk_physical_device = vk::Handle::as_raw(adapter.physical()) as *mut c_void;
        // SAFETY: Streamline is initialized; `info` and the LUID outlive the call.
        let result =
            unsafe { (self.shared.functions.is_feature_supported)(sl::FEATURE_DLSS, &info) };
        if result != sl::OK {
            return Err(DlssError::Streamline {
                call: "slIsFeatureSupported",
                result,
            });
        }
        // SAFETY: the adapter belongs to `instance`.
        let available = unsafe {
            instance
                .raw()
                .enumerate_device_extension_properties(adapter.physical())
        }
        .map_err(RhiError::vk("vkEnumerateDeviceExtensionProperties"))?;
        for name in &self.shared.device_extensions {
            if !available
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(name.as_c_str()))
            {
                return Err(DlssError::MissingRequirement(format!(
                    "the device extension {}",
                    name.to_string_lossy()
                )));
            }
        }
        Ok(())
    }

    /// Gives Streamline the device (`slSetVulkanInfo`), which initializes DLSS on it. The
    /// device must have been created with [`Self::device_extensions`] on an adapter
    /// [`Self::check_adapter`] accepted. Once per [`Streamline`].
    pub fn attach(&self, device: &Device) -> Result<(), DlssError> {
        engine_profiler::zone!("dlss::Streamline::attach");
        if self.shared.attached.get().is_some() {
            return Err(DlssError::MissingRequirement(String::from(
                "one device per Streamline",
            )));
        }
        for name in &self.shared.device_extensions {
            if !device.extra_extension_enabled(name) {
                return Err(DlssError::MissingRequirement(format!(
                    "the device extension {}",
                    name.to_string_lossy()
                )));
            }
        }
        let functions = &self.shared.functions;
        let mut info = sl::VulkanInfo::new();
        info.device = vk::Handle::as_raw(device.raw().handle()) as *mut c_void;
        info.instance = vk::Handle::as_raw(device.instance_raw().handle()) as *mut c_void;
        info.physical_device = vk::Handle::as_raw(device.physical()) as *mut c_void;
        // Streamline's own queues start after the engine's (`Device::with_extras` created
        // them for frame generation; DLSS Super Resolution asks for none).
        let start = device.extra_queue_start();
        info.graphics_queue_family = start.graphics.0;
        info.graphics_queue_index = start.graphics.1;
        info.compute_queue_family = start.compute.0;
        info.compute_queue_index = start.compute.1;
        // SAFETY: Streamline is initialized; the handles are the engine's live instance,
        // physical device and device, created after `slInit`.
        let result = unsafe { (functions.set_vulkan_info)(&info) };
        if result != sl::OK {
            return Err(DlssError::Streamline {
                call: "slSetVulkanInfo",
                result,
            });
        }
        // Features that failed to initialize on the device report it now.
        let adapter_info = sl::AdapterInfo::new();
        // SAFETY: as above; no adapter: a general check after device creation.
        let result = unsafe { (functions.is_feature_supported)(sl::FEATURE_DLSS, &adapter_info) };
        if result != sl::OK {
            return Err(DlssError::Streamline {
                call: "slIsFeatureSupported",
                result,
            });
        }
        let function = |name: &'static CStr| -> Result<*mut c_void, DlssError> {
            let mut f = core::ptr::null_mut();
            // SAFETY: the device is set; `name` is NUL-terminated.
            let result = unsafe {
                (functions.get_feature_function)(sl::FEATURE_DLSS, name.as_ptr(), &mut f)
            };
            if result != sl::OK || f.is_null() {
                return Err(DlssError::Streamline {
                    call: "slGetFeatureFunction",
                    result,
                });
            }
            Ok(f)
        };
        // SAFETY: the DLSS plugin's functions of these names have these signatures
        // (`sl_dlss.h`).
        let dlss = unsafe {
            DlssFunctions {
                get_optimal_settings: core::mem::transmute::<
                    *mut c_void,
                    sl::PfnDlssGetOptimalSettings,
                >(function(c"slDLSSGetOptimalSettings")?),
                get_state: core::mem::transmute::<*mut c_void, sl::PfnDlssGetState>(function(
                    c"slDLSSGetState",
                )?),
                set_options: core::mem::transmute::<*mut c_void, sl::PfnDlssSetOptions>(function(
                    c"slDLSSSetOptions",
                )?),
            }
        };
        // SAFETY: the interposer's `vkGetDeviceProcAddr` with the device just set: its
        // `vkQueuePresentKHR` runs Streamline's present hooks, then the driver's.
        let present = unsafe {
            (functions.get_device_proc_addr)(device.raw().handle(), c"vkQueuePresentKHR".as_ptr())
        }
        .map(|f| {
            // SAFETY: the function Vulkan names `vkQueuePresentKHR` has its signature.
            unsafe {
                core::mem::transmute::<unsafe extern "system" fn(), vk::PFN_vkQueuePresentKHR>(f)
            }
        });
        let frame_generation = self
            .shared
            .frame_generation
            .then(|| self.frame_generation_on(device));
        let _ = self.shared.attached.set(Attached {
            dlss,
            present,
            frame_generation,
        });
        Ok(())
    }

    /// Frame generation's functions when it runs on `device` (Streamline says so after the
    /// device is set), or the call that refused it.
    fn frame_generation_on(
        &self,
        device: &Device,
    ) -> Result<FrameGenerationFunctions, (&'static str, i32)> {
        let functions = &self.shared.functions;
        let mut info = sl::AdapterInfo::new();
        info.vk_physical_device = vk::Handle::as_raw(device.physical()) as *mut c_void;
        for feature in [sl::FEATURE_DLSS_G, sl::FEATURE_REFLEX, sl::FEATURE_PCL] {
            // SAFETY: Streamline is initialized and has the device; `info` outlives the call.
            let result = unsafe { (functions.is_feature_supported)(feature, &info) };
            if result != sl::OK {
                return Err(("slIsFeatureSupported", result));
            }
        }
        let function = |feature: sl::Feature,
                        name: &'static CStr|
         -> Result<*mut c_void, (&'static str, i32)> {
            let mut f = core::ptr::null_mut();
            // SAFETY: the device is set; `name` is NUL-terminated.
            let result =
                unsafe { (functions.get_feature_function)(feature, name.as_ptr(), &mut f) };
            if result == sl::OK && !f.is_null() {
                Ok(f)
            } else {
                Err(("slGetFeatureFunction", result))
            }
        };
        // SAFETY: the plugins' functions of these names have these signatures
        // (`sl_dlss_g.h`, `sl_reflex.h`, `sl_pcl.h`).
        unsafe {
            Ok(FrameGenerationFunctions {
                set_options: core::mem::transmute::<*mut c_void, sl::PfnDlssgSetOptions>(function(
                    sl::FEATURE_DLSS_G,
                    c"slDLSSGSetOptions",
                )?),
                get_state: core::mem::transmute::<*mut c_void, sl::PfnDlssgGetState>(function(
                    sl::FEATURE_DLSS_G,
                    c"slDLSSGGetState",
                )?),
                reflex_set_options: core::mem::transmute::<*mut c_void, sl::PfnReflexSetOptions>(
                    function(sl::FEATURE_REFLEX, c"slReflexSetOptions")?,
                ),
                reflex_sleep: core::mem::transmute::<*mut c_void, sl::PfnReflexSleep>(function(
                    sl::FEATURE_REFLEX,
                    c"slReflexSleep",
                )?),
                pcl_set_marker: core::mem::transmute::<*mut c_void, sl::PfnPclSetMarker>(function(
                    sl::FEATURE_PCL,
                    c"slPCLSetMarker",
                )?),
            })
        }
    }

    /// Streamline's `vkQueuePresentKHR` (after [`Self::attach`]), for
    /// `Swapchain::set_present_function`: presenting through it lets Streamline run its
    /// per-frame bookkeeping, which frees what it retired. Streamline logs an error once
    /// if it sees 60 evaluations without a present (headless rendering).
    pub fn present_function(&self) -> Option<vk::PFN_vkQueuePresentKHR> {
        self.shared.attached.get().and_then(|a| a.present)
    }

    /// Versions of the DLSS plugin and library.
    pub fn versions(&self) -> DlssVersions {
        let mut version = sl::FeatureVersion::new();
        // SAFETY: Streamline is initialized; `version` is a version-1 structure.
        let result =
            unsafe { (self.shared.functions.get_feature_version)(sl::FEATURE_DLSS, &mut version) };
        if result != sl::OK {
            return DlssVersions::default();
        }
        let v = |v: sl::Version| [v.major, v.minor, v.build];
        DlssVersions {
            streamline: v(version.version_sl),
            ngx: v(version.version_ngx),
        }
    }

    /// Warnings and errors Streamline logged in this process.
    pub fn log() -> StreamlineLogReport {
        StreamlineLogReport {
            warnings: WARNINGS.load(Ordering::Relaxed),
            errors: ERRORS.load(Ordering::Relaxed),
            first_errors: FIRST_ERRORS.lock().map(|v| v.clone()).unwrap_or_default(),
        }
    }

    fn dlss(&self) -> Result<DlssFunctions, DlssError> {
        self.shared
            .attached
            .get()
            .map(|a| a.dlss)
            .ok_or(DlssError::NotAttached)
    }
}

/// NVIDIA's DLSS render presets (`sl::DLSSPreset`, `sl_dlss.h` of Streamline 2.14.1):
/// which network runs. [`DlssPreset::Default`] lets NVIDIA choose per mode (K for DLAA,
/// Quality and Balanced; M for Performance; L for Ultra Performance).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DlssPreset {
    /// NVIDIA's choice for the mode (may change with a newer `nvngx_dlss.dll`).
    #[default]
    Default,
    /// Like K, a little less ghosting, a little more flicker.
    J,
    /// The transformer model: the best image, the most expensive.
    K,
    /// Sharper and more stable than J and K, more expensive (Ultra Performance's default).
    L,
    /// L's quality nearer J and K's speed (Performance's default).
    M,
}

impl DlssPreset {
    fn raw(self) -> u32 {
        match self {
            Self::Default => 0,
            Self::J => 10,
            Self::K => 11,
            Self::L => 12,
            Self::M => 13,
        }
    }
}

/// `sl::DLSSMode` for a quality level.
fn mode(quality: Quality) -> u32 {
    match quality {
        Quality::Native => sl::DLSS_MODE_DLAA,
        Quality::Quality => sl::DLSS_MODE_MAX_QUALITY,
        Quality::Balanced => sl::DLSS_MODE_BALANCED,
        Quality::Performance => sl::DLSS_MODE_MAX_PERFORMANCE,
        Quality::UltraPerformance => sl::DLSS_MODE_ULTRA_PERFORMANCE,
    }
}

/// The render sizes DLSS takes for one quality level at one output size
/// (`slDLSSGetOptimalSettings`): it renders best at `optimal` and accepts sizes from `min`
/// to `max` (dynamic resolution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DlssRenderSizes {
    /// The size DLSS was trained for at this quality level.
    pub optimal: (u32, u32),
    /// The smallest size it accepts.
    pub min: (u32, u32),
    /// The largest size it accepts.
    pub max: (u32, u32),
}

/// DLSS's render sizes at one output size, per [`Quality::ALL`] (`slDLSSGetOptimalSettings`).
#[derive(Debug, Clone, Copy)]
struct Optimal {
    output: (u32, u32),
    /// Optimal, minimum and maximum render size per quality level.
    sizes: [DlssRenderSizes; 5],
}

/// DLSS's render sizes for every quality level at `output`.
fn query_optimal(streamline: &Streamline, output: (u32, u32)) -> Result<Optimal, DlssError> {
    let dlss = streamline.dlss()?;
    let mut sizes = [DlssRenderSizes::default(); 5];
    for (i, quality) in Quality::ALL.into_iter().enumerate() {
        let mut options = sl::DlssOptions::new();
        options.mode = mode(quality);
        options.output_width = output.0;
        options.output_height = output.1;
        let mut settings = sl::DlssOptimalSettings::new();
        // SAFETY: Streamline is attached; both structures outlive the call.
        let result = unsafe { (dlss.get_optimal_settings)(&options, &mut settings) };
        if result != sl::OK {
            return Err(DlssError::Streamline {
                call: "slDLSSGetOptimalSettings",
                result,
            });
        }
        let optimal = (
            settings.optimal_render_width,
            settings.optimal_render_height,
        );
        let min = (settings.render_width_min, settings.render_height_min);
        let max = (settings.render_width_max, settings.render_height_max);
        sizes[i] = DlssRenderSizes {
            optimal,
            min: if min.0 == 0 { optimal } else { min },
            max: if max.0 == 0 { optimal } else { max },
        };
    }
    Ok(Optimal { output, sizes })
}

/// A Streamline viewport: one DLSS feature (network, history) at one output size and mode.
#[derive(Debug, Clone, Copy)]
struct Viewport {
    id: u32,
    output: (u32, u32),
    mode: u32,
    preset: DlssPreset,
}

/// Streamline viewport ids, unique in the process.
static NEXT_VIEWPORT: AtomicU32 = AtomicU32::new(1);

/// DLSS Super Resolution for one view (see the crate documentation): it owns the view's
/// Streamline viewport, whose DLSS feature keeps the history.
pub struct Dlss {
    streamline: Streamline,
    optimal: Option<Optimal>,
    viewport: Option<Viewport>,
    /// Viewports replaced by a new size or mode, freed once their frames are done.
    retired: Vec<(u64, u32)>,
    frames_in_flight: u64,
    preset: DlssPreset,
    reset: bool,
    /// Flip the sign of the jitter or the motion vectors passed to DLSS (tests: a wrong
    /// convention must show).
    flip_jitter: bool,
    flip_motion: bool,
}

impl fmt::Debug for Dlss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dlss")
            .field("viewport", &self.viewport)
            .field("preset", &self.preset)
            .finish_non_exhaustive()
    }
}

impl Dlss {
    /// DLSS for a view of the device `streamline` is attached to. `frames_in_flight`
    /// bounds how long a replaced DLSS feature is kept.
    pub fn new(streamline: &Streamline, frames_in_flight: usize) -> Result<Self, DlssError> {
        streamline.dlss()?;
        Ok(Self {
            streamline: streamline.clone(),
            optimal: None,
            viewport: None,
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            preset: DlssPreset::Default,
            reset: false,
            flip_jitter: false,
            flip_motion: false,
        })
    }

    /// Jitter phases for upscaling `render` to `output`: ⌈8 · s²⌉ for a scale `s` per axis,
    /// at least 8, NVIDIA's recommendation (DLSS Programming Guide §3.7.1.1: 8, 18, 24, 32
    /// and 72 for DLAA to Ultra Performance), and the engine's TAAU rule.
    pub fn jitter_phases(render: (u32, u32), output: (u32, u32)) -> u32 {
        engine_upscale::Taau::jitter_phases(render, output)
    }

    /// The texture LOD bias NVIDIA recommends, log2(render / output) − 1 (DLSS
    /// Programming Guide §3.5), one level sharper than the engine's standard bias; it
    /// measured better with DLSS (ADR 0063). Pass it to `MaterialTable::set_texture_lod_bias`.
    pub fn texture_lod_bias(render: (u32, u32), output: (u32, u32)) -> f32 {
        engine_upscale::texture_lod_bias(render, output) - 1.0
    }

    /// Chooses the network ([`DlssPreset`]); takes effect with the next
    /// [`Self::add_pass`] (a new DLSS feature).
    pub fn set_preset(&mut self, preset: DlssPreset) {
        self.preset = preset;
    }

    /// The current preset.
    pub fn preset(&self) -> DlssPreset {
        self.preset
    }

    /// Forgets the history: the next frame starts anew (after a camera cut, with
    /// `Temporal::reset`, which does the same through `MotionOutput::reset`).
    pub fn reset(&mut self) {
        self.reset = true;
    }

    #[doc(hidden)]
    /// Tests: pass DLSS the jitter and the motion vectors with the wrong sign.
    pub fn set_flipped_conventions(&mut self, jitter: bool, motion: bool) {
        self.flip_jitter = jitter;
        self.flip_motion = motion;
    }

    /// The render size DLSS wants for `quality` at `output` (`slDLSSGetOptimalSettings`).
    pub fn render_size(
        &self,
        quality: Quality,
        output: (u32, u32),
    ) -> Result<(u32, u32), DlssError> {
        let optimal = match self.optimal {
            Some(optimal) if optimal.output == output => optimal,
            _ => query_optimal(&self.streamline, output)?,
        };
        Ok(optimal.sizes[quality as usize].optimal)
    }

    /// The render sizes DLSS takes for `quality` at `output`: the optimal size, and the
    /// range it accepts (dynamic resolution).
    pub fn render_sizes(
        &self,
        quality: Quality,
        output: (u32, u32),
    ) -> Result<DlssRenderSizes, DlssError> {
        Ok(query_optimal(&self.streamline, output)?.sizes[quality as usize])
    }

    /// DLSS's estimate of the GPU memory its feature for this view holds (0 before the
    /// first frame).
    pub fn gpu_memory(&self) -> u64 {
        let (Some(viewport), Ok(dlss)) = (self.viewport, self.streamline.dlss()) else {
            return 0;
        };
        let handle = sl::ViewportHandle::new(viewport.id);
        let mut state = sl::DlssState::new();
        // SAFETY: Streamline is attached; both structures outlive the call.
        let result = unsafe { (dlss.get_state)(&handle, &mut state) };
        if result == sl::OK {
            state.estimated_vram_usage_in_bytes
        } else {
            0
        }
    }

    /// Evaluations Streamline refused so far in this process (the frame then has no
    /// upscaled image), and the last refusal's `sl::Result`.
    pub fn evaluate_failures() -> (u32, i32) {
        (
            EVALUATE_FAILURES.load(Ordering::Relaxed),
            LAST_EVALUATE_RESULT.load(Ordering::Relaxed),
        )
    }

    fn optimal(&mut self, output: (u32, u32)) -> Result<Optimal, DlssError> {
        if let Some(optimal) = self.optimal
            && optimal.output == output
        {
            return Ok(optimal);
        }
        let optimal = query_optimal(&self.streamline, output)?;
        self.optimal = Some(optimal);
        Ok(optimal)
    }

    /// The DLSS mode for rendering at `render`: the mode with the smallest optimal size at
    /// least `render` (smaller sizes run as dynamic resolution); DLAA above Quality's size.
    fn mode_for(optimal: &Optimal, render: (u32, u32)) -> u32 {
        if render.0 >= optimal.output.0 && render.1 >= optimal.output.1 {
            return sl::DLSS_MODE_DLAA;
        }
        for (i, quality) in Quality::ALL.into_iter().enumerate().skip(1).rev() {
            let size = optimal.sizes[i].optimal;
            if size.0 >= render.0 && size.1 >= render.1 {
                return mode(quality);
            }
        }
        // Larger than Quality's size: only DLAA's feature takes it.
        sl::DLSS_MODE_DLAA
    }

    fn collect(&mut self, frame_number: u64) {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let Ok(_) = self.streamline.dlss() else {
            return;
        };
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            let (_, id) = self.retired.swap_remove(i);
            let handle = sl::ViewportHandle::new(id);
            // SAFETY: Streamline is attached; the frames that used the viewport are done.
            unsafe { (self.streamline.shared.functions.free_resources)(sl::FEATURE_DLSS, &handle) };
        }
    }

    /// Declares the DLSS pass into an image of `output_size` and returns it: HDR, ready for
    /// tonemapping. `frame_number` bounds how long a replaced DLSS feature is kept.
    pub fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        input: UpscaleInput,
        output_size: (u32, u32),
        frame_number: u64,
    ) -> Result<Texture, DlssError> {
        engine_profiler::zone!("dlss::Dlss::add_pass");
        self.collect(frame_number);
        let dlss = self.streamline.dlss()?;
        let output_size = (output_size.0.max(1), output_size.1.max(1));
        let color_desc = fg.texture_desc(input.color);
        let depth_desc = fg.texture_desc(input.depth);
        let velocity_desc = fg.texture_desc(input.motion.velocity);
        let render = (color_desc.width, color_desc.height);
        let optimal = self.optimal(output_size)?;
        let mode = Self::mode_for(&optimal, render);
        let mut reset = core::mem::take(&mut self.reset) || input.motion.reset;
        if self
            .viewport
            .is_none_or(|v| v.output != output_size || v.mode != mode || v.preset != self.preset)
        {
            let id = NEXT_VIEWPORT.fetch_add(1, Ordering::Relaxed);
            let old = self.viewport.replace(Viewport {
                id,
                output: output_size,
                mode,
                preset: self.preset,
            });
            if let Some(old) = old {
                self.retired.push((frame_number, old.id));
            }
            reset = true;
        }
        let Some(viewport) = self.viewport else {
            unreachable!("created above");
        };

        let mut constants = sl::Constants::new();
        let jitter_sign = if self.flip_jitter { -1.0 } else { 1.0 };
        let motion_sign = if self.flip_motion { 1.0 } else { -1.0 };
        constants.jitter_offset = [
            input.motion.jitter[0] * jitter_sign,
            input.motion.jitter[1] * jitter_sign,
        ];
        // The engine's vectors are the texture-coordinate delta current − previous;
        // DLSS wants them pointing to the previous position, in [-1, 1] after this scale
        // (Streamline multiplies by the render size).
        constants.mvec_scale = [motion_sign, motion_sign];
        constants.camera_near = input.camera.near;
        constants.camera_far = input.camera.far;
        constants.camera_fov = input.camera.fov_y;
        constants.camera_aspect_ratio = render.0 as f32 / render.1.max(1) as f32;
        constants.depth_inverted = sl::FALSE;
        constants.camera_motion_included = sl::TRUE;
        constants.motion_vectors_3d = sl::FALSE;
        constants.motion_vectors_jittered = sl::FALSE;
        constants.motion_vectors_dilated = sl::FALSE;
        constants.orthographic_projection = sl::FALSE;
        constants.reset = if reset { sl::TRUE } else { sl::FALSE };

        let mut options = sl::DlssOptions::new();
        options.mode = viewport.mode;
        options.output_width = output_size.0;
        options.output_height = output_size.1;
        options.color_buffers_hdr = sl::TRUE;
        // The engine's exposure when given (the one the tone mapper applies), else DLSS's
        // own measurement.
        let exposure = input.exposure;
        let exposure_desc = exposure.map(|e| fg.texture_desc(e));
        options.use_auto_exposure = if exposure.is_some() {
            sl::FALSE
        } else {
            sl::TRUE
        };
        let preset = viewport.preset.raw();
        options.dlaa_preset = preset;
        options.quality_preset = preset;
        options.balanced_preset = preset;
        options.performance_preset = preset;
        options.ultra_performance_preset = preset;

        let functions = self.streamline.shared.functions;
        let output = fg.create_texture(
            "dlss output",
            TextureDesc::new_2d(OUTPUT_FORMAT, output_size.0, output_size.1),
        );
        // DLSS clears its output with a transfer (`vkCmdClearColorImage`) when it resets:
        // this pass only gives the graph's image the transfer-destination usage (the
        // graph derives usages from the declared accesses) and records nothing.
        fg.add_pass("dlss output usage")
            .write(output, Access::TransferWrite);
        // The engine's reactive mask is not passed: DLSS's current networks (presets J to
        // M) take no reactive or transparency mask (DLSS Programming Guide §3.15).
        let velocity = input.motion.velocity;
        let mut pass = fg.add_pass("dlss");
        if let Some(exposure) = exposure {
            pass = pass.read(exposure, Access::Sampled(ShaderStages::COMPUTE));
        }
        pass.read(input.color, Access::Sampled(ShaderStages::COMPUTE))
            .read(input.depth, Access::Sampled(ShaderStages::COMPUTE))
            .read(velocity, Access::Sampled(ShaderStages::COMPUTE))
            .write(output, Access::StorageWrite(ShaderStages::COMPUTE))
            .execute(move |ctx| {
                let sampled = vk::ImageUsageFlags::SAMPLED.as_raw();
                let resource = |texture: Texture, desc: TextureDesc, layout, usage| {
                    let mut r = sl::Resource::empty();
                    r.native = vk::Handle::as_raw(ctx.image(texture)) as *mut c_void;
                    r.view = vk::Handle::as_raw(ctx.view(texture)) as *mut c_void;
                    r.state = layout;
                    r.width = desc.width;
                    r.height = desc.height;
                    r.native_format = desc.format.as_raw() as u32;
                    r.mip_levels = 1;
                    r.array_layers = 1;
                    r.usage = usage;
                    r
                };
                let read = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL.as_raw() as u32;
                let mut resources = [
                    resource(input.color, color_desc, read, sampled),
                    resource(input.depth, depth_desc, read, sampled),
                    resource(velocity, velocity_desc, read, sampled),
                    resource(
                        output,
                        TextureDesc::new_2d(OUTPUT_FORMAT, output_size.0, output_size.1),
                        vk::ImageLayout::GENERAL.as_raw() as u32,
                        sampled
                            | (vk::ImageUsageFlags::STORAGE
                                | vk::ImageUsageFlags::TRANSFER_SRC
                                | vk::ImageUsageFlags::TRANSFER_DST)
                                .as_raw(),
                    ),
                    match (exposure, exposure_desc) {
                        (Some(texture), Some(desc)) => resource(texture, desc, read, sampled),
                        _ => sl::Resource::empty(),
                    },
                ];
                let inputs = [
                    (resources[0].native, vk::ImageAspectFlags::COLOR),
                    (resources[1].native, vk::ImageAspectFlags::DEPTH),
                    (resources[2].native, vk::ImageAspectFlags::COLOR),
                    (resources[4].native, vk::ImageAspectFlags::COLOR),
                ];
                // The graph leaves sampled inputs in READ_ONLY_OPTIMAL, which Streamline
                // does not know (it maps unknown layouts to GENERAL); give it
                // SHADER_READ_ONLY_OPTIMAL and restore the graph's layout after.
                // SAFETY: recording outside a rendering scope; the images are the pass's
                // declared inputs, synchronized by the graph for compute sampling.
                unsafe {
                    transition(
                        ctx,
                        &inputs,
                        (
                            vk::PipelineStageFlags2::ALL_COMMANDS,
                            vk::AccessFlags2::MEMORY_WRITE,
                        ),
                        vk::ImageLayout::READ_ONLY_OPTIMAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    );
                }
                let kinds = [
                    sl::BUFFER_SCALING_INPUT_COLOR,
                    sl::BUFFER_DEPTH,
                    sl::BUFFER_MOTION_VECTORS,
                    sl::BUFFER_SCALING_OUTPUT_COLOR,
                    sl::BUFFER_EXPOSURE,
                ];
                // Local tags, valid until the evaluation returns: Streamline copies nothing.
                let mut tags = [sl::ResourceTag::empty(); 5];
                for ((tag, resource), kind) in tags.iter_mut().zip(&mut resources).zip(kinds) {
                    tag.resource = resource;
                    tag.kind = kind;
                    tag.lifecycle = sl::LIFECYCLE_VALID_UNTIL_EVALUATE;
                }
                let handle = sl::ViewportHandle::new(viewport.id);
                let chain: [*const sl::BaseStructure; 6] = [
                    (&raw const handle).cast(),
                    (&raw const tags[0]).cast(),
                    (&raw const tags[1]).cast(),
                    (&raw const tags[2]).cast(),
                    (&raw const tags[3]).cast(),
                    (&raw const tags[4]).cast(),
                ];
                // The exposure's tag only when there is one.
                let links = if exposure.is_some() { 6 } else { 5 };
                let frame_index = frame_number as u32;
                let mut token = core::ptr::null_mut();
                // SAFETY: Streamline is attached; every structure outlives the calls; the
                // command buffer is the frame's, recording; the tagged images are in the
                // layouts stated (inputs just transitioned, the output GENERAL for storage).
                let result = unsafe {
                    let mut result = (functions.get_new_frame_token)(&mut token, &frame_index);
                    if result == sl::OK {
                        result = (functions.set_constants)(&constants, token, &handle);
                    }
                    if result == sl::OK {
                        result = (dlss.set_options)(&handle, &options);
                    }
                    if result == sl::OK {
                        result = (functions.evaluate_feature)(
                            sl::FEATURE_DLSS,
                            token,
                            chain.as_ptr(),
                            links,
                            vk::Handle::as_raw(ctx.cmd) as *mut c_void,
                        );
                    }
                    result
                };
                if result != sl::OK {
                    EVALUATE_FAILURES.fetch_add(1, Ordering::Relaxed);
                    LAST_EVALUATE_RESULT.store(result, Ordering::Relaxed);
                }
                // Back to the graph's layout after DLSS's compute reads.
                // SAFETY: as above.
                unsafe {
                    transition(
                        ctx,
                        &inputs,
                        (
                            vk::PipelineStageFlags2::ALL_COMMANDS,
                            vk::AccessFlags2::NONE,
                        ),
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::READ_ONLY_OPTIMAL,
                    );
                }
            });
        Ok(output)
    }

    /// Frees the view's DLSS features.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy(&mut self) {
        let free = self.streamline.shared.functions.free_resources;
        for id in self
            .retired
            .drain(..)
            .map(|(_, id)| id)
            .chain(self.viewport.take().map(|v| v.id))
        {
            let handle = sl::ViewportHandle::new(id);
            // SAFETY: Streamline is initialized; the GPU is idle (the caller's contract).
            unsafe { free(sl::FEATURE_DLSS, &handle) };
        }
    }
}

/// DLSS behind the engine's upscaler interface.
impl engine_upscale::Upscaler for Dlss {
    fn kind(&self) -> UpscalerKind {
        UpscalerKind::Dlss
    }

    fn render_size(&self, quality: Quality, output: (u32, u32)) -> (u32, u32) {
        Self::render_size(self, quality, output).unwrap_or_else(|_| quality.render_size(output))
    }

    fn jitter_phases(&self, render: (u32, u32), output: (u32, u32)) -> u32 {
        Self::jitter_phases(render, output)
    }

    /// NVIDIA's recommendation, which measured better with DLSS than the standard bias
    /// (ADR 0063; unlike FSR, ADR 0060).
    fn texture_lod_bias(&self, render: (u32, u32), output: (u32, u32)) -> f32 {
        Self::texture_lod_bias(render, output)
    }

    fn reset(&mut self) {
        Self::reset(self);
    }

    fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        _ctx: engine_upscale::UpscaleContext<'_, 'g>,
        input: UpscaleInput,
        output_size: (u32, u32),
        frame_number: u64,
    ) -> Result<Texture, UpscaleError> {
        Ok(Self::add_pass(self, fg, input, output_size, frame_number)?)
    }

    unsafe fn destroy(
        &mut self,
        _allocator: &mut engine_rhi::Allocator,
        _bindless: &engine_rhi::Bindless,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe { Self::destroy(self) };
    }
}

/// Layout transitions of the pass's sampled inputs (null images are skipped), after the
/// accesses in `after`, with a full memory barrier around DLSS's own work.
///
/// # Safety
///
/// Recording outside a rendering scope; the images are in `from`.
unsafe fn transition(
    ctx: &engine_render::graph::PassContext<'_>,
    images: &[(*mut c_void, vk::ImageAspectFlags); 4],
    after: (vk::PipelineStageFlags2, vk::AccessFlags2),
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let mut barriers = [vk::ImageMemoryBarrier2::default(); 4];
    let mut count = 0;
    for &(image, aspect) in images {
        if image.is_null() {
            continue;
        }
        barriers[count] = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(after.0)
            .src_access_mask(after.1)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(from)
            .new_layout(to)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(vk::Handle::from_raw(image as u64))
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        count += 1;
    }
    // DLSS writes and reads with stages and accesses of its own (a transfer clear of the
    // output on reset, compute passes): order everything before against everything in it,
    // and it against everything after, which the graph's barriers then chain from.
    let global = [vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)];
    // SAFETY: guaranteed by the caller.
    unsafe {
        ctx.device.cmd_pipeline_barrier2(
            ctx.cmd,
            &vk::DependencyInfo::default()
                .memory_barriers(&global)
                .image_memory_barriers(&barriers[..count]),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NGX 310.9.1's optimal sizes at 1920 × 1080 (`dlss_probe`).
    fn at_1080p() -> Optimal {
        let size = |w, h| DlssRenderSizes {
            optimal: (w, h),
            min: (w, h),
            max: (1920, 1080),
        };
        Optimal {
            output: (1920, 1080),
            sizes: [
                size(1920, 1080),
                size(1280, 720),
                size(1114, 626),
                size(960, 540),
                size(640, 360),
            ],
        }
    }

    #[test]
    fn the_mode_follows_the_render_size() {
        let optimal = at_1080p();
        let mode_at = |w, h| Dlss::mode_for(&optimal, (w, h));
        assert_eq!(mode_at(1920, 1080), sl::DLSS_MODE_DLAA);
        assert_eq!(mode_at(1600, 900), sl::DLSS_MODE_DLAA);
        assert_eq!(mode_at(1280, 720), sl::DLSS_MODE_MAX_QUALITY);
        assert_eq!(mode_at(1129, 635), sl::DLSS_MODE_MAX_QUALITY);
        assert_eq!(mode_at(1114, 626), sl::DLSS_MODE_BALANCED);
        assert_eq!(mode_at(960, 540), sl::DLSS_MODE_MAX_PERFORMANCE);
        assert_eq!(mode_at(800, 450), sl::DLSS_MODE_MAX_PERFORMANCE);
        assert_eq!(mode_at(640, 360), sl::DLSS_MODE_ULTRA_PERFORMANCE);
        assert_eq!(mode_at(320, 180), sl::DLSS_MODE_ULTRA_PERFORMANCE);
    }

    #[test]
    fn modes_and_presets_have_streamlines_values() {
        assert_eq!(mode(Quality::Native), 6);
        assert_eq!(mode(Quality::Quality), 3);
        assert_eq!(mode(Quality::Balanced), 2);
        assert_eq!(mode(Quality::Performance), 1);
        assert_eq!(mode(Quality::UltraPerformance), 4);
        assert_eq!(DlssPreset::K.raw(), 11);
        assert_eq!(result_name(7), "eErrorAdapterNotSupported");
        assert_eq!(result_name(99), "unknown sl::Result");
    }
}
