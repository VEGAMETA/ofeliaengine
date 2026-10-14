//! Vulkan instance, validation layer and debug messenger.
//!
//! **An instance whose last handle goes while other instances of the process live is
//! destroyed with the last of them** ([`Instances`]). On the Radeon iGPU's driver (26.8.1),
//! destroying an instance loses another instance's device that renders with MSAA and
//! streamed textures: its next submit returns `ERROR_DEVICE_LOST`, while destroying only the
//! other device does not (`docs/status.md` §6). A game creates one instance and destroys it
//! last, as before; tests and tools that run several at once keep the ones that finished
//! until the last one does.

use crate::adapter::{Adapter, AdapterPreference};
use crate::error::RhiError;
use ash::{ext, vk};
use core::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";

/// Parameters for [`Instance::new`].
#[derive(Debug, Clone)]
pub struct InstanceDesc {
    /// Application name reported to drivers and tools.
    pub app_name: String,
    /// Enable the Khronos validation layer if installed (Vulkan SDK). Development only:
    /// it costs a lot of CPU time.
    pub validation: bool,
    /// Enable `VK_EXT_debug_utils` for object names and GPU markers (RenderDoc, Nsight)
    /// even without validation.
    pub debug_utils: bool,
    /// Enable the surface extensions needed to present to windows.
    pub surface: bool,
    /// Enable `VK_EXT_headless_surface` (tests and servers: swapchains without a window).
    pub headless_surface: bool,
    /// With validation: also enable synchronization validation, which reports missing or
    /// wrong barriers (hazards). Slower still; tests of barrier-generating code use it.
    pub sync_validation: bool,
}

impl Default for InstanceDesc {
    fn default() -> Self {
        Self {
            app_name: String::from("engine"),
            validation: cfg!(debug_assertions),
            debug_utils: true,
            surface: true,
            headless_surface: false,
            sync_validation: false,
        }
    }
}

/// Counts of validation messages since the instance was created.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// Validation layer is active.
    pub enabled: bool,
    /// Error messages.
    pub errors: u32,
    /// Warning messages.
    pub warnings: u32,
    /// Text of the first few error messages.
    pub first_errors: Vec<String>,
}

/// The process's instances: how many are alive, and those whose last handle went while
/// others lived, destroyed with the last one (the module's documentation says why).
struct Instances {
    live: usize,
    parked: Vec<Parked>,
}

static INSTANCES: Mutex<Instances> = Mutex::new(Instances {
    live: 0,
    parked: Vec::new(),
});

/// An instance waiting for the process's other instances to go.
struct Parked {
    raw: ash::Instance,
    debug_utils: Option<(ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    /// The messenger's user data, alive as long as the messenger.
    _debug_state: Option<Box<DebugState>>,
}

impl Parked {
    /// Destroys the messenger, then the instance.
    ///
    /// # Safety
    ///
    /// Every object created from the instance (devices, surfaces) has been destroyed.
    unsafe fn destroy(self) {
        // SAFETY: guaranteed by the caller; the messenger belongs to the instance.
        unsafe {
            if let Some((utils, messenger)) = self.debug_utils {
                utils.destroy_debug_utils_messenger(messenger, None);
            }
            self.raw.destroy_instance(None);
        }
    }
}

/// Instances alive and parked in the process (tests).
#[cfg(test)]
fn instance_counts() -> (usize, usize) {
    let instances = INSTANCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (instances.live, instances.parked.len())
}

#[derive(Default)]
struct DebugState {
    errors: AtomicU32,
    warnings: AtomicU32,
    // Validation is a development-only path; a mutex here never runs in shipping builds.
    first_errors: Mutex<Vec<String>>,
}

pub(crate) struct InstanceShared {
    pub(crate) entry: ash::Entry,
    pub(crate) raw: ash::Instance,
    debug_utils: Option<(ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    debug_state: Option<Box<DebugState>>,
    pub(crate) surface_enabled: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) win32_surface_enabled: bool,
    pub(crate) headless_surface_enabled: bool,
    /// `VK_EXT_swapchain_colorspace`: surfaces may offer HDR color spaces.
    pub(crate) swapchain_colorspace_enabled: bool,
    pub(crate) debug_utils_enabled: bool,
    /// Extensions a library asked for ([`Instance::with_extensions`]) that were enabled.
    pub(crate) extra_extensions: Vec<std::ffi::CString>,
}

impl Drop for InstanceShared {
    fn drop(&mut self) {
        let this = Parked {
            raw: self.raw.clone(),
            debug_utils: self.debug_utils.take(),
            _debug_state: self.debug_state.take(),
        };
        // Not a hot path: instances come and go with devices. The lock is held while the
        // last instance destroys the parked ones, so none is destroyed while another one
        // counts as alive.
        let mut instances = INSTANCES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        instances.live = instances.live.saturating_sub(1);
        if instances.live > 0 {
            instances.parked.push(this);
            return;
        }
        let parked = std::mem::take(&mut instances.parked);
        // SAFETY: every object created from an instance (devices, surfaces) holds an `Arc`
        // to its shared state and is destroyed first: this one's and the parked ones' are
        // gone.
        unsafe {
            this.destroy();
            for instance in parked {
                instance.destroy();
            }
        }
    }
}

/// A Vulkan instance. Cheap to clone (shared); destroyed when the last clone and every
/// device created from it are gone, or, while other instances of the process live, with the
/// last of them (the module's documentation says why).
#[derive(Clone)]
pub struct Instance {
    pub(crate) shared: Arc<InstanceShared>,
}

impl core::fmt::Debug for Instance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Instance")
            .field("validation", &self.shared.debug_state.is_some())
            .finish()
    }
}

unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _types: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    user: *mut c_void,
) -> vk::Bool32 {
    // SAFETY: `user` is the `DebugState` registered with the messenger, alive until the
    // messenger is destroyed; `data` is valid for the duration of the callback.
    let (state, message) = unsafe {
        let state = &*(user as *const DebugState);
        let message = if data.is_null() || (*data).p_message.is_null() {
            std::borrow::Cow::Borrowed("<no message>")
        } else {
            CStr::from_ptr((*data).p_message).to_string_lossy()
        };
        (state, message)
    };
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        state.errors.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut first) = state.first_errors.lock()
            && first.len() < 8
        {
            first.push(message.clone().into_owned());
        }
        report(log::Level::Error, &message);
    } else if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::WARNING) {
        state.warnings.fetch_add(1, Ordering::Relaxed);
        report(log::Level::Warn, &message);
    }
    vk::FALSE
}

/// Sends a validation message to the log (target `vulkan`, ADR 0061); to stderr when no
/// logger is installed (tests), so messages are never silently dropped.
fn report(level: log::Level, message: &str) {
    if log::max_level() == log::LevelFilter::Off {
        #[allow(clippy::print_stderr)] // no logger: the only place left to report it
        {
            eprintln!("[vulkan {level}] {message}");
        }
        engine_profiler::message(message);
    } else {
        log::log!(target: "vulkan", level, "{message}");
    }
}

impl Instance {
    /// Loads the Vulkan loader and creates the instance (Vulkan 1.3).
    pub fn new(desc: &InstanceDesc) -> Result<Self, RhiError> {
        Self::with_extensions(desc, &[])
    }

    /// Like [`Self::new`], also enabling `extra` instance extensions a library needs (the
    /// ones Streamline asks for DLSS, ADR 0063). Those the loader does not list are skipped:
    /// [`Self::extra_extension_enabled`] tells which were enabled.
    ///
    /// The instance is created on the process's Vulkan loader thread, which lives as long
    /// as the process, and the layers and drivers loaded for it stay loaded until the
    /// process exits (`modules.rs`: the validation layer's allocator crashes when the
    /// thread that loaded it has exited).
    pub fn with_extensions(desc: &InstanceDesc, extra: &[&CStr]) -> Result<Self, RhiError> {
        engine_profiler::zone!("rhi::Instance::new");
        let desc = desc.clone();
        let extra: Vec<std::ffi::CString> = extra.iter().map(|&name| name.to_owned()).collect();
        crate::modules::on_loader_thread(move || {
            let extra: Vec<&CStr> = extra.iter().map(std::ffi::CString::as_c_str).collect();
            Self::create(&desc, &extra)
        })
    }

    fn create(desc: &InstanceDesc, extra: &[&CStr]) -> Result<Self, RhiError> {
        engine_profiler::zone!("rhi::Instance::create");
        // The layers and drivers loaded for this instance stay loaded for the process
        // (`modules.rs`).
        let pinning = crate::modules::Pinning::start();
        let entry = crate::modules::entry()?;
        // SAFETY: global command with no preconditions.
        let version = unsafe { entry.try_enumerate_instance_version() }
            .map_err(RhiError::vk("vkEnumerateInstanceVersion"))?
            .unwrap_or(vk::API_VERSION_1_0);
        if version < vk::API_VERSION_1_3 {
            return Err(RhiError::VersionTooOld { found: version });
        }

        // SAFETY: global commands with no preconditions.
        let layers = unsafe { entry.enumerate_instance_layer_properties() }
            .map_err(RhiError::vk("vkEnumerateInstanceLayerProperties"))?;
        // SAFETY: as above.
        let extensions = unsafe { entry.enumerate_instance_extension_properties(None) }
            .map_err(RhiError::vk("vkEnumerateInstanceExtensionProperties"))?;
        let has_layer = |name: &CStr| layers.iter().any(|l| l.layer_name_as_c_str() == Ok(name));
        let has_extension = |name: &CStr| {
            extensions
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(name))
        };

        let validation = desc.validation && has_layer(VALIDATION_LAYER);
        // VK_EXT_validation_features is provided by the layer itself.
        let sync_validation = validation
            && desc.sync_validation
            // SAFETY: global command; the layer name is NUL-terminated.
            && unsafe { entry.enumerate_instance_extension_properties(Some(VALIDATION_LAYER)) }
                .unwrap_or_default()
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(ext::validation_features::NAME));
        let debug_utils = (validation || desc.debug_utils) && has_extension(ext::debug_utils::NAME);
        let mut enabled_extensions: Vec<*const i8> = Vec::new();
        if debug_utils {
            enabled_extensions.push(ext::debug_utils::NAME.as_ptr());
        }
        let wants_surface = desc.surface || desc.headless_surface;
        let surface = wants_surface && has_extension(ash::khr::surface::NAME);
        let win32_surface = surface
            && cfg!(windows)
            && desc.surface
            && has_extension(ash::khr::win32_surface::NAME);
        let headless_surface =
            surface && desc.headless_surface && has_extension(ext::headless_surface::NAME);
        // HDR output: without it, surfaces only list sRGB color spaces.
        let swapchain_colorspace = surface && has_extension(ext::swapchain_colorspace::NAME);
        if surface {
            enabled_extensions.push(ash::khr::surface::NAME.as_ptr());
        }
        if win32_surface {
            enabled_extensions.push(ash::khr::win32_surface::NAME.as_ptr());
        }
        if headless_surface {
            enabled_extensions.push(ext::headless_surface::NAME.as_ptr());
        }
        if swapchain_colorspace {
            enabled_extensions.push(ext::swapchain_colorspace::NAME.as_ptr());
        }
        if sync_validation {
            enabled_extensions.push(ext::validation_features::NAME.as_ptr());
        }
        let mut extra_extensions = Vec::new();
        for &name in extra {
            let listed = enabled_extensions.iter().any(|&e| {
                // SAFETY: the list holds pointers to NUL-terminated static names.
                (unsafe { CStr::from_ptr(e) }) == name
            });
            if has_extension(name) {
                if !listed {
                    enabled_extensions.push(name.as_ptr());
                }
                extra_extensions.push(name.to_owned());
            }
        }
        let enabled_layers: Vec<*const i8> = if validation {
            vec![VALIDATION_LAYER.as_ptr()]
        } else {
            Vec::new()
        };

        let app_name = std::ffi::CString::new(desc.app_name.replace('\0', "")).unwrap_or_default();
        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .engine_name(c"engine")
            .api_version(vk::API_VERSION_1_3);

        let debug_state = validation.then(|| Box::new(DebugState::default()));
        let mut messenger_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
            .message_severity(
                vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                    | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING,
            )
            .message_type(
                vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                    | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                    | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
            )
            .pfn_user_callback(Some(debug_callback));
        if let Some(state) = &debug_state {
            messenger_info = messenger_info.user_data((&raw const **state).cast_mut().cast());
        }

        let mut create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_layer_names(&enabled_layers)
            .enabled_extension_names(&enabled_extensions);
        if debug_state.is_some() {
            // Also report problems during instance creation and destruction.
            create_info = create_info.push_next(&mut messenger_info);
        }
        let enabled_features = [vk::ValidationFeatureEnableEXT::SYNCHRONIZATION_VALIDATION];
        let mut validation_features =
            vk::ValidationFeaturesEXT::default().enabled_validation_features(&enabled_features);
        if sync_validation {
            create_info = create_info.push_next(&mut validation_features);
        }
        // SAFETY: all pointers in `create_info` refer to locals that outlive the call.
        let raw = unsafe { entry.create_instance(&create_info, None) }
            .map_err(RhiError::vk("vkCreateInstance"))?;
        pinning.finish();

        let messenger = if debug_state.is_some() {
            let utils = ext::debug_utils::Instance::new(&entry, &raw);
            // SAFETY: valid create info; the user data (`debug_state`) lives in the shared
            // state for as long as the messenger.
            match unsafe { utils.create_debug_utils_messenger(&messenger_info, None) } {
                Ok(messenger) => Some((utils, messenger)),
                Err(result) => {
                    // SAFETY: nothing else was created from the instance yet.
                    unsafe { raw.destroy_instance(None) };
                    return Err(RhiError::Vulkan {
                        call: "vkCreateDebugUtilsMessengerEXT",
                        result,
                    });
                }
            }
        } else {
            None
        };

        INSTANCES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live += 1;
        Ok(Self {
            shared: Arc::new(InstanceShared {
                entry,
                raw,
                debug_utils: messenger,
                debug_state,
                surface_enabled: surface,
                win32_surface_enabled: win32_surface,
                headless_surface_enabled: headless_surface,
                swapchain_colorspace_enabled: swapchain_colorspace,
                debug_utils_enabled: debug_utils,
                extra_extensions,
            }),
        })
    }

    /// The ash instance, for calls this layer does not wrap.
    pub fn raw(&self) -> &ash::Instance {
        &self.shared.raw
    }

    /// The ash entry (global commands).
    pub fn entry(&self) -> &ash::Entry {
        &self.shared.entry
    }

    /// Whether `name`, passed to [`Self::with_extensions`], was enabled.
    pub fn extra_extension_enabled(&self, name: &CStr) -> bool {
        self.shared
            .extra_extensions
            .iter()
            .any(|e| e.as_c_str() == name)
    }

    /// Whether the surface extensions are enabled.
    pub fn surface_enabled(&self) -> bool {
        self.shared.surface_enabled
    }

    /// Whether `VK_EXT_swapchain_colorspace` is enabled: surfaces may offer HDR color
    /// spaces ([`OutputColor`](crate::OutputColor)).
    pub fn hdr_color_spaces_enabled(&self) -> bool {
        self.shared.swapchain_colorspace_enabled
    }

    /// Validation messages since creation.
    pub fn validation_report(&self) -> ValidationReport {
        match &self.shared.debug_state {
            None => ValidationReport::default(),
            Some(state) => ValidationReport {
                enabled: true,
                errors: state.errors.load(Ordering::Relaxed),
                warnings: state.warnings.load(Ordering::Relaxed),
                first_errors: state
                    .first_errors
                    .lock()
                    .map(|v| v.clone())
                    .unwrap_or_default(),
            },
        }
    }

    /// All physical devices, suitable or not.
    pub fn adapters(&self) -> Result<Vec<Adapter>, RhiError> {
        // SAFETY: the instance is alive.
        let physical = unsafe { self.shared.raw.enumerate_physical_devices() }
            .map_err(RhiError::vk("vkEnumeratePhysicalDevices"))?;
        Ok(physical
            .into_iter()
            .map(|p| Adapter::query(self, p))
            .collect())
    }

    /// Picks the best suitable adapter for `preference`. The `ENGINE_ADAPTER` environment
    /// variable overrides the preference (see [`AdapterPreference::from_env`]).
    pub fn select_adapter(&self, preference: &AdapterPreference) -> Result<Adapter, RhiError> {
        let adapters = self.adapters()?;
        let env = AdapterPreference::from_env();
        let preference = env.as_ref().unwrap_or(preference);
        let unsuitable: Vec<(String, Vec<&'static str>)> = adapters
            .iter()
            .filter(|a| !a.missing_features().is_empty())
            .map(|a| (a.info_name().to_owned(), a.missing_features().to_vec()))
            .collect();
        adapters
            .into_iter()
            .filter(|a| a.missing_features().is_empty())
            .max_by_key(|a| a.score(preference))
            .ok_or(RhiError::NoSuitableAdapter(unsuitable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_that_goes_beside_another_is_destroyed_with_the_last_one() {
        let desc = InstanceDesc {
            validation: false,
            surface: false,
            ..InstanceDesc::default()
        };
        // No Vulkan loader on the machine: nothing to test.
        let Ok(first) = Instance::new(&desc) else {
            return;
        };
        let second = Instance::new(&desc).expect("a second instance");
        assert_eq!(instance_counts(), (2, 0));
        // A clone keeps it alive; the last handle parks it while `second` lives.
        let clone = first.clone();
        drop(first);
        assert_eq!(instance_counts(), (2, 0));
        drop(clone);
        assert_eq!(instance_counts(), (1, 1));
        drop(second);
        assert_eq!(instance_counts(), (0, 0));
        // Alone, an instance is destroyed with its last handle, as before.
        let alone = Instance::new(&desc).expect("an instance");
        drop(alone);
        assert_eq!(instance_counts(), (0, 0));
    }
}
