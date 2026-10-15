//! Where a WebM file's video and audio frames are, read once when a video opens: playing
//! and seeking then go straight to the bytes, without walking the clusters again.

use crate::webm::{Codec, Track, TrackKind, Webm, WebmError};

/// A frame of a track: a video temporal unit or an audio packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Indexed {
    /// Its first byte in the file.
    pub offset: usize,
    /// Its size in bytes.
    pub size: usize,
    /// Its time in nanoseconds, as the file has it.
    pub time_ns: i64,
    /// Decoding may begin here.
    pub keyframe: bool,
}

impl Indexed {
    /// Its bytes in the file `data`.
    pub fn bytes<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.offset..self.offset + self.size]
    }
}

/// A file's AV1 video track and Opus audio track, frame by frame.
#[derive(Debug, Clone)]
pub struct FileIndex {
    /// The video track.
    pub video: Track,
    /// Its temporal units, in file order.
    pub units: Vec<Indexed>,
    /// The Opus track, if the file has one.
    pub audio: Option<Track>,
    /// Its packets (each laced frame on its own), in file order.
    pub packets: Vec<Indexed>,
    /// The duration in nanoseconds: the file's, or past the last unit.
    pub duration_ns: i64,
}

impl FileIndex {
    /// Reads `data`'s tracks and blocks.
    ///
    /// # Errors
    ///
    /// Not a WebM file, a damaged one, or one without an AV1 track.
    pub fn read(data: &[u8]) -> Result<Self, WebmError> {
        let webm = Webm::read(data)?;
        let video = webm
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video && t.codec == Codec::Av1)
            .cloned()
            .ok_or(WebmError::Unsupported("no AV1 video track"))?;
        let audio = webm
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Audio && t.codec == Codec::Opus)
            .cloned();
        let base = data.as_ptr() as usize;
        let (mut units, mut packets) = (Vec::new(), Vec::new());
        for block in webm.blocks() {
            let block = block?;
            let audio_track = audio.as_ref().map(|a| a.number);
            if block.track == video.number {
                let frame = block.data();
                units.push(Indexed {
                    offset: frame.as_ptr() as usize - base,
                    size: frame.len(),
                    time_ns: block.time_ns,
                    keyframe: block.keyframe,
                });
            } else if Some(block.track) == audio_track {
                for frame in block.frames() {
                    packets.push(Indexed {
                        offset: frame.as_ptr() as usize - base,
                        size: frame.len(),
                        time_ns: block.time_ns,
                        keyframe: true,
                    });
                }
            }
        }
        if units.is_empty() {
            return Err(WebmError::Corrupt("an AV1 track without frames"));
        }
        let frame_ns = if video.default_duration_ns > 0 {
            video.default_duration_ns as i64
        } else {
            units
                .windows(2)
                .map(|w| w[1].time_ns - w[0].time_ns)
                .filter(|&d| d > 0)
                .min()
                .unwrap_or(33_333_333)
        };
        let past_last = units.iter().map(|u| u.time_ns).max().unwrap_or(0) + frame_ns;
        let duration_ns = webm
            .duration_ns
            .map_or(past_last, |d| (d as i64).max(past_last));
        Ok(Self {
            video,
            units,
            audio,
            packets,
            duration_ns,
        })
    }

    /// The unit at or before `time_ns` that decoding may begin at (the stream's first
    /// unit when none is).
    pub fn keyframe_before(&self, time_ns: i64) -> usize {
        keyframe_before(&self.units, time_ns)
    }
}

/// The unit of `units` at or before `time_ns` that decoding may begin at (the first when
/// none is).
pub fn keyframe_before(units: &[Indexed], time_ns: i64) -> usize {
    let at = units.partition_point(|u| u.time_ns <= time_ns);
    units[..at.max(1).min(units.len())]
        .iter()
        .rposition(|u| u.keyframe)
        .unwrap_or(0)
}
