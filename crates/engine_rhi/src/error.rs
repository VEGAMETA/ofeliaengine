use ash::vk;
use core::fmt;

/// Errors from the RHI.
#[derive(Debug)]
pub enum RhiError {
    /// The Vulkan loader (`vulkan-1.dll`) could not be loaded: no driver installed.
    Loader(String),
    /// The loader or driver supports an older Vulkan version than 1.3.
    VersionTooOld {
        /// Supported version (`vk::make_api_version` encoding).
        found: u32,
    },
    /// No adapter supports the required features. One entry per adapter:
    /// `(name, missing features)`.
    NoSuitableAdapter(Vec<(String, Vec<&'static str>)>),
    /// No memory type allowed for the resource suits the requested location.
    NoMemoryType {
        /// `VkMemoryRequirements::memoryTypeBits`.
        type_bits: u32,
        /// Requested location.
        location: crate::memory::MemoryLocation,
    },
    /// A shader does not match the code using it (for example its push constant block is
    /// larger than the data pushed), or its SPIR-V could not be read.
    ShaderInterface(String),
    /// A Vulkan call failed.
    Vulkan {
        /// What was being done.
        call: &'static str,
        /// The result code.
        result: vk::Result,
    },
    /// Reading or writing a file failed (the pipeline cache).
    Io {
        /// What was being done.
        what: &'static str,
        /// The error.
        error: std::io::Error,
    },
}

impl RhiError {
    /// The swapchain must be recreated before the next frame: it no longer matches its
    /// surface (`ERROR_OUT_OF_DATE_KHR`), or it lost the monitor it held for itself
    /// (`ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT`, ADR 0099; recreation tries to take it
    /// again).
    pub fn is_out_of_date(&self) -> bool {
        matches!(
            self,
            Self::Vulkan {
                result: vk::Result::ERROR_OUT_OF_DATE_KHR
                    | vk::Result::ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT,
                ..
            }
        )
    }

    /// Maps a failed Vulkan call's result: `.map_err(RhiError::vk("vkCreateImage"))`.
    ///
    /// `VK_ERROR_DEVICE_LOST` also produces the GPU fault report (once per device loss;
    /// see [`crate::last_gpu_fault`]).
    pub fn vk(call: &'static str) -> impl FnOnce(vk::Result) -> Self {
        move |result| {
            if result == vk::Result::ERROR_DEVICE_LOST {
                crate::fault::device_lost(call);
            }
            Self::Vulkan { call, result }
        }
    }
}

impl fmt::Display for RhiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loader(e) => write!(f, "cannot load the Vulkan loader: {e}"),
            Self::VersionTooOld { found } => write!(
                f,
                "Vulkan 1.3 required, found {}.{}.{}",
                vk::api_version_major(*found),
                vk::api_version_minor(*found),
                vk::api_version_patch(*found)
            ),
            Self::NoSuitableAdapter(adapters) => {
                write!(f, "no GPU supports the required features")?;
                for (name, missing) in adapters {
                    write!(f, "; {name}: missing {}", missing.join(", "))?;
                }
                Ok(())
            }
            Self::NoMemoryType {
                type_bits,
                location,
            } => write!(
                f,
                "no memory type for {location:?} among the allowed types {type_bits:#b}"
            ),
            Self::ShaderInterface(what) => write!(f, "shader interface: {what}"),
            Self::Vulkan { call, result } => write!(f, "{call} failed: {result}"),
            Self::Io { what, error } => write!(f, "{what}: {error}"),
        }
    }
}

impl std::error::Error for RhiError {}
