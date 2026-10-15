//! The AV1 headers (the AOMedia AV1 Bitstream & Decoding Process Specification, 1.0.0
//! with errata 1): what a GPU's video decoder needs of a stream that it does not parse
//! itself (ADR 0100).
//!
//! Vulkan Video decodes an AV1 frame's tiles; the application parses the sequence and
//! frame headers into `StdVideoAV1*` structures, finds the tiles in the bitstream and
//! keeps the reference frames' state the specification's reference frame update process
//! describes. [`Parser`] does that from a stream's temporal units (a WebM block each).
//!
//! Written from the specification. NVIDIA's AV1 parser in the Khronos Vulkan Video samples
//! (Apache-2.0) was read for what Vulkan needs; no code was copied from it or from FFmpeg
//! (LGPL, ideas only). rav1d's decoding checks it in the tests.

pub mod bits;
pub mod frame;
pub mod obu;
pub mod parser;
pub mod sequence;

use core::fmt;

pub use frame::FrameHeader;
pub use parser::{Frame, Parser, TemporalUnit};
pub use sequence::SequenceHeader;

/// Why an AV1 stream could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Av1Error {
    /// The data ends inside a syntax element.
    Truncated,
    /// A value the specification does not allow.
    Corrupt(&'static str),
    /// A valid stream with something the engine does not decode (a profile, scalability).
    Unsupported(&'static str),
    /// A frame before any sequence header, or one referring to a frame never decoded.
    MissingReference,
}

impl fmt::Display for Av1Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("AV1 data ends early"),
            Self::Corrupt(what) => write!(f, "damaged AV1 stream: {what}"),
            Self::Unsupported(what) => write!(f, "unsupported AV1 stream: {what}"),
            Self::MissingReference => {
                f.write_str("an AV1 frame needs a header or reference frame that is missing")
            }
        }
    }
}

impl std::error::Error for Av1Error {}

/// `NUM_REF_FRAMES`: reference frame slots.
pub const NUM_REF_FRAMES: usize = 8;
/// `REFS_PER_FRAME`: references an inter frame names.
pub const REFS_PER_FRAME: usize = 7;
/// `TOTAL_REFS_PER_FRAME`: the intra frame and the seven references.
pub const TOTAL_REFS_PER_FRAME: usize = 8;
/// `PRIMARY_REF_NONE`.
pub const PRIMARY_REF_NONE: u8 = 7;
/// `MAX_SEGMENTS`: segments a frame may have.
pub const MAX_SEGMENTS: usize = 8;
/// `SEG_LVL_MAX`: features a segment may have.
pub const SEG_LVL_MAX: usize = 8;
/// `MAX_TILE_COLS`: tile columns a frame may have.
pub const MAX_TILE_COLS: usize = 64;
/// `MAX_TILE_ROWS`: tile rows a frame may have.
pub const MAX_TILE_ROWS: usize = 64;
