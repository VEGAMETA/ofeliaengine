//! FLAC through claxon.

use crate::{Pending, Source, check_channels, fill};
use engine_audio::{DecodeError, Decoder};

/// A FLAC decoder.
pub struct FlacDecoder<S: Source> {
    reader: Option<claxon::FlacReader<S>>,
    sample_rate: u32,
    channels: u16,
    frames: Option<u64>,
    /// 1 / 2^(bits − 1): integer samples to [−1, 1).
    scale: f32,
    buffer: Vec<i32>,
    pending: Pending,
}

fn corrupt(e: claxon::Error) -> DecodeError {
    match e {
        claxon::Error::IoError(io) => DecodeError::Io(io.to_string()),
        claxon::Error::FormatError(what) => DecodeError::Corrupt(what.to_owned()),
        claxon::Error::Unsupported(what) => DecodeError::Unsupported(what.to_owned()),
    }
}

impl<S: Source> FlacDecoder<S> {
    /// Reads the metadata of the FLAC stream in `source`.
    pub fn new(source: S) -> Result<Self, DecodeError> {
        let reader = claxon::FlacReader::new(source).map_err(corrupt)?;
        let info = reader.streaminfo();
        let channels = check_channels(info.channels as usize)?;
        Ok(Self {
            sample_rate: info.sample_rate,
            channels,
            frames: info.samples,
            scale: 1.0 / (1u64 << (info.bits_per_sample.clamp(1, 32) - 1)) as f32,
            reader: Some(reader),
            buffer: Vec::new(),
            pending: Pending::default(),
        })
    }
}

impl<S: Source> Decoder for FlacDecoder<S> {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn frames(&self) -> Option<u64> {
        self.frames
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
        engine_profiler::zone!("audio_decode::flac");
        let Some(reader) = self.reader.as_mut() else {
            return Ok(0);
        };
        let (scale, channels) = (self.scale, u32::from(self.channels));
        let buffer = &mut self.buffer;
        fill(
            out,
            usize::from(self.channels),
            &mut self.pending,
            |pending| {
                let mut blocks = reader.blocks();
                match blocks
                    .read_next_or_eof(core::mem::take(buffer))
                    .map_err(corrupt)?
                {
                    Some(block) => {
                        let frames = block.duration() as usize;
                        pending.samples.reserve(frames * channels as usize);
                        for i in 0..frames as u32 {
                            for c in 0..channels {
                                pending.samples.push(block.sample(c, i) as f32 * scale);
                            }
                        }
                        *buffer = block.into_buffer();
                        Ok(true)
                    }
                    None => Ok(false),
                }
            },
        )
    }

    fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        let reader = self
            .reader
            .take()
            .ok_or(DecodeError::Io("decoder failed".into()))?;
        let mut source = reader.into_inner();
        source.rewind()?;
        self.reader = Some(claxon::FlacReader::new(source).map_err(corrupt)?);
        self.pending.clear();
        self.pending.skip = frame;
        Ok(())
    }
}
