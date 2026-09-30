//! Ogg Vorbis through lewton's packet decoder, with the Ogg framing handled here so the
//! stream's first and last samples follow the granule positions exactly (Vorbis I
//! specification, A.2: the last page's granule position ends the stream; a first page
//! whose granule position is below the samples it decodes to starts later).

use crate::{Pending, Source, check_channels, fill};
use engine_audio::{DecodeError, Decoder};
use lewton::audio::{PreviousWindowRight, read_audio_packet_generic};
use lewton::header::{IdentHeader, SetupHeader};
use lewton::inside_ogg::read_headers;
use lewton::samples::InterleavedSamples;
use ogg::PacketReader;

/// An Ogg Vorbis decoder.
pub struct VorbisDecoder<S: Source> {
    reader: Option<PacketReader<S>>,
    ident: IdentHeader,
    setup: SetupHeader,
    serial: u32,
    window: PreviousWindowRight,
    channels: u16,
    /// Samples per channel decoded since the start, before the start trim (the granule
    /// position's clock).
    decoded: u64,
    /// Samples the first page decodes to beyond its granule position, dropped at the
    /// start.
    start_trim: u64,
    pending: Pending,
}

fn corrupt(e: lewton::VorbisError) -> DecodeError {
    match e {
        lewton::VorbisError::OggError(ogg::OggReadError::ReadError(io)) => {
            DecodeError::Io(io.to_string())
        }
        e => DecodeError::Corrupt(e.to_string()),
    }
}

fn corrupt_ogg(e: ogg::OggReadError) -> DecodeError {
    corrupt(lewton::VorbisError::OggError(e))
}

impl<S: Source> VorbisDecoder<S> {
    /// Reads the headers of the Ogg Vorbis stream in `source`.
    pub fn new(source: S) -> Result<Self, DecodeError> {
        let mut reader = PacketReader::new(source);
        let ((ident, _comments, setup), serial) = read_headers(&mut reader).map_err(corrupt)?;
        let channels = check_channels(usize::from(ident.audio_channels))?;
        let mut this = Self {
            reader: Some(reader),
            ident,
            setup,
            serial,
            window: PreviousWindowRight::new(),
            channels,
            decoded: 0,
            start_trim: 0,
            pending: Pending::default(),
        };
        this.start_trim = this.measure_start_trim()?;
        this.seek(0)?;
        Ok(this)
    }

    /// Decodes the first audio page and compares its length with its granule position.
    fn measure_start_trim(&mut self) -> Result<u64, DecodeError> {
        let mut decoded = 0u64;
        while let Some(packet) = self.next_packet()? {
            let samples = self.decode_packet(&packet.data)?;
            decoded += (samples.len() / usize::from(self.channels)) as u64;
            if packet.last_in_page() {
                // A first page that also ends the stream is trimmed at the end instead.
                if packet.last_in_stream() {
                    return Ok(0);
                }
                return Ok(decoded.saturating_sub(packet.absgp_page()));
            }
        }
        Ok(0)
    }

    fn next_packet(&mut self) -> Result<Option<ogg::Packet>, DecodeError> {
        let reader = self
            .reader
            .as_mut()
            .ok_or(DecodeError::Io("decoder failed".into()))?;
        loop {
            match reader.read_packet().map_err(corrupt_ogg)? {
                Some(p) if p.stream_serial() != self.serial => continue,
                other => return Ok(other),
            }
        }
    }

    fn decode_packet(&mut self, data: &[u8]) -> Result<Vec<f32>, DecodeError> {
        let samples: InterleavedSamples<f32> =
            read_audio_packet_generic(&self.ident, &self.setup, data, &mut self.window)
                .map_err(|e| corrupt(lewton::VorbisError::BadAudio(e)))?;
        Ok(samples.samples)
    }
}

impl<S: Source> Decoder for VorbisDecoder<S> {
    fn sample_rate(&self) -> u32 {
        self.ident.audio_sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn frames(&self) -> Option<u64> {
        None
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
        engine_profiler::zone!("audio_decode::vorbis");
        if self.reader.is_none() {
            return Ok(0);
        }
        let channels = usize::from(self.channels);
        let mut pending = core::mem::take(&mut self.pending);
        let result = fill(out, channels, &mut pending, |pending| {
            let Some(packet) = self.next_packet()? else {
                return Ok(false);
            };
            let samples = self.decode_packet(&packet.data)?;
            let mut frames = (samples.len() / channels) as u64;
            if packet.last_in_stream() {
                frames = frames.min(packet.absgp_page().saturating_sub(self.decoded));
            }
            self.decoded += frames;
            pending
                .samples
                .extend_from_slice(&samples[..frames as usize * channels]);
            Ok(true)
        });
        self.pending = pending;
        result
    }

    fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        // Back to the start and decode up to `frame`: exact, and free for loops to 0.
        let reader = self
            .reader
            .take()
            .ok_or(DecodeError::Io("decoder failed".into()))?;
        let mut source = reader.into_inner();
        source.rewind()?;
        let mut reader = PacketReader::new(source);
        read_headers(&mut reader).map_err(corrupt)?;
        self.reader = Some(reader);
        self.window = PreviousWindowRight::new();
        self.decoded = 0;
        self.pending.clear();
        self.pending.skip = self.start_trim + frame;
        Ok(())
    }
}
