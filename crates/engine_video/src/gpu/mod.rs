//! Decoding on the GPU's video engine with Vulkan Video (ADR 0100): the AV1 decode
//! session, its parameters, the decoded picture buffer and the decode commands, on the
//! device's decode queue (`engine_rhi::Device::video_decode`, asked for with
//! `ExtraQueues::video_decode`).
//!
//! Both of Vulkan Video's arrangements are handled:
//! - **coinciding** (NVIDIA): a frame is decoded into its reference picture, then copied
//!   into an output image on the decode queue (its family copies too);
//! - **distinct** (AMD): a frame is decoded into an output image, its reference picture
//!   written beside it.
//!
//! Written from the Vulkan specification (the video coding and AV1 decode chapters,
//! read 2026-10-16); the frame data comes from [`crate::av1::Parser`].

pub mod caps;
mod decoder;
mod std_av1;

use core::fmt;

use engine_rhi::RhiError;
use engine_rhi::ash::vk;

pub use caps::GpuCaps;
pub use decoder::{GpuDecoder, GpuDecoderDesc, GpuPicture};

/// Why the GPU could not decode.
#[derive(Debug)]
pub enum GpuError {
    /// The device has no AV1 decode queue (asked for with `ExtraQueues::video_decode`, on
    /// a GPU that decodes AV1).
    NoDecodeQueue,
    /// The stream needs what the device's decoder lacks (profile, depth, level, size): the
    /// software decoder takes it.
    Unsupported(&'static str),
    /// A frame needs a reference the decoder has not decoded (a stream started or sought
    /// anywhere but at a key frame).
    MissingReference,
    /// Vulkan refused a call.
    Vulkan {
        /// The call.
        call: &'static str,
        /// What it returned.
        result: vk::Result,
    },
    /// Memory for the session or its pictures.
    Rhi(RhiError),
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDecodeQueue => f.write_str("the device has no AV1 decode queue"),
            Self::Unsupported(what) => write!(f, "the GPU's AV1 decoder does not take {what}"),
            Self::MissingReference => f.write_str("an AV1 frame needs a reference not decoded yet"),
            Self::Vulkan { call, result } => write!(f, "{call} failed: {result}"),
            Self::Rhi(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for GpuError {}

impl From<RhiError> for GpuError {
    fn from(error: RhiError) -> Self {
        Self::Rhi(error)
    }
}

/// A Vulkan result as a [`GpuError`].
pub(crate) fn vk_error(call: &'static str) -> impl FnOnce(vk::Result) -> GpuError {
    move |result| GpuError::Vulkan { call, result }
}
