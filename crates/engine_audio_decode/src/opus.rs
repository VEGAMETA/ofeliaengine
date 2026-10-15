//! Ogg Opus (RFC 7845) through the vendored libopus: the `ogg` crate reads the pages,
//! libopus decodes the packets.

use crate::opus_ffi as ffi;
use crate::{Pending, Source, check_channels, fill};
use engine_audio::{DecodeError, Decoder};
use ogg::PacketReader;

/// Opus always decodes at 48 kHz.
const RATE: u32 = 48_000;
/// The longest Opus packet: 120 ms at 48 kHz.
const MAX_FRAME: usize = 5_760;

/// An Ogg Opus decoder.
pub struct OpusDecoder<S: Source> {
    reader: Option<PacketReader<S>>,
    decoder: *mut ffi::OpusDecoder,
    channels: u16,
    pre_skip: u64,
    /// The header's output gain as a linear factor.
    gain: f32,
    /// Samples per channel decoded since the start of the stream, pre-skip included (the
    /// granule position's clock).
    decoded: u64,
    pcm: Vec<f32>,
    pending: Pending,
}

// SAFETY: the libopus decoder is plain memory owned by this value, with no thread
// affinity; it is only used through `&mut self`.
unsafe impl<S: Source> Send for OpusDecoder<S> {}

fn corrupt(e: ogg::OggReadError) -> DecodeError {
    match e {
        ogg::OggReadError::ReadError(io) => DecodeError::Io(io.to_string()),
        e => DecodeError::Corrupt(e.to_string()),
    }
}

/// The `OpusHead` header: channels, pre-skip and output gain (RFC 7845 §5.1).
fn parse_head(packet: &[u8]) -> Result<(u16, u64, f32), DecodeError> {
    if packet.len() < 19 || !packet.starts_with(b"OpusHead") {
        return Err(DecodeError::Corrupt("missing OpusHead".into()));
    }
    let version = packet[8];
    if version >> 4 != 0 {
        return Err(DecodeError::Unsupported(format!(
            "Ogg Opus version {version}"
        )));
    }
    let channels = check_channels(usize::from(packet[9]))?;
    let pre_skip = u64::from(u16::from_le_bytes([packet[10], packet[11]]));
    let gain_q8 = i16::from_le_bytes([packet[16], packet[17]]);
    let mapping = packet[18];
    if mapping != 0 {
        return Err(DecodeError::Unsupported(format!(
            "channel mapping family {mapping}"
        )));
    }
    // Q7.8 decibels.
    let gain = 10f32.powf(f32::from(gain_q8) / (20.0 * 256.0));
    Ok((channels, pre_skip, gain))
}

impl<S: Source> OpusDecoder<S> {
    /// Reads the headers of the Ogg Opus stream in `source`.
    pub fn new(source: S) -> Result<Self, DecodeError> {
        let mut reader = PacketReader::new(source);
        let head = reader
            .read_packet()
            .map_err(corrupt)?
            .ok_or_else(|| DecodeError::Corrupt("empty stream".into()))?;
        let (channels, pre_skip, gain) = parse_head(&head.data)?;
        let mut error = 0;
        // SAFETY: plain constructor; the result is checked before use.
        let decoder =
            unsafe { ffi::opus_decoder_create(RATE as i32, i32::from(channels), &mut error) };
        if decoder.is_null() || error != ffi::OPUS_OK {
            return Err(DecodeError::Corrupt(format!(
                "libopus refused the stream ({error})"
            )));
        }
        let mut this = Self {
            reader: Some(reader),
            decoder,
            channels,
            pre_skip,
            gain,
            decoded: 0,
            pcm: vec![0.0; MAX_FRAME * usize::from(channels)],
            pending: Pending::default(),
        };
        this.skip_tags()?;
        this.pending.skip = pre_skip;
        Ok(this)
    }

    /// Skips the `OpusTags` header that follows `OpusHead`.
    fn skip_tags(&mut self) -> Result<(), DecodeError> {
        let reader = self
            .reader
            .as_mut()
            .ok_or(DecodeError::Io("decoder failed".into()))?;
        let tags = reader
            .read_packet()
            .map_err(corrupt)?
            .ok_or_else(|| DecodeError::Corrupt("missing OpusTags".into()))?;
        if !tags.data.starts_with(b"OpusTags") {
            return Err(DecodeError::Corrupt("missing OpusTags".into()));
        }
        Ok(())
    }
}

impl<S: Source> Drop for OpusDecoder<S> {
    fn drop(&mut self) {
        // SAFETY: created by `opus_decoder_create` and destroyed once.
        unsafe { ffi::opus_decoder_destroy(self.decoder) };
    }
}

impl<S: Source> Decoder for OpusDecoder<S> {
    fn sample_rate(&self) -> u32 {
        RATE
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn frames(&self) -> Option<u64> {
        None
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
        engine_profiler::zone!("audio_decode::opus");
        let Some(reader) = self.reader.as_mut() else {
            return Ok(0);
        };
        let channels = usize::from(self.channels);
        let (decoder, pcm, decoded, gain) =
            (self.decoder, &mut self.pcm, &mut self.decoded, self.gain);
        fill(out, channels, &mut self.pending, |pending| {
            let Some(packet) = reader.read_packet().map_err(corrupt)? else {
                return Ok(false);
            };
            let len = i32::try_from(packet.data.len())
                .map_err(|_| DecodeError::Corrupt("packet too long".into()))?;
            // SAFETY: `decoder` is live; `pcm` holds MAX_FRAME frames of `channels`.
            let frames = unsafe {
                ffi::opus_decode_float(
                    decoder,
                    packet.data.as_ptr(),
                    len,
                    pcm.as_mut_ptr(),
                    MAX_FRAME as i32,
                    0,
                )
            };
            if frames < 0 {
                return Err(DecodeError::Corrupt(format!("libopus error {frames}")));
            }
            let mut frames = frames as u64;
            // The last page's granule position ends the stream exactly (RFC 7845 §4.5).
            if packet.last_in_stream() {
                frames = frames.min(packet.absgp_page().saturating_sub(*decoded));
            }
            *decoded += frames;
            let samples = &pcm[..frames as usize * channels];
            if gain == 1.0 {
                pending.samples.extend_from_slice(samples);
            } else {
                pending.samples.extend(samples.iter().map(|s| s * gain));
            }
            Ok(true)
        })
    }

    fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        let reader = self
            .reader
            .take()
            .ok_or(DecodeError::Io("decoder failed".into()))?;
        let mut source = reader.into_inner();
        source.rewind()?;
        let mut reader = PacketReader::new(source);
        reader.read_packet().map_err(corrupt)?; // OpusHead, already parsed
        self.reader = Some(reader);
        self.skip_tags()?;
        // SAFETY: `decoder` is live; OPUS_RESET_STATE takes no argument.
        unsafe { ffi::opus_decoder_ctl(self.decoder, ffi::OPUS_RESET_STATE) };
        self.decoded = 0;
        self.pending.clear();
        self.pending.skip = self.pre_skip + frame;
        Ok(())
    }
}

/// Raw Opus packets, as a WebM track holds them (`A_OPUS`), decoded one at a time by
/// libopus. The track's codec private data is the `OpusHead` header: channels, pre-skip
/// (the frames to drop at the stream's start) and output gain.
pub struct OpusPackets {
    decoder: *mut ffi::OpusDecoder,
    channels: u16,
    pre_skip: u64,
    gain: f32,
}

// SAFETY: as `OpusDecoder`: plain memory owned by this value, used through `&mut self`.
unsafe impl Send for OpusPackets {}

impl core::fmt::Debug for OpusPackets {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpusPackets")
            .field("channels", &self.channels)
            .field("pre_skip", &self.pre_skip)
            .finish_non_exhaustive()
    }
}

impl OpusPackets {
    /// The longest packet decoded: 120 ms at 48 kHz, in frames.
    pub const MAX_FRAMES: usize = MAX_FRAME;

    /// A decoder for the stream `head` (an `OpusHead` header) describes.
    ///
    /// # Errors
    ///
    /// A damaged header, more than two channels, or a mapping family other than 0.
    pub fn new(head: &[u8]) -> Result<Self, DecodeError> {
        let (channels, pre_skip, gain) = parse_head(head)?;
        let mut error = 0;
        // SAFETY: plain constructor; the result is checked before use.
        let decoder =
            unsafe { ffi::opus_decoder_create(RATE as i32, i32::from(channels), &mut error) };
        if decoder.is_null() || error != ffi::OPUS_OK {
            return Err(DecodeError::Corrupt(format!(
                "libopus refused the stream ({error})"
            )));
        }
        Ok(Self {
            decoder,
            channels,
            pre_skip,
            gain,
        })
    }

    /// The channels: 1 or 2.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Frames to drop at the stream's start (the encoder's delay).
    pub fn pre_skip(&self) -> u64 {
        self.pre_skip
    }

    /// Opus always decodes at 48 kHz.
    pub fn sample_rate(&self) -> u32 {
        RATE
    }

    /// Decodes `packet` into `out` (interleaved, with the header's gain), at most
    /// [`Self::MAX_FRAMES`] frames, and returns how many.
    ///
    /// # Errors
    ///
    /// A damaged packet, or `out` shorter than [`Self::MAX_FRAMES`] frames.
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, DecodeError> {
        let channels = usize::from(self.channels);
        if out.len() < MAX_FRAME * channels {
            return Err(DecodeError::Unsupported(
                "an output shorter than the longest packet".into(),
            ));
        }
        let len = i32::try_from(packet.len())
            .map_err(|_| DecodeError::Corrupt("packet too long".into()))?;
        // SAFETY: the decoder is live; `out` holds MAX_FRAME frames of `channels`.
        let frames = unsafe {
            ffi::opus_decode_float(
                self.decoder,
                packet.as_ptr(),
                len,
                out.as_mut_ptr(),
                MAX_FRAME as i32,
                0,
            )
        };
        if frames < 0 {
            return Err(DecodeError::Corrupt(format!("libopus error {frames}")));
        }
        let frames = frames as usize;
        if self.gain != 1.0 {
            for sample in &mut out[..frames * channels] {
                *sample *= self.gain;
            }
        }
        Ok(frames)
    }

    /// The frames `packet` decodes to, read from its header without decoding it.
    ///
    /// # Errors
    ///
    /// A damaged packet.
    pub fn packet_frames(packet: &[u8]) -> Result<usize, DecodeError> {
        let len = i32::try_from(packet.len())
            .map_err(|_| DecodeError::Corrupt("packet too long".into()))?;
        // SAFETY: the pointer and length describe `packet`; libopus only reads it.
        let frames = unsafe { ffi::opus_packet_get_nb_samples(packet.as_ptr(), len, RATE as i32) };
        usize::try_from(frames).map_err(|_| DecodeError::Corrupt(format!("libopus error {frames}")))
    }

    /// Forgets what the decoder heard (decoding continues elsewhere, after a seek).
    pub fn reset(&mut self) {
        // SAFETY: the decoder is live; OPUS_RESET_STATE takes no argument.
        unsafe { ffi::opus_decoder_ctl(self.decoder, ffi::OPUS_RESET_STATE) };
    }
}

impl Drop for OpusPackets {
    fn drop(&mut self) {
        // SAFETY: created by `opus_decoder_create` and destroyed once.
        unsafe { ffi::opus_decoder_destroy(self.decoder) };
    }
}
