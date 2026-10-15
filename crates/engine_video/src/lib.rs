//! Video playback (ADR 0100): AV1 in WebM, decoded by the GPU's video engine through
//! Vulkan Video, or on worker threads by rav1d where the GPU cannot; the picture as a
//! texture, kept in time with its Opus sound.
//!
//! - [`webm`]: the container, read from memory.
//! - [`av1`]: the AV1 headers the GPU's decoder needs, parsed from a stream's temporal
//!   units.
//! - [`software`]: rav1d's decoding (the `software` feature, on by default).

pub mod av1;
#[cfg(feature = "software")]
pub mod software;
pub mod webm;
