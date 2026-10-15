//! WebM files (ADR 0100): the subset of Matroska (RFC 9559, on EBML, RFC 8794) that WebM
//! uses, read from memory.
//!
//! A [`Webm`] reads the header, the segment's `Info` (the timestamp scale, the duration),
//! its `Tracks` (codec, private data, picture size, sample rate) and its `Cues`, and
//! [`Webm::blocks`] walks its clusters' blocks in file order, each with its track, its
//! timestamp in nanoseconds and its frames (laced blocks split: Opus may lace).
//!
//! Clusters of unknown size (live streams) end where the next top-level element begins.
//! Elements the engine has no use for (tags, chapters, attachments, the seek head) are
//! skipped.

use core::fmt;

/// Why a WebM file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebmError {
    /// Not an EBML file, or not a WebM / Matroska one.
    NotWebm,
    /// The data ends inside an element, or an element is malformed.
    Corrupt(&'static str),
    /// A valid file with something the engine does not read.
    Unsupported(&'static str),
}

impl fmt::Display for WebmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotWebm => f.write_str("not a WebM file"),
            Self::Corrupt(what) => write!(f, "damaged WebM file: {what}"),
            Self::Unsupported(what) => write!(f, "unsupported WebM file: {what}"),
        }
    }
}

impl std::error::Error for WebmError {}

// Element ids (RFC 9559), with their length markers, as they are written.
const EBML: u32 = 0x1A45_DFA3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const CODEC_DELAY: u32 = 0x56AA;
const SEEK_PRE_ROLL: u32 = 0x56BB;
const DEFAULT_DURATION: u32 = 0x23_E383;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const REFERENCE_BLOCK: u32 = 0xFB;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;

/// Top-level elements of a segment: an element of unknown size ends where one begins.
const SEGMENT_CHILDREN: [u32; 8] = [
    0x114D_9B74, // SeekHead
    INFO,
    TRACKS,
    CLUSTER,
    CUES,
    0x1941_A469, // Attachments
    0x1043_A770, // Chapters
    0x1254_C367, // Tags
];

/// What a track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    /// Pictures.
    Video,
    /// Sound.
    Audio,
    /// Subtitles, buttons and the like, which the engine does not play.
    Other,
}

/// A track's codec, by its Matroska codec id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    /// `V_AV1`: blocks are AV1 temporal units (OBUs with their sizes).
    Av1,
    /// `A_OPUS`: blocks are Opus packets.
    Opus,
    /// Anything else, by its id.
    Other(String),
}

/// A track of the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    /// Its number, as blocks name it.
    pub number: u64,
    /// What it carries.
    pub kind: TrackKind,
    /// How its blocks are coded.
    pub codec: Codec,
    /// Codec private data: AV1's configuration record (with the sequence header), Opus's
    /// `OpusHead`.
    pub private: Vec<u8>,
    /// Video: the picture's width in pixels.
    pub width: u32,
    /// Video: the picture's height in pixels.
    pub height: u32,
    /// Audio: samples per second.
    pub sample_rate: f64,
    /// Audio: the channels.
    pub channels: u32,
    /// The codec's delay (Opus's pre-skip), nanoseconds.
    pub codec_delay_ns: u64,
    /// How long before a seek target decoding must begin (Opus: 80 ms), nanoseconds.
    pub seek_pre_roll_ns: u64,
    /// The duration of each frame when constant, nanoseconds (0: not given).
    pub default_duration_ns: u64,
}

impl Track {
    fn new() -> Self {
        Self {
            number: 0,
            kind: TrackKind::Other,
            codec: Codec::Other(String::new()),
            private: Vec::new(),
            width: 0,
            height: 0,
            sample_rate: 8000.0,
            channels: 1,
            codec_delay_ns: 0,
            seek_pre_roll_ns: 0,
            default_duration_ns: 0,
        }
    }
}

/// A seek point: the cluster holding a keyframe of `track` at `time_ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CuePoint {
    /// The keyframe's time in nanoseconds.
    pub time_ns: u64,
    /// The track whose keyframe it is.
    pub track: u64,
    /// The cluster's offset from the segment's data.
    pub cluster_offset: u64,
}

/// A block of a cluster: one track's frame (or frames, when laced) at one time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block<'a> {
    /// The track's number.
    pub track: u64,
    /// The cluster's timestamp plus the block's, scaled to nanoseconds.
    pub time_ns: i64,
    /// A keyframe: decoding may begin here (SimpleBlock's flag; a BlockGroup without a
    /// reference block).
    pub keyframe: bool,
    /// The block's frames: one, or several for a laced block.
    frames: Frames<'a>,
    /// The block's offset in the file (for messages).
    pub offset: usize,
}

impl<'a> Block<'a> {
    /// The block's frames, in order.
    pub fn frames(&self) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.frames.iter()
    }

    /// The block's only frame, or its first when laced.
    pub fn data(&self) -> &'a [u8] {
        self.frames.iter().next().unwrap_or(&[])
    }

    /// How many frames the block carries.
    pub fn frame_count(&self) -> usize {
        self.frames.count
    }
}

/// A block's frames: up to [`MAX_LACED`] laced frames kept by offset, so a block needs no
/// allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frames<'a> {
    data: &'a [u8],
    /// The frames' ends in `data` (the first starts at 0).
    ends: [u32; MAX_LACED],
    count: usize,
}

/// Frames a laced block may hold here (Opus blocks lace few; more are refused).
const MAX_LACED: usize = 16;

impl<'a> Frames<'a> {
    fn iter(&self) -> impl Iterator<Item = &'a [u8]> + '_ {
        let data = self.data;
        (0..self.count).map(move |i| {
            let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
            &data[start..self.ends[i] as usize]
        })
    }
}

/// A WebM file read from memory.
#[derive(Debug, Clone)]
pub struct Webm<'a> {
    data: &'a [u8],
    /// The segment's data: offsets of cues are from its start.
    segment: core::ops::Range<usize>,
    /// Nanoseconds per timestamp unit.
    pub timestamp_scale: u64,
    /// The duration in nanoseconds, if given.
    pub duration_ns: Option<f64>,
    /// The tracks, in file order.
    pub tracks: Vec<Track>,
    /// The seek points, in file order (by time, as muxers write them).
    pub cues: Vec<CuePoint>,
    /// Where the first cluster begins.
    first_cluster: Option<usize>,
}

impl<'a> Webm<'a> {
    /// Reads `data`'s header, info, tracks and cues.
    ///
    /// # Errors
    ///
    /// Not a WebM file, a damaged one, or one without a segment.
    pub fn read(data: &'a [u8]) -> Result<Self, WebmError> {
        let mut at = 0;
        let header = element(data, &mut at)?;
        if header.id != EBML {
            return Err(WebmError::NotWebm);
        }
        let header_end = header.end(data.len())?;
        let mut inner = header.data_start;
        while inner < header_end {
            let child = element(&data[..header_end], &mut inner)?;
            let end = child.end(header_end)?;
            if child.id == DOC_TYPE {
                let doc = &data[child.data_start..end];
                if doc != b"webm" && doc != b"matroska" {
                    return Err(WebmError::NotWebm);
                }
            }
            inner = end;
        }
        at = header_end;
        // The segment (the first one: WebM has one).
        let segment = loop {
            if at >= data.len() {
                return Err(WebmError::Corrupt("no segment"));
            }
            let e = element(data, &mut at)?;
            if e.id == SEGMENT {
                break e;
            }
            at = e.end(data.len())?;
        };
        let segment_end = segment.end(data.len())?;
        let mut webm = Self {
            data,
            segment: segment.data_start..segment_end,
            timestamp_scale: 1_000_000,
            duration_ns: None,
            tracks: Vec::new(),
            cues: Vec::new(),
            first_cluster: None,
        };
        let mut at = segment.data_start;
        let mut duration = None;
        while at < segment_end {
            let start = at;
            let e = element(&data[..segment_end], &mut at)?;
            let end = if e.unknown_size {
                next_sibling(data, e.data_start, segment_end, &SEGMENT_CHILDREN)?
            } else {
                e.end(segment_end)?
            };
            match e.id {
                INFO => {
                    let mut inner = e.data_start;
                    while inner < end {
                        let c = element(&data[..end], &mut inner)?;
                        let c_end = c.end(end)?;
                        let body = &data[c.data_start..c_end];
                        match c.id {
                            TIMESTAMP_SCALE => webm.timestamp_scale = uint(body)?.max(1),
                            DURATION => duration = Some(float(body)?),
                            _ => {}
                        }
                        inner = c_end;
                    }
                }
                TRACKS => webm.tracks = tracks(&data[..end], e.data_start, end)?,
                CUES => webm.cues = cues(&data[..end], e.data_start, end)?,
                CLUSTER => {
                    if webm.first_cluster.is_none() {
                        webm.first_cluster = Some(start);
                    }
                }
                _ => {}
            }
            at = end;
        }
        webm.duration_ns = duration.map(|d| d * webm.timestamp_scale as f64);
        // Cue times are in timestamp units, and the scale may come after the cues.
        for cue in &mut webm.cues {
            cue.time_ns = cue.time_ns.saturating_mul(webm.timestamp_scale);
        }
        Ok(webm)
    }

    /// The first track of `kind`.
    pub fn track(&self, kind: TrackKind) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == kind)
    }

    /// The blocks of every cluster, in file order.
    pub fn blocks(&self) -> Blocks<'a> {
        Blocks {
            data: self.data,
            segment_end: self.segment.end,
            at: self.first_cluster.unwrap_or(self.segment.end),
            cluster_end: 0,
            cluster_time: 0,
            timestamp_scale: self.timestamp_scale,
        }
    }

    /// The blocks from the cluster at `cluster_offset` (a cue's, from the segment's data)
    /// on.
    pub fn blocks_from(&self, cluster_offset: u64) -> Blocks<'a> {
        let at = self.segment.start.saturating_add(cluster_offset as usize);
        Blocks {
            at: at.min(self.segment.end),
            ..self.blocks()
        }
    }
}

/// An element's header: its id, where its data begins, its size.
#[derive(Debug, Clone, Copy)]
struct Element {
    id: u32,
    data_start: usize,
    size: u64,
    unknown_size: bool,
}

impl Element {
    /// Where the element ends, within `limit`.
    fn end(&self, limit: usize) -> Result<usize, WebmError> {
        if self.unknown_size {
            return Ok(limit);
        }
        let end = (self.data_start as u64)
            .checked_add(self.size)
            .ok_or(WebmError::Corrupt("element size"))?;
        if end > limit as u64 {
            return Err(WebmError::Corrupt("element past its parent"));
        }
        Ok(end as usize)
    }
}

/// Reads the element header at `*at`, leaving `*at` at its data.
fn element(data: &[u8], at: &mut usize) -> Result<Element, WebmError> {
    let (id, id_len) = vint(data, *at, true)?;
    let (size, size_len) = vint(data, *at + id_len, false)?;
    let unknown_size = size == (1u64 << (7 * size_len)) - 1;
    *at += id_len + size_len;
    Ok(Element {
        id: id as u32,
        data_start: *at,
        size,
        unknown_size,
    })
}

/// A variable-length integer at `at`: its value (with its length marker for ids) and its
/// length in bytes.
fn vint(data: &[u8], at: usize, keep_marker: bool) -> Result<(u64, usize), WebmError> {
    let first = *data
        .get(at)
        .ok_or(WebmError::Corrupt("data ends in a header"))?;
    if first == 0 {
        return Err(WebmError::Corrupt(
            "variable-length integer longer than 8 bytes",
        ));
    }
    let len = first.leading_zeros() as usize + 1;
    if keep_marker && len > 4 {
        return Err(WebmError::Corrupt("element id longer than 4 bytes"));
    }
    let bytes = data
        .get(at..at + len)
        .ok_or(WebmError::Corrupt("data ends in a header"))?;
    let mut value = if keep_marker {
        u64::from(first)
    } else {
        u64::from(first) & (0xFF >> len)
    };
    for &b in &bytes[1..] {
        value = value << 8 | u64::from(b);
    }
    Ok((value, len))
}

/// Where an element of unknown size that began its data at `from` ends: at the next of
/// `siblings` at its level, or at `limit`.
fn next_sibling(
    data: &[u8],
    from: usize,
    limit: usize,
    siblings: &[u32],
) -> Result<usize, WebmError> {
    let mut at = from;
    while at < limit {
        let start = at;
        let e = element(&data[..limit], &mut at)?;
        if siblings.contains(&e.id) {
            return Ok(start);
        }
        at = e.end(limit)?;
    }
    Ok(limit)
}

fn uint(body: &[u8]) -> Result<u64, WebmError> {
    if body.len() > 8 {
        return Err(WebmError::Corrupt("unsigned integer longer than 8 bytes"));
    }
    Ok(body.iter().fold(0u64, |v, &b| v << 8 | u64::from(b)))
}

fn float(body: &[u8]) -> Result<f64, WebmError> {
    match body.len() {
        0 => Ok(0.0),
        4 => Ok(f64::from(f32::from_be_bytes([
            body[0], body[1], body[2], body[3],
        ]))),
        8 => Ok(f64::from_be_bytes([
            body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
        ])),
        _ => Err(WebmError::Corrupt("float neither 4 nor 8 bytes")),
    }
}

fn tracks(data: &[u8], start: usize, end: usize) -> Result<Vec<Track>, WebmError> {
    let mut out = Vec::new();
    let mut at = start;
    while at < end {
        let e = element(data, &mut at)?;
        let e_end = e.end(end)?;
        if e.id == TRACK_ENTRY {
            out.push(track(data, e.data_start, e_end)?);
        }
        at = e_end;
    }
    Ok(out)
}

fn track(data: &[u8], start: usize, end: usize) -> Result<Track, WebmError> {
    let mut t = Track::new();
    let mut at = start;
    while at < end {
        let e = element(data, &mut at)?;
        let e_end = e.end(end)?;
        let body = &data[e.data_start..e_end];
        match e.id {
            TRACK_NUMBER => t.number = uint(body)?,
            TRACK_TYPE => {
                t.kind = match uint(body)? {
                    1 => TrackKind::Video,
                    2 => TrackKind::Audio,
                    _ => TrackKind::Other,
                }
            }
            CODEC_ID => {
                let id = core::str::from_utf8(body)
                    .map_err(|_| WebmError::Corrupt("codec id not UTF-8"))?
                    .trim_end_matches('\0');
                t.codec = match id {
                    "V_AV1" => Codec::Av1,
                    "A_OPUS" => Codec::Opus,
                    other => Codec::Other(other.to_owned()),
                };
            }
            CODEC_PRIVATE => t.private = body.to_vec(),
            CODEC_DELAY => t.codec_delay_ns = uint(body)?,
            SEEK_PRE_ROLL => t.seek_pre_roll_ns = uint(body)?,
            DEFAULT_DURATION => t.default_duration_ns = uint(body)?,
            VIDEO => {
                let mut inner = e.data_start;
                while inner < e_end {
                    let c = element(data, &mut inner)?;
                    let c_end = c.end(e_end)?;
                    let v = &data[c.data_start..c_end];
                    match c.id {
                        PIXEL_WIDTH => t.width = uint(v)? as u32,
                        PIXEL_HEIGHT => t.height = uint(v)? as u32,
                        _ => {}
                    }
                    inner = c_end;
                }
            }
            AUDIO => {
                let mut inner = e.data_start;
                while inner < e_end {
                    let c = element(data, &mut inner)?;
                    let c_end = c.end(e_end)?;
                    let v = &data[c.data_start..c_end];
                    match c.id {
                        SAMPLING_FREQUENCY => t.sample_rate = float(v)?,
                        CHANNELS => t.channels = uint(v)? as u32,
                        _ => {}
                    }
                    inner = c_end;
                }
            }
            _ => {}
        }
        at = e_end;
    }
    Ok(t)
}

fn cues(data: &[u8], start: usize, end: usize) -> Result<Vec<CuePoint>, WebmError> {
    let mut out = Vec::new();
    let mut at = start;
    while at < end {
        let e = element(data, &mut at)?;
        let e_end = e.end(end)?;
        if e.id == CUE_POINT {
            let (mut time, mut positions) = (0, Vec::new());
            let mut inner = e.data_start;
            while inner < e_end {
                let c = element(data, &mut inner)?;
                let c_end = c.end(e_end)?;
                match c.id {
                    CUE_TIME => time = uint(&data[c.data_start..c_end])?,
                    CUE_TRACK_POSITIONS => {
                        let (mut track, mut cluster) = (0, 0);
                        let mut p = c.data_start;
                        while p < c_end {
                            let g = element(data, &mut p)?;
                            let g_end = g.end(c_end)?;
                            let v = &data[g.data_start..g_end];
                            match g.id {
                                CUE_TRACK => track = uint(v)?,
                                CUE_CLUSTER_POSITION => cluster = uint(v)?,
                                _ => {}
                            }
                            p = g_end;
                        }
                        positions.push((track, cluster));
                    }
                    _ => {}
                }
                inner = c_end;
            }
            for (track, cluster_offset) in positions {
                out.push(CuePoint {
                    time_ns: time,
                    track,
                    cluster_offset,
                });
            }
        }
        at = e_end;
    }
    Ok(out)
}

/// The blocks of a file's clusters, in file order ([`Webm::blocks`]).
#[derive(Debug, Clone)]
pub struct Blocks<'a> {
    data: &'a [u8],
    segment_end: usize,
    at: usize,
    /// The current cluster's end (0: between clusters).
    cluster_end: usize,
    cluster_time: i64,
    timestamp_scale: u64,
}

impl<'a> Blocks<'a> {
    fn next_block(&mut self) -> Result<Option<Block<'a>>, WebmError> {
        let data = self.data;
        loop {
            if self.at >= self.cluster_end {
                // The next cluster.
                if self.at >= self.segment_end {
                    return Ok(None);
                }
                let e = element(&data[..self.segment_end], &mut self.at)?;
                if e.id != CLUSTER {
                    self.at = if e.unknown_size {
                        next_sibling(data, e.data_start, self.segment_end, &SEGMENT_CHILDREN)?
                    } else {
                        e.end(self.segment_end)?
                    };
                    continue;
                }
                self.cluster_end = if e.unknown_size {
                    next_sibling(data, e.data_start, self.segment_end, &SEGMENT_CHILDREN)?
                } else {
                    e.end(self.segment_end)?
                };
                self.cluster_time = 0;
                continue;
            }
            let start = self.at;
            let e = element(&data[..self.cluster_end], &mut self.at)?;
            let end = e.end(self.cluster_end)?;
            match e.id {
                CLUSTER_TIMESTAMP => {
                    self.cluster_time = uint(&data[e.data_start..end])? as i64;
                    self.at = end;
                }
                SIMPLE_BLOCK => {
                    self.at = end;
                    return block(
                        data,
                        e.data_start,
                        end,
                        start,
                        self.cluster_time,
                        self.timestamp_scale,
                        None,
                    )
                    .map(Some);
                }
                BLOCK_GROUP => {
                    // Its reference block (if any) may come after the block.
                    let mut referenced = false;
                    let mut found = None;
                    let mut inner = e.data_start;
                    while inner < end {
                        let c = element(&data[..end], &mut inner)?;
                        let c_end = c.end(end)?;
                        match c.id {
                            BLOCK => found = Some((c.data_start, c_end)),
                            REFERENCE_BLOCK => referenced = true,
                            _ => {}
                        }
                        inner = c_end;
                    }
                    self.at = end;
                    if let Some((s, b_end)) = found {
                        return block(
                            data,
                            s,
                            b_end,
                            start,
                            self.cluster_time,
                            self.timestamp_scale,
                            Some(!referenced),
                        )
                        .map(Some);
                    }
                }
                _ => self.at = end,
            }
        }
    }
}

impl<'a> Iterator for Blocks<'a> {
    type Item = Result<Block<'a>, WebmError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_block() {
            Ok(Some(b)) => Some(Ok(b)),
            Ok(None) => None,
            Err(e) => {
                // Nothing more after damage.
                self.at = self.segment_end;
                self.cluster_end = 0;
                Some(Err(e))
            }
        }
    }
}

/// A block's body at `start..end`: its track, time, flags and frames. `keyframe` is
/// given for a BlockGroup's block (none: a SimpleBlock's flag says).
fn block<'a>(
    data: &'a [u8],
    start: usize,
    end: usize,
    offset: usize,
    cluster_time: i64,
    scale: u64,
    keyframe: Option<bool>,
) -> Result<Block<'a>, WebmError> {
    let (track, len) = vint(&data[..end], start, false)?;
    let header = start + len;
    let bytes = data
        .get(header..header + 3)
        .filter(|_| header + 3 <= end)
        .ok_or(WebmError::Corrupt("block header"))?;
    let relative = i64::from(i16::from_be_bytes([bytes[0], bytes[1]]));
    let flags = bytes[2];
    let time_ns = (cluster_time + relative).saturating_mul(scale as i64);
    let body = &data[header + 3..end];
    let frames = match (flags >> 1) & 3 {
        0 => Frames {
            data: body,
            ends: {
                let mut ends = [0; MAX_LACED];
                ends[0] = body.len() as u32;
                ends
            },
            count: 1,
        },
        lacing => laced(body, lacing)?,
    };
    Ok(Block {
        track,
        time_ns,
        keyframe: keyframe.unwrap_or(flags & 0x80 != 0),
        frames,
        offset,
    })
}

/// A laced block's frames: Xiph (1), fixed-size (2) or EBML (3) lacing.
fn laced(body: &[u8], lacing: u8) -> Result<Frames<'_>, WebmError> {
    let count = usize::from(*body.first().ok_or(WebmError::Corrupt("laced block"))?) + 1;
    if count > MAX_LACED {
        return Err(WebmError::Unsupported(
            "more laced frames than the engine reads",
        ));
    }
    let mut sizes = [0usize; MAX_LACED];
    let mut at = 1;
    match lacing {
        1 => {
            for size in sizes.iter_mut().take(count - 1) {
                loop {
                    let b = *body.get(at).ok_or(WebmError::Corrupt("Xiph lacing"))?;
                    at += 1;
                    *size += usize::from(b);
                    if b != 255 {
                        break;
                    }
                }
            }
        }
        2 => {
            let each = (body.len() - 1) / count;
            if each * count != body.len() - 1 {
                return Err(WebmError::Corrupt("fixed-size lacing"));
            }
            for size in sizes.iter_mut().take(count - 1) {
                *size = each;
            }
        }
        _ if count > 1 => {
            let (first, len) = vint(body, at, false)?;
            at += len;
            sizes[0] = first as usize;
            for i in 1..count - 1 {
                let (raw, len) = vint(body, at, false)?;
                at += len;
                // A signed difference from the previous size.
                let bias = (1i64 << (7 * len - 1)) - 1;
                let size = sizes[i - 1] as i64 + raw as i64 - bias;
                if size < 0 {
                    return Err(WebmError::Corrupt("EBML lacing"));
                }
                sizes[i] = size as usize;
            }
        }
        // EBML lacing of a single frame codes no sizes.
        _ => {}
    }
    let frames_start = at;
    let known: usize = sizes[..count - 1].iter().sum();
    let total = body
        .len()
        .checked_sub(frames_start)
        .ok_or(WebmError::Corrupt("lacing"))?;
    sizes[count - 1] = total
        .checked_sub(known)
        .ok_or(WebmError::Corrupt("lacing sizes"))?;
    let data = &body[frames_start..];
    let mut ends = [0u32; MAX_LACED];
    let mut end = 0;
    for (e, size) in ends.iter_mut().zip(&sizes).take(count) {
        end += size;
        *e = end as u32;
    }
    Ok(Frames { data, ends, count })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_length_integers() {
        assert_eq!(vint(&[0x81], 0, false), Ok((1, 1)));
        assert_eq!(vint(&[0x40, 0x02], 0, false), Ok((2, 2)));
        assert_eq!(
            vint(&[0x1A, 0x45, 0xDF, 0xA3], 0, true),
            Ok((0x1A45_DFA3, 4))
        );
        assert!(vint(&[0x00], 0, false).is_err());
        assert!(vint(&[0x40], 0, false).is_err());
    }

    #[test]
    fn xiph_and_ebml_lacing() {
        // Three frames of 2, 300 and 1 bytes, Xiph-laced.
        let mut body = vec![2u8, 2, 255, 45];
        body.extend(core::iter::repeat_n(7u8, 2 + 300 + 1));
        let f = laced(&body, 1).unwrap();
        let sizes: Vec<usize> = f.iter().map(<[u8]>::len).collect();
        assert_eq!(sizes, [2, 300, 1]);
        // EBML lacing: 2 frames of 3 and 5 bytes (the first size as a VINT, the last implied).
        let mut body = vec![1u8, 0x83];
        body.extend([1u8; 8]);
        let f = laced(&body, 3).unwrap();
        let sizes: Vec<usize> = f.iter().map(<[u8]>::len).collect();
        assert_eq!(sizes, [3, 5]);
    }
}
