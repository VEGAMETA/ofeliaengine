//! Playing a video (ADR 0100): a WebM file's AV1 pictures decoded ahead of the clock, the
//! one due converted into the video's texture, its Opus sound played as an audio stream.
//!
//! - **Decoding:** on the GPU's video engine when the device has the decode queue and
//!   takes the stream; otherwise, or when the GPU refuses a frame on the way (a size
//!   outside its range), by rav1d on a thread of its own, from the key frame before the
//!   clock.
//! - **The clock:** while the sound plays, its position in the mix less the output's
//!   latency; otherwise the time steps the caller passes (the game's for a video in the
//!   world, real time for a loading screen). The picture shown is the newest whose time
//!   the clock has reached: late pictures are skipped, early ones held. After opening or
//!   a seek the clock waits for the first picture.
//! - **Each frame:** [`VideoPlayer::update`] before the frame graph is built (the clock,
//!   decoding, the picture due), then [`VideoPlayer::add_pass`] while it is (the
//!   conversion, when a new picture is due).
//!
//! The controls ([`VideoPlayer::play`], [`VideoPlayer::pause`], [`VideoPlayer::seek`],
//! looping and speed) take effect at the next update. A speed other than 1 plays the
//! sound faster or slower, its pitch with it.

// Without the software decoder, the GPU's is the only kind.
#![cfg_attr(not(feature = "software"), allow(irrefutable_let_patterns))]

use core::fmt;
use std::collections::VecDeque;
use std::sync::Arc;

use engine_audio::{Audio, AudioError, BusId, Decoder, PlayParams, VoiceHandle};
use engine_render::graph::FrameGraph;
use engine_rhi::{Allocator, Bindless, Device, RhiError};

use crate::audio::VideoAudio;
use crate::av1::sequence::SequenceHeader;
use crate::av1::{Av1Error, Parser, TemporalUnit};
use crate::convert::{Conversion, Converter, DecoderPlanes, VideoTexture, wait_for_decode};
use crate::gpu::{GpuDecoder, GpuDecoderDesc, GpuError, GpuPicture};
use crate::index::{FileIndex, Indexed, keyframe_before};
use crate::webm::WebmError;

/// Opus's rate.
const RATE: f64 = 48_000.0;
/// Temporal units the GPU path decodes in one update at most (catching up after a seek
/// takes a few frames rather than one long one).
const UNITS_PER_UPDATE: usize = 8;
/// The fade of a sound stopped for a pause or a seek, in seconds.
const FADE: f32 = 0.005;

/// How a video plays.
#[derive(Debug, Clone, Copy)]
pub struct VideoDesc {
    /// Plays once opened; otherwise it shows its first picture until
    /// [`VideoPlayer::play`].
    pub autoplay: bool,
    /// Starts over at the end.
    pub looping: bool,
    /// Decodes on the GPU's video engine when it can.
    pub gpu: bool,
    /// Plays the sound, on `bus` at `gain`.
    pub sound: bool,
    /// The bus of the sound.
    pub bus: BusId,
    /// The sound's linear gain.
    pub gain: f32,
    /// Pictures decoded ahead of the clock.
    pub ahead: usize,
    /// rav1d's threads; 0: half the logical cores, at most 8.
    pub threads: u32,
}

impl Default for VideoDesc {
    fn default() -> Self {
        Self {
            autoplay: true,
            looping: false,
            gpu: true,
            sound: true,
            bus: BusId::MASTER,
            gain: 1.0,
            ahead: 3,
            threads: 0,
        }
    }
}

/// What happened to a video during an update.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VideoEvent {
    /// A picture is in the texture from this frame on (its time in seconds).
    Shown(f64),
    /// The last picture is shown and the clock passed the end (the video is not looping).
    Ended,
    /// The end was reached and the video starts over.
    Looped,
}

/// Why a video could not open or play on.
#[derive(Debug)]
pub enum VideoError {
    /// The file.
    Webm(WebmError),
    /// The AV1 stream.
    Av1(Av1Error),
    /// The GPU's decoding.
    Gpu(GpuError),
    /// GPU memory or a Vulkan call.
    Rhi(RhiError),
    /// The sound could not start.
    Audio(AudioError),
    /// The software decoder.
    Software(String),
    /// Neither the GPU nor the software decoder (a build without it) takes the stream.
    NoDecoder(&'static str),
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Webm(e) => write!(f, "{e}"),
            Self::Av1(e) => write!(f, "{e}"),
            Self::Gpu(e) => write!(f, "{e}"),
            Self::Rhi(e) => write!(f, "{e}"),
            Self::Audio(e) => write!(f, "video sound: {e}"),
            Self::Software(e) => write!(f, "software video decoding: {e}"),
            Self::NoDecoder(why) => write!(f, "no decoder for the video: {why}"),
        }
    }
}

impl std::error::Error for VideoError {}

impl From<WebmError> for VideoError {
    fn from(e: WebmError) -> Self {
        Self::Webm(e)
    }
}

impl From<Av1Error> for VideoError {
    fn from(e: Av1Error) -> Self {
        Self::Av1(e)
    }
}

impl From<GpuError> for VideoError {
    fn from(e: GpuError) -> Self {
        Self::Gpu(e)
    }
}

impl From<RhiError> for VideoError {
    fn from(e: RhiError) -> Self {
        Self::Rhi(e)
    }
}

impl From<AudioError> for VideoError {
    fn from(e: AudioError) -> Self {
        Self::Audio(e)
    }
}

impl From<engine_audio::DecodeError> for VideoError {
    fn from(e: engine_audio::DecodeError) -> Self {
        Self::Audio(AudioError::Decode(e))
    }
}

/// What [`VideoPlayer::update`] needs of the frame.
pub struct Tick<'a> {
    /// The frame's number (`engine_rhi::Frame::number`).
    pub frame: u64,
    /// Frames in flight: frame `frame - in_flight` has finished.
    pub in_flight: u64,
    /// Seconds since the last update, for a video without sound playing (the game's time
    /// step, or real time).
    pub dt: f64,
    /// The audio engine, for the sound and its clock (`None`: no sound).
    pub audio: Option<&'a mut Audio>,
    /// The output's latency in seconds: how long a frame mixed now takes to be heard
    /// (`engine_audio::output::OutputInfo`'s queue and stream latency).
    pub latency: f64,
}

impl fmt::Debug for Tick<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tick")
            .field("frame", &self.frame)
            .field("dt", &self.dt)
            .field("latency", &self.latency)
            .finish_non_exhaustive()
    }
}

/// A decoded picture not shown yet (moved, never boxed: no allocation per picture).
#[allow(clippy::large_enum_variant)]
enum Picture {
    Gpu(GpuPicture),
    #[cfg(feature = "software")]
    Software(crate::software::Picture),
}

/// A picture and its time in seconds.
struct Ahead {
    time: f64,
    picture: Picture,
}

/// Decoding on the GPU's video engine.
struct GpuDecoding {
    decoder: GpuDecoder,
    planes: DecoderPlanes,
    parser: Parser,
    unit: TemporalUnit,
    /// The unit to decode next.
    next: usize,
    out: Vec<GpuPicture>,
}

enum Decoding {
    Gpu(Box<GpuDecoding>),
    #[cfg(feature = "software")]
    Software(software::Decoding),
}

/// The sound.
struct Sound {
    /// A decoder of the track, for each stream started ([`VideoAudio::fresh`]).
    track: VideoAudio,
    /// The first packet's time, seconds.
    start: f64,
    voice: Option<VoiceHandle>,
    /// The clock's time at the voice's first frame.
    base: f64,
    /// The sound ended before the picture (or could not start): the time steps run the
    /// clock until the next seek.
    done: bool,
    bus: BusId,
    gain: f32,
}

/// A playing video (see the module).
pub struct VideoPlayer {
    data: Arc<[u8]>,
    units: Arc<[Indexed]>,
    duration: f64,
    sequence: SequenceHeader,
    conversion: Conversion,
    texture: VideoTexture,
    decoding: Decoding,
    /// GPU decoders replaced by the software one, destroyed once the frames that read
    /// their pictures (up to the number beside) have finished.
    retired: Vec<(Box<GpuDecoding>, u64)>,
    /// Pictures decoded and not shown, in time order.
    ahead: VecDeque<Ahead>,
    /// The picture [`Self::add_pass`] converts this frame.
    due: Option<Ahead>,
    /// GPU pictures converted, given back once their frames finish.
    in_use: VecDeque<(GpuPicture, u64)>,
    sound: Option<Sound>,
    clock: f64,
    playing: bool,
    /// The clock holds until the next picture is shown (opened, sought).
    waiting: bool,
    speed: f64,
    speed_changed: bool,
    looping: bool,
    /// Decoding reached the end of the stream.
    drained: bool,
    ended: bool,
    seek: Option<f64>,
    /// The sound starts again at the clock (a seek).
    restart_sound: bool,
    events: Vec<VideoEvent>,
    ahead_limit: usize,
    in_flight: usize,
    threads: u32,
}

impl fmt::Debug for VideoPlayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoPlayer")
            .field("clock", &self.clock)
            .field("duration", &self.duration)
            .field("playing", &self.playing)
            .field("gpu", &self.decodes_on_gpu())
            .finish_non_exhaustive()
    }
}

impl VideoPlayer {
    /// Opens the WebM file `data` for a renderer with `in_flight` frames in flight: its
    /// texture, its decoder (the GPU's when `desc.gpu` and the device takes the stream),
    /// its sound.
    ///
    /// # Errors
    ///
    /// A file or stream that cannot be read, no decoder for it, or out of memory.
    pub fn open(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        data: Arc<[u8]>,
        desc: &VideoDesc,
        in_flight: usize,
    ) -> Result<Self, VideoError> {
        let index = FileIndex::read(&data)?;
        let parser = Parser::with_codec_private(&index.video.private)?;
        // The first unit: the sequence header where the codec private data lacks it,
        // and the size the pictures are shown at.
        let mut probe = parser.clone();
        let mut unit = TemporalUnit::default();
        probe.parse(index.units[0].bytes(&data), &mut unit)?;
        let sequence = *probe
            .sequence()
            .ok_or(Av1Error::Unsupported("a stream without a sequence header"))?;
        let size = unit
            .frames
            .iter()
            .find(|f| !f.header.show_existing_frame)
            .map(|f| [f.header.render_width, f.header.render_height])
            .filter(|s| s[0] > 0 && s[1] > 0)
            .unwrap_or([sequence.max_width(), sequence.max_height()]);
        let conversion = Conversion::new(&sequence.color, size[0], size[1]);
        let threads = if desc.threads > 0 {
            desc.threads
        } else {
            let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
            (cores / 2).clamp(1, 8) as u32
        };
        let units: Arc<[Indexed]> = index.units.into();
        let sound = match (&index.audio, desc.sound) {
            (Some(track), true) if !index.packets.is_empty() => {
                let start = index.packets[0].time_ns as f64 * 1e-9;
                let packets: Arc<[Indexed]> = index.packets.into();
                Some(Sound {
                    track: VideoAudio::new(data.clone(), packets, &track.private)?,
                    start,
                    voice: None,
                    base: 0.0,
                    done: false,
                    bus: desc.bus,
                    gain: desc.gain,
                })
            }
            _ => None,
        };
        let gpu = if desc.gpu && device.video_decode.is_some() {
            start_gpu(
                device,
                allocator,
                bindless,
                &sequence,
                probe.sequence_serial(),
                parser,
            )?
        } else {
            None
        };
        let decoding = match gpu {
            Some(gpu) => Decoding::Gpu(gpu),
            None => start_software(
                device, allocator, bindless, &sequence, &data, &units, 0, in_flight, threads,
                desc.ahead,
            )?,
        };
        let texture = match VideoTexture::new(device, allocator, bindless, size[0], size[1]) {
            Ok(texture) => texture,
            Err(e) => {
                // SAFETY: nothing was recorded with the decoder yet.
                unsafe { destroy_decoding(decoding, device, allocator, bindless, 0) };
                return Err(e.into());
            }
        };
        let ahead_limit = desc.ahead.max(1);
        Ok(Self {
            data,
            units,
            duration: index.duration_ns as f64 * 1e-9,
            sequence,
            conversion,
            texture,
            decoding,
            retired: Vec::new(),
            ahead: VecDeque::with_capacity(ahead_limit + 1),
            due: None,
            in_use: VecDeque::with_capacity(in_flight + 1),
            sound,
            clock: 0.0,
            playing: desc.autoplay,
            waiting: true,
            speed: 1.0,
            speed_changed: false,
            looping: desc.looping,
            drained: false,
            ended: false,
            seek: None,
            restart_sound: false,
            events: Vec::with_capacity(4),
            ahead_limit,
            in_flight,
            threads,
        })
    }

    /// Plays on (from the start once ended).
    pub fn play(&mut self) {
        if self.ended {
            self.seek = Some(0.0);
        }
        self.playing = true;
    }

    /// Holds the picture and the sound where they are.
    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Goes to `time` seconds (clamped to the video): decoding resumes at the key frame
    /// before it, and the clock waits for its picture.
    pub fn seek(&mut self, time: f64) {
        self.seek = Some(time);
    }

    /// Whether it starts over at the end.
    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    /// Plays `speed` times as fast (1/16 to 16), the sound's pitch with it.
    pub fn set_speed(&mut self, speed: f64) {
        let speed = speed.clamp(0.0625, 16.0);
        if speed != self.speed {
            self.speed = speed;
            self.speed_changed = true;
        }
    }

    /// The clock, in seconds from the start.
    pub fn time(&self) -> f64 {
        self.clock
    }

    /// The video's length in seconds.
    pub fn duration(&self) -> f64 {
        self.duration
    }

    /// It plays (neither paused nor ended).
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// It reached its end and stopped.
    pub fn has_ended(&self) -> bool {
        self.ended
    }

    /// The pictures are decoded on the GPU's video engine (rather than by rav1d).
    pub fn decodes_on_gpu(&self) -> bool {
        matches!(self.decoding, Decoding::Gpu(_))
    }

    /// The texture the pictures are shown in ([`VideoTexture::sampled_index`] for
    /// materials and the UI, once [`VideoTexture::is_written`]).
    pub fn texture(&self) -> &VideoTexture {
        &self.texture
    }

    /// The stream's sequence header.
    pub fn sequence(&self) -> &SequenceHeader {
        &self.sequence
    }

    /// What happened since the last call (pictures shown, the end, the loop point).
    pub fn events(&mut self) -> impl Iterator<Item = VideoEvent> + '_ {
        self.events.drain(..)
    }

    /// Advances the video for a frame, before the frame graph is built: gives back the
    /// pictures finished frames read, applies the controls, moves the clock, decodes
    /// ahead and picks the picture due ([`Self::add_pass`] converts it).
    ///
    /// # Errors
    ///
    /// A damaged stream, a decoder that failed, or a sound that could not start.
    pub fn update(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        tick: Tick<'_>,
    ) -> Result<(), VideoError> {
        engine_profiler::zone!("video::update");
        let completed = tick.frame.saturating_sub(tick.in_flight);
        self.release_finished(device, allocator, bindless, completed);
        let audio = tick.audio;
        if let Some(time) = self.seek.take() {
            self.restart_at(time)?;
        }
        if self.playing && !self.waiting {
            self.advance(audio.as_deref(), tick.dt, tick.latency);
        }
        self.decode(device, allocator, bindless, tick.frame)?;
        self.pick(device)?;
        self.check_end();
        self.keep_sound(audio)
    }

    /// Adds the conversion of the picture due to the frame graph, if a new one is; the
    /// frame is number `frame` in slot `slot`. Call between [`Self::update`] and the
    /// frame graph's execution, before the passes that sample the texture.
    ///
    /// # Errors
    ///
    /// A failed submission of the decode wait (a lost device), or software planes that
    /// do not fit the stream's.
    pub fn add_pass<'g>(
        &mut self,
        fg: &mut FrameGraph<'g>,
        converter: &'g Converter,
        bindless: &'g Bindless,
        device: &Device,
        frame: u64,
        #[cfg_attr(not(feature = "software"), allow(unused_variables))] slot: usize,
    ) -> Result<(), VideoError> {
        let Some(due) = self.due.take() else {
            return Ok(());
        };
        let time = due.time;
        match (due.picture, &mut self.decoding) {
            (Picture::Gpu(picture), Decoding::Gpu(gpu)) => {
                let Some(source) = gpu.planes.source(&picture) else {
                    gpu.decoder.release(&picture);
                    return Ok(());
                };
                wait_for_decode(device, gpu.decoder.semaphore(), picture.ready)?;
                converter.add_pass(fg, bindless, &mut self.texture, &source, &self.conversion);
                self.in_use.push_back((picture, frame));
            }
            #[cfg(feature = "software")]
            (Picture::Software(picture), Decoding::Software(software)) => {
                let planes = crate::convert::Planes::from(&picture);
                let source = software.upload.stage(slot, &planes)?;
                converter.add_pass(fg, bindless, &mut self.texture, &source, &self.conversion);
            }
            // A picture of a decoder replaced since (it goes with it).
            #[allow(unreachable_patterns)]
            _ => return Ok(()),
        }
        self.events.push(VideoEvent::Shown(time));
        Ok(())
    }

    /// Destroys the video: stops its sound and its decoding thread, frees its GPU objects
    /// (bindless indices reused once frame `last_frame` has finished).
    ///
    /// # Safety
    ///
    /// `device`, `allocator` and `bindless` are those it was opened with; the GPU no
    /// longer uses its texture or its pictures (the frames that read them finished).
    pub unsafe fn destroy(
        mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        audio: Option<&mut Audio>,
        last_frame: u64,
    ) {
        if let (Some(sound), Some(audio)) = (&mut self.sound, audio)
            && let Some(voice) = sound.voice.take()
        {
            audio.stop(voice, 0.0);
        }
        self.ahead.clear();
        self.due = None;
        self.in_use.clear();
        // SAFETY: the caller's contract.
        unsafe {
            destroy_decoding(self.decoding, device, allocator, bindless, last_frame);
            for (gpu, _) in self.retired {
                gpu.planes.release(bindless, last_frame);
                gpu.decoder.destroy(device, allocator);
            }
            self.texture
                .destroy(device, allocator, bindless, last_frame);
        }
    }

    /// Gives back the GPU pictures read by frames up to `completed`, and destroys the
    /// replaced decoders those frames were the last to need.
    fn release_finished(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        completed: u64,
    ) {
        while let Some(&(picture, frame)) = self.in_use.front() {
            if frame > completed {
                break;
            }
            self.in_use.pop_front();
            if let Decoding::Gpu(gpu) = &mut self.decoding {
                gpu.decoder.release(&picture);
            }
        }
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].1 <= completed {
                let (gpu, _) = self.retired.swap_remove(i);
                gpu.planes.release(bindless, completed);
                // SAFETY: the frames that read its pictures finished; it is this
                // device's.
                unsafe { gpu.decoder.destroy(device, allocator) };
            } else {
                i += 1;
            }
        }
    }

    /// Resumes decoding and the clock at `time`.
    fn restart_at(&mut self, time: f64) -> Result<(), VideoError> {
        let time = time.clamp(0.0, self.duration);
        let key = keyframe_before(&self.units, nanoseconds(time));
        for ahead in self.ahead.drain(..).chain(self.due.take()) {
            discard(&mut self.decoding, ahead.picture);
        }
        match &mut self.decoding {
            Decoding::Gpu(gpu) => {
                gpu.decoder.reset();
                gpu.parser.reset();
                gpu.next = key;
            }
            #[cfg(feature = "software")]
            Decoding::Software(software) => software.seek(key)?,
        }
        self.clock = time;
        self.waiting = true;
        self.drained = false;
        self.ended = false;
        self.restart_sound = true;
        if let Some(sound) = &mut self.sound {
            sound.done = false;
        }
        Ok(())
    }

    /// Moves the clock: the sound's position while it plays, the time step otherwise.
    fn advance(&mut self, audio: Option<&Audio>, dt: f64, latency: f64) {
        let mut next = self.clock + dt * self.speed;
        if let (Some(sound), Some(audio)) = (&mut self.sound, audio)
            && let Some(voice) = sound.voice
        {
            match audio.stream_position(voice) {
                Some(frames) => next = sound.base + frames as f64 / RATE - latency * self.speed,
                None => {
                    // Ended (or stolen by a sound of higher priority).
                    sound.voice = None;
                    sound.done = true;
                }
            }
        }
        self.clock = self.clock.max(next);
    }

    /// Decodes ahead of the clock.
    fn decode(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame: u64,
    ) -> Result<(), VideoError> {
        let clock = self.clock;
        let refused = match &mut self.decoding {
            Decoding::Gpu(gpu) => {
                let mut refused = false;
                let mut budget = UNITS_PER_UPDATE;
                while self.ahead.len() < self.ahead_limit
                    && budget > 0
                    && gpu.next < self.units.len()
                {
                    budget -= 1;
                    let unit = self.units[gpu.next];
                    gpu.next += 1;
                    let bytes = unit.bytes(&self.data);
                    gpu.parser.parse(bytes, &mut gpu.unit)?;
                    let sequence = *gpu.parser.sequence().ok_or(Av1Error::MissingReference)?;
                    gpu.out.clear();
                    match gpu.decoder.decode(
                        device,
                        allocator,
                        bytes,
                        &gpu.unit,
                        &sequence,
                        unit.time_ns,
                        &mut gpu.out,
                    ) {
                        Ok(()) => {}
                        Err(GpuError::Unsupported(_)) => {
                            refused = true;
                            break;
                        }
                        Err(e) => return Err(e.into()),
                    }
                    for picture in gpu.out.drain(..) {
                        let ahead = Ahead {
                            time: picture.timestamp as f64 * 1e-9,
                            picture: Picture::Gpu(picture),
                        };
                        push(&mut self.ahead, clock, ahead, |late| {
                            if let Picture::Gpu(p) = late.picture {
                                gpu.decoder.release(&p);
                            }
                        });
                    }
                }
                self.drained = gpu.next >= self.units.len();
                refused
            }
            #[cfg(feature = "software")]
            Decoding::Software(software) => {
                software.receive(&mut self.ahead, self.ahead_limit, clock, &mut self.drained)?;
                false
            }
        };
        if refused {
            self.fall_back(device, allocator, bindless, frame)?;
        }
        Ok(())
    }

    /// The GPU refused a frame: rav1d decodes on from the key frame before the clock.
    fn fall_back(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame: u64,
    ) -> Result<(), VideoError> {
        let key = keyframe_before(&self.units, nanoseconds(self.clock));
        let software = start_software(
            device,
            allocator,
            bindless,
            &self.sequence,
            &self.data,
            &self.units,
            key,
            self.in_flight,
            self.threads,
            self.ahead_limit,
        )?;
        let old = core::mem::replace(&mut self.decoding, software);
        if let Decoding::Gpu(gpu) = old {
            // Its pictures not shown go with it; those converted are read until their
            // frames finish, and it with them.
            self.ahead.clear();
            self.due = None;
            let last = self.in_use.drain(..).map(|(_, f)| f).max().unwrap_or(0);
            self.retired.push((gpu, last.max(frame)));
        }
        self.drained = false;
        Ok(())
    }

    /// Picks the picture due: the newest the clock has reached, once decoded.
    fn pick(&mut self, device: &Device) -> Result<(), VideoError> {
        while self.ahead.len() >= 2 && reached(self.ahead[1].time, self.clock) {
            if let Some(late) = self.ahead.pop_front() {
                discard(&mut self.decoding, late.picture);
            }
        }
        let Some(front) = self.ahead.front() else {
            return Ok(());
        };
        let due = if self.waiting {
            // The picture of the time sought (or opened at): the first at or past it, or
            // the last before it once a later one or the end says it is the last.
            front.time >= self.clock - 1e-6 || self.ahead.len() >= 2 || self.drained
        } else {
            reached(front.time, self.clock)
        };
        if !due {
            return Ok(());
        }
        if let (Picture::Gpu(picture), Decoding::Gpu(gpu)) = (&front.picture, &self.decoding)
            && gpu.decoder.completed(device)? < picture.ready
        {
            // Still decoding: the picture shown stays a frame longer.
            return Ok(());
        }
        let Some(due) = self.ahead.pop_front() else {
            return Ok(());
        };
        if self.waiting {
            self.clock = self.clock.max(due.time);
            self.waiting = false;
        }
        if let Some(old) = self.due.replace(due) {
            discard(&mut self.decoding, old.picture);
        }
        Ok(())
    }

    /// The end: start over, or stop.
    fn check_end(&mut self) {
        if self.ended
            || self.waiting
            || !self.drained
            || !self.ahead.is_empty()
            || self.due.is_some()
            || self.clock < self.duration
        {
            return;
        }
        if self.looping {
            self.seek = Some(0.0);
            self.events.push(VideoEvent::Looped);
        } else {
            self.ended = true;
            self.playing = false;
            self.events.push(VideoEvent::Ended);
        }
    }

    /// Starts, stops and retunes the sound as the controls and the clock say.
    fn keep_sound(&mut self, audio: Option<&mut Audio>) -> Result<(), VideoError> {
        let (Some(sound), Some(audio)) = (&mut self.sound, audio) else {
            return Ok(());
        };
        let wanted = self.playing && !self.waiting && !sound.done && !self.ended;
        if let Some(voice) = sound.voice {
            if !wanted || self.restart_sound {
                audio.stop(voice, FADE);
                sound.voice = None;
            } else if self.speed_changed {
                audio.set_voice_pitch(voice, self.speed as f32);
            }
        }
        self.restart_sound = false;
        self.speed_changed = false;
        if !wanted || sound.voice.is_some() {
            return Ok(());
        }
        let mut stream = sound.track.fresh()?;
        let frame = ((self.clock - sound.start).max(0.0) * RATE).round() as u64;
        if frame >= stream.length() {
            sound.done = true;
            return Ok(());
        }
        stream.seek(frame)?;
        let voice = audio.play_stream(
            Box::new(stream),
            &PlayParams {
                bus: sound.bus,
                gain: sound.gain,
                pitch: self.speed as f32,
                ..PlayParams::default()
            },
        )?;
        sound.voice = Some(voice);
        sound.base = sound.start + frame as f64 / RATE;
        Ok(())
    }
}

/// Adds `ahead` behind the pictures decoded before it; a picture the clock has reached
/// makes those before it that the clock passed too late (`discard` takes them).
fn push(ahead: &mut VecDeque<Ahead>, clock: f64, picture: Ahead, mut discard: impl FnMut(Ahead)) {
    if reached(picture.time, clock) {
        while ahead.back().is_some_and(|b| reached(b.time, clock)) {
            if let Some(late) = ahead.pop_back() {
                discard(late);
            }
        }
    }
    ahead.push_back(picture);
}

/// Whether the clock has reached a picture's `time`: within a microsecond, since times
/// in seconds are not exact (a seek to 1.9 must show the picture at 1 900 000 000 ns).
fn reached(time: f64, clock: f64) -> bool {
    time <= clock + 1e-6
}

/// Seconds to the nanoseconds of the file's times.
fn nanoseconds(seconds: f64) -> i64 {
    (seconds * 1e9).round() as i64
}

/// Drops a picture not shown (a GPU one goes back to its decoder).
fn discard(decoding: &mut Decoding, picture: Picture) {
    if let (Picture::Gpu(picture), Decoding::Gpu(gpu)) = (&picture, decoding) {
        gpu.decoder.release(picture);
    }
}

/// The GPU's decoder for the stream, or `None` when it does not take it.
fn start_gpu(
    device: &Device,
    allocator: &mut Allocator,
    bindless: &Bindless,
    sequence: &SequenceHeader,
    serial: u32,
    parser: Parser,
) -> Result<Option<Box<GpuDecoding>>, VideoError> {
    let decoder = match GpuDecoder::new(
        device,
        allocator,
        sequence,
        serial,
        &GpuDecoderDesc::default(),
    ) {
        Ok(decoder) => decoder,
        Err(GpuError::Unsupported(_) | GpuError::NoDecodeQueue) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let planes = match DecoderPlanes::new(&decoder, bindless) {
        Ok(planes) => planes,
        Err(e) => {
            // SAFETY: nothing decoded yet.
            unsafe { decoder.destroy(device, allocator) };
            return match e {
                GpuError::Unsupported(_) => Ok(None),
                e => Err(e.into()),
            };
        }
    };
    Ok(Some(Box::new(GpuDecoding {
        decoder,
        planes,
        parser,
        unit: TemporalUnit::default(),
        next: 0,
        out: Vec::with_capacity(4),
    })))
}

/// rav1d's decoding on a thread of its own, from unit `from`.
#[cfg(feature = "software")]
#[allow(clippy::too_many_arguments)]
fn start_software(
    device: &Device,
    allocator: &mut Allocator,
    bindless: &Bindless,
    sequence: &SequenceHeader,
    data: &Arc<[u8]>,
    units: &Arc<[Indexed]>,
    from: usize,
    in_flight: usize,
    threads: u32,
    ahead: usize,
) -> Result<Decoding, VideoError> {
    software::Decoding::start(
        device, allocator, bindless, sequence, data, units, from, in_flight, threads, ahead,
    )
    .map(Decoding::Software)
}

#[cfg(not(feature = "software"))]
#[allow(clippy::too_many_arguments)]
fn start_software(
    _device: &Device,
    _allocator: &mut Allocator,
    _bindless: &Bindless,
    _sequence: &SequenceHeader,
    _data: &Arc<[u8]>,
    _units: &Arc<[Indexed]>,
    _from: usize,
    _in_flight: usize,
    _threads: u32,
    _ahead: usize,
) -> Result<Decoding, VideoError> {
    Err(VideoError::NoDecoder(
        "the GPU does not take the stream and the build has no software decoder",
    ))
}

/// Frees a decoding's GPU objects and stops its thread.
///
/// # Safety
///
/// As [`VideoPlayer::destroy`].
unsafe fn destroy_decoding(
    decoding: Decoding,
    device: &Device,
    allocator: &mut Allocator,
    bindless: &Bindless,
    last_frame: u64,
) {
    match decoding {
        Decoding::Gpu(gpu) => {
            gpu.planes.release(bindless, last_frame);
            // SAFETY: the caller's contract.
            unsafe { gpu.decoder.destroy(device, allocator) };
        }
        #[cfg(feature = "software")]
        Decoding::Software(software) => {
            // SAFETY: the caller's contract.
            unsafe { software.destroy(device, allocator, bindless, last_frame) };
        }
    }
}

/// rav1d's decoding on a thread of its own.
#[cfg(feature = "software")]
mod software {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
    use std::thread::JoinHandle;

    use engine_rhi::{Allocator, Bindless, Device};

    use super::{Ahead, Picture, VideoError, push};
    use crate::av1::sequence::SequenceHeader;
    use crate::convert::PlaneUpload;
    use crate::index::Indexed;
    use crate::software::{Settings, SoftwareDecoder};

    enum Command {
        Seek { unit: usize, generation: u32 },
        Stop,
    }

    #[allow(clippy::large_enum_variant)]
    enum Message {
        Picture(crate::software::Picture),
        End,
        Error(String),
    }

    /// The thread, the planes' upload, and which seek the pictures belong to.
    pub(super) struct Decoding {
        worker: Worker,
        pub(super) upload: PlaneUpload,
        generation: u32,
    }

    /// The thread and its channels; dropping it stops the thread.
    struct Worker {
        commands: Sender<Command>,
        pictures: Option<Receiver<(u32, Message)>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.commands.send(Command::Stop);
            // A thread waiting to hand over a picture wakes when the receiver goes.
            self.pictures = None;
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Decoding {
        #[allow(clippy::too_many_arguments)]
        pub(super) fn start(
            device: &Device,
            allocator: &mut Allocator,
            bindless: &Bindless,
            sequence: &SequenceHeader,
            data: &Arc<[u8]>,
            units: &Arc<[Indexed]>,
            from: usize,
            in_flight: usize,
            threads: u32,
            ahead: usize,
        ) -> Result<Self, VideoError> {
            let upload = PlaneUpload::new(
                device,
                allocator,
                bindless,
                &sequence.color,
                [sequence.max_width(), sequence.max_height()],
                in_flight,
            )?;
            let (commands, command_rx) = mpsc::channel();
            let (picture_tx, pictures) = mpsc::sync_channel(ahead.max(1));
            let (data, units) = (data.clone(), units.clone());
            let thread = std::thread::Builder::new()
                .name(String::from("video decode"))
                .spawn(move || work(&data, &units, from, threads, &command_rx, &picture_tx));
            let thread = match thread {
                Ok(thread) => thread,
                Err(e) => {
                    // SAFETY: never used by the GPU.
                    unsafe { upload.destroy(device, allocator, bindless, 0) };
                    return Err(VideoError::Software(e.to_string()));
                }
            };
            Ok(Self {
                worker: Worker {
                    commands,
                    pictures: Some(pictures),
                    thread: Some(thread),
                },
                upload,
                generation: 0,
            })
        }

        /// Decoding resumes at unit `unit`; pictures decoded before are dropped as they
        /// come.
        pub(super) fn seek(&mut self, unit: usize) -> Result<(), VideoError> {
            self.generation = self.generation.wrapping_add(1);
            self.worker
                .commands
                .send(Command::Seek {
                    unit,
                    generation: self.generation,
                })
                .map_err(|_| VideoError::Software(String::from("the decoding thread stopped")))
        }

        /// Takes the pictures decoded, up to `limit` ahead.
        pub(super) fn receive(
            &mut self,
            ahead: &mut VecDeque<Ahead>,
            limit: usize,
            clock: f64,
            drained: &mut bool,
        ) -> Result<(), VideoError> {
            let Some(pictures) = &self.worker.pictures else {
                return Ok(());
            };
            while ahead.len() < limit {
                match pictures.try_recv() {
                    Ok((generation, _)) if generation != self.generation => {}
                    Ok((_, Message::Picture(picture))) => {
                        let picture = Ahead {
                            time: picture.timestamp() as f64 * 1e-9,
                            picture: Picture::Software(picture),
                        };
                        push(ahead, clock, picture, drop);
                    }
                    Ok((_, Message::End)) => *drained = true,
                    Ok((_, Message::Error(e))) => return Err(VideoError::Software(e)),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        return Err(VideoError::Software(String::from(
                            "the decoding thread stopped",
                        )));
                    }
                }
            }
            Ok(())
        }

        /// Stops the thread and frees the upload's GPU objects.
        ///
        /// # Safety
        ///
        /// As `VideoPlayer::destroy`.
        pub(super) unsafe fn destroy(
            self,
            device: &Device,
            allocator: &mut Allocator,
            bindless: &Bindless,
            last_frame: u64,
        ) {
            let Self { worker, upload, .. } = self;
            drop(worker);
            // SAFETY: the caller's contract.
            unsafe { upload.destroy(device, allocator, bindless, last_frame) };
        }
    }

    /// The thread: decodes unit after unit, handing the pictures over (waiting while the
    /// player has enough), and follows the seeks.
    fn work(
        data: &[u8],
        units: &[Indexed],
        mut next: usize,
        threads: u32,
        commands: &Receiver<Command>,
        pictures: &SyncSender<(u32, Message)>,
    ) {
        let settings = Settings {
            threads,
            max_frame_delay: 0,
            apply_grain: true,
            output_invisible_frames: false,
        };
        let mut generation = 0;
        let mut decoder = match SoftwareDecoder::new(&settings) {
            Ok(decoder) => decoder,
            Err(e) => {
                let _ = pictures.send((generation, Message::Error(e.to_string())));
                return;
            }
        };
        let mut ready = Vec::new();
        let mut ended = false;
        loop {
            // At the end, wait for a seek; otherwise look for one between units.
            let command = if ended {
                match commands.recv() {
                    Ok(command) => Some(command),
                    Err(_) => return,
                }
            } else {
                match commands.try_recv() {
                    Ok(command) => Some(command),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            };
            match command {
                Some(Command::Seek {
                    unit,
                    generation: g,
                }) => {
                    decoder.flush();
                    next = unit;
                    generation = g;
                    ended = false;
                    continue;
                }
                Some(Command::Stop) => return,
                None => {}
            }
            let result = match units.get(next) {
                Some(unit) => {
                    next += 1;
                    decoder.decode(unit.bytes(data), unit.time_ns, |p| ready.push(p))
                }
                None => {
                    ended = true;
                    decoder.finish(|p| ready.push(p))
                }
            };
            if let Err(error) = result {
                ready.clear();
                ended = true;
                if pictures
                    .send((generation, Message::Error(error.to_string())))
                    .is_err()
                {
                    return;
                }
                continue;
            }
            for picture in ready.drain(..) {
                if pictures
                    .send((generation, Message::Picture(picture)))
                    .is_err()
                {
                    return;
                }
            }
            if ended && pictures.send((generation, Message::End)).is_err() {
                return;
            }
        }
    }
}
