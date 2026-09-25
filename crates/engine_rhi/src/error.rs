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
    /// A Vulkan call failed.
    Vulkan {
        /// What was being done.
        call: &'static str,
        /// The result code.
        result: vk::Result,
    },
}

impl RhiError {
    pub(crate) fn vk(call: &'static str) -> impl FnOnce(vk::Result) -> Self {
        move |result| Self::Vulkan { call, result }
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
            Self::Vulkan { call, result } => write!(f, "{call} failed: {result}"),
        }
    }
}

impl std::error::Error for RhiError {}
