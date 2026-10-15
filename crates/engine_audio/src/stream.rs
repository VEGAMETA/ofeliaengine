//! Compressed audio: the [`Decoder`] interface that codecs implement (Vorbis, Opus and FLAC
//! live in `engine_audio_decode`), decoding a whole file into a [`Clip`], and streaming
//! long sounds (ADR 0051).
//!
//! A stream is a [`StreamRing`] of decoded samples between the stream thread, which decodes
//! ahead into it, and the audio thread, which reads it like a clip. The ring is allocated
//! on the game thread when the stream starts and freed there when its voice ends; neither
//! thread ever waits for the other.

use crate::clip::{Clip, ClipError};
use core::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

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

/// Decoded samples between the stream thread (the only writer) and the audio thread (the
/// only reader): interleaved frames in a power-of-two ring, with counters of the frames
/// written and released since the start. Samples are atomics accessed relaxed (plain moves
/// on x86); the counters publish them (release on write, acquire on read).
pub(crate) struct StreamRing {
    samples: Box<[AtomicU32]>,
    /// Capacity in frames − 1 (a power of two minus one).
    mask: u64,
    pub(crate) channels: usize,
    pub(crate) sample_rate: u32,
    /// Frames written since the start.
    written: AtomicU64,
    /// Frames the reader is done with.
    released: AtomicU64,
    /// No more frames will come (the decoder ended or failed).
    finished: AtomicBool,
    /// The voice ended: the stream thread drops the decoder.
    closed: AtomicBool,
}

impl StreamRing {
    pub(crate) fn new(frames: usize, channels: usize, sample_rate: u32) -> Self {
        let capacity = frames.max(1024).next_power_of_two();
        Self {
            samples: (0..capacity * channels)
                .map(|_| AtomicU32::new(0))
                .collect(),
            mask: capacity as u64 - 1,
            channels,
            sample_rate,
            written: AtomicU64::new(0),
            released: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn capacity(&self) -> u64 {
        self.mask + 1
    }

    /// Frames the reader is done with since the start: the stream's position in the mix.
    pub(crate) fn released(&self) -> u64 {
        self.released.load(Ordering::Acquire)
    }

    /// Writer: frames that fit now.
    pub(crate) fn space(&self) -> u64 {
        let written = self.written.load(Ordering::Relaxed);
        self.capacity() - (written - self.released.load(Ordering::Acquire))
    }

    /// Writer: appends interleaved frames (at most [`Self::space`]).
    pub(crate) fn push(&self, interleaved: &[f32]) {
        let written = self.written.load(Ordering::Relaxed);
        let frames = (interleaved.len() / self.channels) as u64;
        debug_assert!(frames <= self.space());
        for (f, frame) in interleaved.chunks_exact(self.channels).enumerate() {
            let at = ((written + f as u64) & self.mask) as usize * self.channels;
            for (c, &s) in frame.iter().enumerate() {
                self.samples[at + c].store(s.to_bits(), Ordering::Relaxed);
            }
        }
        self.written.store(written + frames, Ordering::Release);
    }

    /// Writer: no more frames.
    pub(crate) fn finish(&self) {
        self.finished.store(true, Ordering::Release);
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Reader: `(frames written, whether they are all there will be)`. `finished` is read
    /// first: once it is set, `written` is final.
    #[inline]
    fn available(&self) -> (u64, bool) {
        let finished = self.finished.load(Ordering::Acquire);
        (self.written.load(Ordering::Acquire), finished)
    }

    /// Reader: sample `channel` of frame `frame` (written, not yet released).
    #[inline]
    fn sample(&self, frame: u64, channel: usize) -> f32 {
        let at = (frame & self.mask) as usize * self.channels + channel;
        f32::from_bits(self.samples[at].load(Ordering::Relaxed))
    }

    /// Reader: frames before `frame` may be overwritten.
    #[inline]
    fn release(&self, frame: u64) {
        self.released.store(frame, Ordering::Release);
    }
}

/// What reading a stream came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamRead {
    /// All requested frames.
    Full,
    /// Fewer: the decoder has not caught up (an underrun); the voice waits.
    Starved(usize),
    /// Fewer: the stream ended.
    Ended(usize),
}

/// Reads frames from `pos` (32.32, in the stream's frames) advancing by `step` per output
/// frame, with linear interpolation, into `left` (and `right` for stereo), as the clip
/// resampler does. The last frame fades towards silence when the stream has ended.
pub(crate) fn resample_stream(
    ring: &StreamRing,
    pos: &mut u64,
    step: u64,
    left: &mut [f32],
    right: &mut [f32],
) -> StreamRead {
    let (written, finished) = ring.available();
    let stereo = ring.channels == 2;
    let mut p = *pos;
    let mut result = StreamRead::Full;
    let mut i = 0;
    if step == 1 << 32 && p as u32 == 0 {
        // Same rate, on a frame: a straight copy of the frames with a successor (the rule
        // the interpolating path below keeps too).
        let run = (left.len() as u64).min(written.saturating_sub((p >> 32) + 1)) as usize;
        let first = p >> 32;
        for (k, l) in left[..run].iter_mut().enumerate() {
            *l = ring.sample(first + k as u64, 0);
        }
        if stereo {
            for (k, r) in right[..run].iter_mut().enumerate() {
                *r = ring.sample(first + k as u64, 1);
            }
        }
        i = run;
        p += (run as u64) << 32;
    }
    for i in i..left.len() {
        let index = p >> 32;
        let t = (p as u32) as f32 * (1.0 / 4_294_967_296.0);
        if index + 1 >= written {
            if !finished {
                result = StreamRead::Starved(i);
                break;
            }
            if index >= written {
                result = StreamRead::Ended(i);
                break;
            }
            // The last frame leads into silence.
            left[i] = ring.sample(index, 0) * (1.0 - t);
            if stereo {
                right[i] = ring.sample(index, 1) * (1.0 - t);
            }
        } else {
            let (a0, a1) = (ring.sample(index, 0), ring.sample(index + 1, 0));
            left[i] = a0 + (a1 - a0) * t;
            if stereo {
                let (b0, b1) = (ring.sample(index, 1), ring.sample(index + 1, 1));
                right[i] = b0 + (b1 - b0) * t;
            }
        }
        p += step;
    }
    *pos = p;
    ring.release((p >> 32).min(written));
    result
}

/// Moves a stream voice's position `n` output frames on without reading (virtual voices),
/// as far as the decoder has got; `true` if the stream ended.
pub(crate) fn advance_stream(ring: &StreamRing, pos: &mut u64, step: u64, n: u64) -> bool {
    let (written, finished) = ring.available();
    let target = pos.saturating_add(step.saturating_mul(n));
    if target >> 32 <= written {
        *pos = target;
    } else {
        *pos = written << 32;
    }
    ring.release(*pos >> 32);
    finished && *pos >> 32 >= written
}

/// Frames the stream thread decodes at a time.
const CHUNK: usize = 1024;

/// A stream the stream thread keeps filled.
pub(crate) struct StreamJob {
    pub(crate) ring: Arc<StreamRing>,
    pub(crate) decoder: Box<dyn Decoder>,
    pub(crate) looping: bool,
    /// Decode errors so far (reported through [`crate::Audio::stream_errors`]).
    pub(crate) errors: Arc<AtomicU64>,
}

impl StreamJob {
    /// Decodes into the ring while it has room for a chunk; `false` once the job is done
    /// (the stream ended or failed, or its voice ended).
    pub(crate) fn fill(&mut self, scratch: &mut Vec<f32>) -> bool {
        if self.ring.is_closed() {
            return false;
        }
        let channels = self.ring.channels;
        let mut restarted = false;
        while self.ring.space() >= CHUNK as u64 {
            scratch.resize(CHUNK * channels, 0.0);
            let frames = match self.decoder.read(scratch) {
                Ok(frames) => frames,
                Err(_) => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    0
                }
            };
            if frames == 0 {
                // A looping stream starts over; one that yields nothing even from its
                // start (empty, or failing) ends instead of spinning.
                if self.looping && !restarted && self.decoder.seek(0).is_ok() {
                    restarted = true;
                    continue;
                }
                self.ring.finish();
                return false;
            }
            restarted = false;
            self.ring.push(&scratch[..frames * channels]);
        }
        true
    }
}

struct StreamerShared {
    new_jobs: Mutex<Vec<StreamJob>>,
    wake: Condvar,
    quit: AtomicBool,
}

/// The stream thread: decodes ahead into every playing stream's ring. Started with the
/// first stream; stopped and joined when the [`crate::Audio`] is dropped.
pub(crate) struct Streamer {
    shared: Arc<StreamerShared>,
    thread: Option<JoinHandle<()>>,
}

/// How often the stream thread looks at its streams while any play. With the default
/// 500 ms rings this leaves over 400 ms of margin for disk hiccups.
const POLL: std::time::Duration = std::time::Duration::from_millis(10);

impl Streamer {
    pub(crate) fn start() -> Self {
        let shared = Arc::new(StreamerShared {
            new_jobs: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            quit: AtomicBool::new(false),
        });
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("audio-stream".into())
            .spawn(move || run(&thread_shared))
            .ok();
        Self { shared, thread }
    }

    pub(crate) fn add(&self, job: StreamJob) {
        if let Ok(mut jobs) = self.shared.new_jobs.lock() {
            jobs.push(job);
        }
        self.shared.wake.notify_one();
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Release);
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(shared: &StreamerShared) {
    let mut jobs: Vec<StreamJob> = Vec::new();
    let mut scratch = Vec::new();
    loop {
        {
            let Ok(mut new_jobs) = shared.new_jobs.lock() else {
                return;
            };
            // Sleep until something may need decoding: a new stream, a poll while
            // streams play, or quitting.
            while new_jobs.is_empty() && !shared.quit.load(Ordering::Acquire) {
                let wait = if jobs.is_empty() {
                    shared.wake.wait(new_jobs).map_err(drop)
                } else {
                    shared
                        .wake
                        .wait_timeout(new_jobs, POLL)
                        .map(|(g, _)| g)
                        .map_err(drop)
                };
                let Ok(guard) = wait else {
                    return;
                };
                new_jobs = guard;
                if !jobs.is_empty() {
                    break;
                }
            }
            if shared.quit.load(Ordering::Acquire) {
                return;
            }
            jobs.append(&mut new_jobs);
        }
        engine_profiler::zone!("audio::stream");
        jobs.retain_mut(|job| job.fill(&mut scratch));
    }
}
