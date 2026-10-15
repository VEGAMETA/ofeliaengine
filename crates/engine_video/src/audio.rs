//! A video's Opus track as an audio stream (`engine_audio::Decoder`): the game plays it
//! with `Audio::play_stream`, and its position in the mix is the video's clock.
//!
//! The stream's time 0 is its first block's time: the codec's delay (`OpusHead`'s
//! pre-skip, WebM's `CodecDelay`) is decoded and dropped. Seeks count the packets' own
//! lengths from the start rather than trusting the blocks' times, which muxers round to
//! the millisecond.

use std::sync::Arc;

use engine_audio::{DecodeError, Decoder};
use engine_audio_decode::OpusPackets;

use crate::index::Indexed;

/// Opus's rate.
const RATE: u64 = 48_000;
/// Frames decoded before a seek target, for the decoder to converge (WebM's
/// `SeekPreRoll` for Opus: 80 ms).
const PRE_ROLL: u64 = RATE * 80 / 1000;

/// The sound of a video, decoded packet by packet from the file in memory.
pub struct VideoAudio {
    data: Arc<[u8]>,
    packets: Arc<[Indexed]>,
    /// Each packet's first frame, counted from the stream's first decoded frame (the
    /// codec's delay included).
    starts: Arc<[u64]>,
    /// The packet to decode next.
    next: usize,
    opus: OpusPackets,
    /// Decoded frames, `pending.0..pending.1` of them not read yet.
    pcm: Vec<f32>,
    pending: (usize, usize),
    /// Frames still to drop (the codec's delay, the part before a seek target).
    skip: u64,
}

impl core::fmt::Debug for VideoAudio {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VideoAudio")
            .field("packets", &self.packets.len())
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

impl VideoAudio {
    /// The Opus track whose `OpusHead` is `head` (its codec private data) and whose packets
    /// are `packets` in `data`.
    ///
    /// # Errors
    ///
    /// A header libopus refuses, or a damaged packet.
    pub fn new(data: Arc<[u8]>, packets: Arc<[Indexed]>, head: &[u8]) -> Result<Self, DecodeError> {
        let opus = OpusPackets::new(head)?;
        let mut starts = Vec::with_capacity(packets.len());
        let mut at = 0u64;
        for packet in packets.iter() {
            starts.push(at);
            at += OpusPackets::packet_frames(packet.bytes(&data))? as u64;
        }
        let channels = usize::from(opus.channels());
        Ok(Self {
            skip: opus.pre_skip(),
            data,
            packets,
            starts: starts.into(),
            next: 0,
            pcm: vec![0.0; OpusPackets::MAX_FRAMES * channels],
            opus,
            pending: (0, 0),
        })
    }

    /// The stream's length in frames (the codec's delay left out).
    pub fn length(&self) -> u64 {
        let end = self.starts.last().map_or(0, |&s| {
            s + self
                .packets
                .last()
                .and_then(|p| OpusPackets::packet_frames(p.bytes(&self.data)).ok())
                .unwrap_or(0) as u64
        });
        end.saturating_sub(self.opus.pre_skip())
    }
}

impl Decoder for VideoAudio {
    fn sample_rate(&self) -> u32 {
        RATE as u32
    }

    fn channels(&self) -> u16 {
        self.opus.channels()
    }

    fn frames(&self) -> Option<u64> {
        Some(self.length())
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
        let channels = usize::from(self.opus.channels());
        let wanted = out.len() / channels;
        let mut written = 0;
        while written < wanted {
            if self.pending.0 == self.pending.1 {
                let Some(packet) = self.packets.get(self.next) else {
                    break;
                };
                self.next += 1;
                let frames = self.opus.decode(packet.bytes(&self.data), &mut self.pcm)?;
                let dropped = (self.skip as usize).min(frames);
                self.skip -= dropped as u64;
                self.pending = (dropped, frames);
                continue;
            }
            let take = (self.pending.1 - self.pending.0).min(wanted - written);
            let from = self.pending.0 * channels;
            out[written * channels..(written + take) * channels]
                .copy_from_slice(&self.pcm[from..from + take * channels]);
            self.pending.0 += take;
            written += take;
        }
        Ok(written)
    }

    fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        // The frame counted from the first decoded one, and where decoding begins.
        let target = frame + self.opus.pre_skip();
        let begin = target.saturating_sub(PRE_ROLL);
        self.next = self
            .starts
            .partition_point(|&s| s <= begin)
            .saturating_sub(1);
        self.opus.reset();
        self.pending = (0, 0);
        self.skip = target - self.starts.get(self.next).copied().unwrap_or(0);
        Ok(())
    }
}
