//! Decoded audio in memory.

use core::fmt;
use std::sync::Arc;

/// Decoded mono or stereo audio, shared by reference: cloning a clip and playing it is an
/// atomic increment, never a copy or an allocation. The renderer never drops the last
/// reference; finished voices hand their clip back to the game thread.
#[derive(Clone)]
pub struct Clip(pub(crate) Arc<ClipData>);

pub(crate) struct ClipData {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u8,
    pub(crate) frames: usize,
    /// Planar: all of channel 0, then all of channel 1.
    pub(crate) samples: Box<[f32]>,
}

/// Why a clip could not be created or decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipError {
    /// The sample rate is zero or the channel count is not 1 or 2.
    Format {
        /// Sample rate in Hz.
        sample_rate: u32,
        /// Channel count.
        channels: u16,
    },
    /// The sample count is not a multiple of the channel count, or there are no samples.
    Length,
    /// Not a RIFF/WAVE file.
    NotWav,
    /// A WAVE file without a `fmt ` or `data` chunk, or with a chunk cut short.
    Truncated,
    /// A WAVE encoding other than PCM (8/16/24/32-bit) or IEEE float (32/64-bit).
    UnsupportedEncoding {
        /// `wFormatTag` (or the extensible sub-format's tag).
        tag: u16,
        /// Bits per sample.
        bits: u16,
    },
    /// A compressed file could not be decoded.
    Decode(crate::stream::DecodeError),
}

impl fmt::Display for ClipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format {
                sample_rate,
                channels,
            } => write!(
                f,
                "unsupported format: {sample_rate} Hz, {channels} channels (1 or 2 channels at a non-zero rate)"
            ),
            Self::Length => f.write_str("no samples, or a partial frame at the end"),
            Self::NotWav => f.write_str("not a RIFF/WAVE file"),
            Self::Truncated => f.write_str("WAVE file without complete fmt and data chunks"),
            Self::Decode(e) => e.fmt(f),
            Self::UnsupportedEncoding { tag, bits } => write!(
                f,
                "unsupported WAVE encoding: format tag {tag:#06x}, {bits} bits (PCM 8/16/24/32 or float 32/64)"
            ),
        }
    }
}

impl std::error::Error for ClipError {}

impl Clip {
    /// Creates a clip from interleaved samples (`channels` = 1 or 2).
    pub fn from_interleaved(
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
    ) -> Result<Self, ClipError> {
        if sample_rate == 0 || !(1..=2).contains(&channels) {
            return Err(ClipError::Format {
                sample_rate,
                channels,
            });
        }
        let c = usize::from(channels);
        if samples.is_empty() || !samples.len().is_multiple_of(c) {
            return Err(ClipError::Length);
        }
        let frames = samples.len() / c;
        let mut planar = vec![0.0f32; samples.len()].into_boxed_slice();
        for (i, frame) in samples.chunks_exact(c).enumerate() {
            for (ch, &s) in frame.iter().enumerate() {
                planar[ch * frames + i] = s;
            }
        }
        Ok(Self(Arc::new(ClipData {
            sample_rate,
            channels: channels as u8,
            frames,
            samples: planar,
        })))
    }

    /// Decodes a RIFF/WAVE file: PCM 8/16/24/32-bit or IEEE float 32/64-bit, mono or
    /// stereo, including `WAVE_FORMAT_EXTENSIBLE`.
    pub fn from_wav(bytes: &[u8]) -> Result<Self, ClipError> {
        let (sample_rate, channels, interleaved) = crate::wav::decode(bytes)?;
        Self::from_interleaved(sample_rate, channels, &interleaved)
    }

    /// Sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.0.sample_rate
    }

    /// 1 (mono) or 2 (stereo).
    pub fn channels(&self) -> u16 {
        u16::from(self.0.channels)
    }

    /// Length in frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.0.frames
    }

    /// Length in seconds.
    pub fn duration(&self) -> f64 {
        self.0.frames as f64 / f64::from(self.0.sample_rate)
    }

    /// Samples of one channel (`channel` 0 or 1); a mono clip returns its only channel for both.
    pub fn channel(&self, channel: usize) -> &[f32] {
        self.0.channel(channel)
    }
}

impl ClipData {
    pub(crate) fn channel(&self, channel: usize) -> &[f32] {
        let c = if usize::from(self.channels) > channel {
            channel
        } else {
            0
        };
        &self.samples[c * self.frames..(c + 1) * self.frames]
    }
}

impl fmt::Debug for Clip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clip")
            .field("sample_rate", &self.0.sample_rate)
            .field("channels", &self.0.channels)
            .field("frames", &self.0.frames)
            .finish()
    }
}
