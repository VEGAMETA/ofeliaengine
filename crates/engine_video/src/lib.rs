//! Video playback (ADR 0100): AV1 in WebM, decoded by the GPU's video engine through
//! Vulkan Video, or on worker threads by rav1d where the GPU cannot; the picture as a
//! texture, kept in time with its Opus sound.
//!
//! - [`webm`]: the container, read from memory.
//! - [`av1`]: the AV1 headers the GPU's decoder needs, parsed from a stream's temporal
//!   units.
//! - [`gpu`]: decoding on the GPU's video engine (Vulkan Video).
//! - [`software`]: rav1d's decoding (the `software` feature, on by default).
//! - [`index`]: where a file's video units and audio packets are.
//! - [`audio`]: the Opus track as an audio stream, its position the clock.
//! - [`convert`]: the pictures as a texture (the conversion pass from Y'CbCr).
//! - [`player`]: playing a video: decoding ahead, the clock, the controls.

pub mod audio;
pub mod av1;
pub mod convert;
pub mod gpu;
pub mod index;
pub mod player;
#[cfg(feature = "software")]
pub mod software;
pub mod webm;
