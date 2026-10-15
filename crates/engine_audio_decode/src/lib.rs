//! Compressed audio for `engine_audio` (ADR 0051): Ogg Vorbis (lewton), Ogg Opus (the
//! vendored libopus) and FLAC (claxon), as [`engine_audio::Decoder`]s.
//!
//! ```no_run
//! // Short sounds: decode once when loading.
//! let bytes = std::fs::read("sounds/door.ogg")?;
//! let clip = engine_audio_decode::decode(bytes)?;
//!
//! // Long ones (music, ambience): stream from the file while playing.
//! let music = engine_audio_decode::open_file("music/theme.opus")?;
//! # let _ = (clip, music);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Every decoder produces mono or stereo `f32` samples at the file's rate (Opus: 48 kHz);
//! files with more channels are refused. Seeking is exact to the frame; it decodes from the
//! start of the file, so its cost grows with the position (ADR 0051 has the numbers).

mod flac;
mod opus;
mod opus_ffi;
mod vorbis;

use engine_audio::{Clip, ClipError, DecodeError, Decoder};
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::Path;

pub use flac::FlacDecoder;
pub use opus::{OpusDecoder, OpusPackets};
pub use vorbis::VorbisDecoder;

/// A source the decoders read from: a file, or bytes in memory.
pub trait Source: Read + Seek + Send + 'static {}
impl<T: Read + Seek + Send + 'static> Source for T {}

/// The formats this crate reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Vorbis in an Ogg container (`.ogg`).
    Vorbis,
    /// Opus in an Ogg container (`.opus`).
    Opus,
    /// FLAC (`.flac`).
    Flac,
}

/// Recognizes the format from the first bytes of a file (for Ogg, the first page's header
/// and the start of its first packet: 64 bytes are enough).
pub fn sniff(head: &[u8]) -> Option<Format> {
    if head.starts_with(b"fLaC") {
        return Some(Format::Flac);
    }
    if !head.starts_with(b"OggS") || head.len() < 27 {
        return None;
    }
    // The first packet starts after the page header and its segment table.
    let segments = usize::from(head[26]);
    let packet = head.get(27 + segments..)?;
    if packet.starts_with(b"\x01vorbis") {
        Some(Format::Vorbis)
    } else if packet.starts_with(b"OpusHead") {
        Some(Format::Opus)
    } else {
        None
    }
}

/// A decoder for `source`, whatever its format.
pub fn open<S: Source>(mut source: S) -> Result<Box<dyn Decoder>, DecodeError> {
    let mut head = [0u8; 128];
    let mut filled = 0;
    while filled < head.len() {
        let n = source.read(&mut head[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    source.rewind()?;
    Ok(
        match sniff(&head[..filled]).ok_or(DecodeError::NotRecognized)? {
            Format::Vorbis => Box::new(VorbisDecoder::new(source)?),
            Format::Opus => Box::new(OpusDecoder::new(source)?),
            Format::Flac => Box::new(FlacDecoder::new(source)?),
        },
    )
}

/// A decoder reading the file at `path` as it plays (for streaming).
pub fn open_file(path: impl AsRef<Path>) -> Result<Box<dyn Decoder>, DecodeError> {
    let file = std::fs::File::open(path)?;
    open(BufReader::with_capacity(64 * 1024, file))
}

/// A decoder over a file already in memory.
pub fn open_bytes(
    bytes: impl AsRef<[u8]> + Send + 'static,
) -> Result<Box<dyn Decoder>, DecodeError> {
    open(Cursor::new(bytes))
}

/// Decodes a whole file in memory into a [`Clip`].
pub fn decode(bytes: impl AsRef<[u8]> + Send + 'static) -> Result<Clip, ClipError> {
    engine_profiler::zone!("audio_decode::decode");
    let mut decoder = open_bytes(bytes).map_err(ClipError::Decode)?;
    Clip::from_decoder(decoder.as_mut())
}

/// Decoded interleaved samples waiting to be read, reused between packets.
#[derive(Debug, Default)]
struct Pending {
    samples: Vec<f32>,
    /// Samples already handed out.
    read: usize,
    /// Frames still to drop before handing out (seeking, Opus pre-skip).
    skip: u64,
}

impl Pending {
    fn clear(&mut self) {
        self.samples.clear();
        self.read = 0;
    }

    /// Drops the frames still to skip, then copies what fits in `out` (whole frames);
    /// returns the samples written.
    fn drain(&mut self, out: &mut [f32], channels: usize) -> usize {
        let available = (self.samples.len() - self.read) / channels;
        let skip = self.skip.min(available as u64) as usize;
        self.skip -= skip as u64;
        self.read += skip * channels;
        let n = (self.samples.len() - self.read).min(out.len() / channels * channels);
        out[..n].copy_from_slice(&self.samples[self.read..self.read + n]);
        self.read += n;
        if self.read == self.samples.len() {
            self.clear();
        }
        n
    }
}

/// Fills `out` from `pending`, calling `next` for another packet until `out` is full or
/// `next` reports the end; returns the frames written.
fn fill(
    out: &mut [f32],
    channels: usize,
    pending: &mut Pending,
    mut next: impl FnMut(&mut Pending) -> Result<bool, DecodeError>,
) -> Result<usize, DecodeError> {
    let mut written = 0;
    loop {
        written += pending.drain(&mut out[written..], channels);
        if written + channels > out.len() || !next(pending)? {
            break;
        }
    }
    Ok(written / channels)
}

/// The channel count, refused beyond stereo.
fn check_channels(channels: usize) -> Result<u16, DecodeError> {
    match channels {
        1 | 2 => Ok(channels as u16),
        n => Err(DecodeError::Unsupported(format!(
            "{n} channels (mono or stereo only)"
        ))),
    }
}
