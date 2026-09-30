//! Compressed audio: the [`Decoder`] interface that codecs implement (Vorbis, Opus and FLAC
//! live in `engine_audio_decode`), decoding a whole file into a [`Clip`], and streaming
//! long sounds (ADR 0051).

use crate::clip::{Clip, ClipError};
use core::fmt;

/// Why decoding failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Not a format the decoder reads.
    NotRecognized,
    /// The data is damaged or cut short.
    Corrupt(String),
    /// A valid file with something the engine does not play (for example more than two
    /// channels).
    Unsupported(String),
    /// Reading the source failed.
    Io(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRecognized => f.write_str("not a recognized audio format"),
            Self::Corrupt(what) => write!(f, "damaged or truncated audio: {what}"),
            Self::Unsupported(what) => write!(f, "unsupported audio: {what}"),
            Self::Io(what) => write!(f, "reading audio failed: {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<std::io::Error> for DecodeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// A source of decoded samples: a compressed file being read. Decoders run off the audio
/// thread (on the game thread for [`Clip::from_decoder`], on the stream thread for
/// streams), so they may allocate and do I/O.
pub trait Decoder: Send + 'static {
    /// Sample rate in Hz.
    fn sample_rate(&self) -> u32;

    /// Channels: 1 or 2.
    fn channels(&self) -> u16;

    /// Length in frames, when the file says.
    fn frames(&self) -> Option<u64>;

    /// Decodes up to `out.len() / channels` frames into `out`, interleaved, and returns
    /// how many; 0 at the end.
    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError>;

    /// Continues from `frame` (0 is the start; exact to the frame).
    fn seek(&mut self, frame: u64) -> Result<(), DecodeError>;
}

impl Clip {
    /// Decodes everything `decoder` has into a clip (for short sounds: decoding once at
    /// load is cheaper than streaming them every time they play).
    pub fn from_decoder(decoder: &mut dyn Decoder) -> Result<Self, ClipError> {
        let channels = decoder.channels();
        let capacity = decoder.frames().map_or(0, |f| f as usize) * usize::from(channels);
        let mut samples = Vec::with_capacity(capacity);
        let mut chunk = vec![0.0f32; 4096 * usize::from(channels.max(1))];
        loop {
            let frames = decoder.read(&mut chunk).map_err(ClipError::Decode)?;
            if frames == 0 {
                break;
            }
            samples.extend_from_slice(&chunk[..frames * usize::from(channels)]);
        }
        Self::from_interleaved(decoder.sample_rate(), channels, &samples)
    }
}
