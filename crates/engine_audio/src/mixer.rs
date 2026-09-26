//! The audio-thread side: voices, buses, effects and the render loop.
//!
//! [`Renderer::render`] runs on the audio thread. It applies the commands queued since the
//! last call, decides which voices are mixed and which stay virtual, then renders in
//! sub-blocks of at most [`MAX_BLOCK`] frames: voices into their buses, buses in schedule
//! order (effects, pre-fader sends, fader, meters, post-fader sends, parent), the master
//! into the output. It never allocates, frees, locks or blocks: objects it no longer needs
//! go back to the game thread.

use crate::clip::{Clip, ClipData};
use crate::command::{BusChange, Command, NO_BUS, Return, VoiceChange, VoiceStart};
use crate::dsp::{Block, Effect};
use crate::graph::Schedule;
use crate::shared::Shared;
use engine_core::spsc::{Consumer, Producer};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Largest sub-block the mixer processes at once, in frames. A render call of any size is
/// split into sub-blocks this size or smaller; bus buffers stay in L2 (64 buses × 2
/// channels × 256 frames × 4 bytes = 128 KiB).
pub const MAX_BLOCK: usize = 256;

/// One bus's buffers: left then right, `MAX_BLOCK` frames each.
const REGION: usize = 2 * MAX_BLOCK;
/// 1.0 in 32.32 fixed point.
const ONE: u64 = 1 << 32;
const FRACTION: u64 = ONE - 1;
const INV_ONE: f32 = 1.0 / 4_294_967_296.0;
/// Shortest gain change in frames (about 1.3 ms at 48 kHz), so no change ever clicks.
const DECLICK: u32 = 64;
/// Below this effective gain (-80 dB) a voice is not mixed.
const AUDIBLE: f32 = 1e-4;
/// Output samples are limited to this magnitude (+12 dBFS) and NaN becomes silence, so a
/// misbehaving effect cannot blast the speakers.
const OUTPUT_LIMIT: f32 = 4.0;

/// A linear ramp towards a target, advanced frame by frame.
#[derive(Debug, Clone, Copy, Default)]
struct Ramp {
    value: f32,
    target: f32,
    step: f32,
    left: u32,
}

impl Ramp {
    fn at(value: f32) -> Self {
        Self {
            value,
            target: value,
            step: 0.0,
            left: 0,
        }
    }

    fn set(&mut self, target: f32, frames: u32) {
        self.target = target;
        if frames == 0 || self.value == target {
            self.value = target;
            self.step = 0.0;
            self.left = 0;
        } else {
            self.step = (target - self.value) / frames as f32;
            self.left = frames;
        }
    }

    /// Advances `n` frames without producing output.
    fn skip(&mut self, n: u32) {
        if n >= self.left {
            self.value = self.target;
            self.left = 0;
        } else {
            self.value += self.step * n as f32;
            self.left -= n;
        }
    }

    /// `out[i] += src[i] * gain`, with the gain ramping.
    #[inline]
    fn add_scaled(&mut self, out: &mut [f32], src: &[f32]) {
        let n = out.len().min(src.len());
        let k = (self.left as usize).min(n);
        let (out_ramp, out_flat) = out[..n].split_at_mut(k);
        let (src_ramp, src_flat) = src[..n].split_at(k);
        let mut g = self.value;
        for (o, s) in out_ramp.iter_mut().zip(src_ramp) {
            *o += s * g;
            g += self.step;
        }
        self.finish(k, g);
        if self.left == 0 {
            let g = self.value;
            for (o, s) in out_flat.iter_mut().zip(src_flat) {
                *o += s * g;
            }
        }
    }

    /// `buf[i] *= gain`, with the gain ramping.
    #[inline]
    fn scale(&mut self, buf: &mut [f32]) {
        let k = (self.left as usize).min(buf.len());
        let (ramp, flat) = buf.split_at_mut(k);
        let mut g = self.value;
        for x in ramp {
            *x *= g;
            g += self.step;
        }
        self.finish(k, g);
        if self.left == 0 {
            let g = self.value;
            if g != 1.0 {
                for x in flat {
                    *x *= g;
                }
            }
        }
    }

    #[inline]
    fn finish(&mut self, k: usize, g: f32) {
        if k > 0 {
            self.left -= k as u32;
            self.value = if self.left == 0 { self.target } else { g };
        }
    }
}

/// Channel gains for a source: constant-power pan for mono, balance for stereo.
fn channel_gains(gain: f32, pan: f32, stereo: bool) -> [f32; 2] {
    let p = pan.clamp(-1.0, 1.0);
    if stereo {
        [gain * (1.0 - p.max(0.0)), gain * (1.0 + p.min(0.0))]
    } else {
        let (sin, cos) = ((p + 1.0) * core::f32::consts::FRAC_PI_4).sin_cos();
        [gain * cos, gain * sin]
    }
}

fn seconds_to_frames(seconds: f32, sample_rate: u32) -> u32 {
    ((seconds.max(0.0) * sample_rate as f32) as u32).max(DECLICK)
}

#[derive(Default)]
struct Voice {
    active: bool,
    clip: Option<Clip>,
    bus: u16,
    /// Position in the clip, 32.32 frames.
    pos: u64,
    /// Advance per output frame, 32.32 frames.
    step: u64,
    pitch: f32,
    gain: f32,
    pan: f32,
    /// Current channel gains (user gain and pan, fades and virtual transitions).
    gains: [Ramp; 2],
    looping: bool,
    priority: u8,
    start_at: Option<u64>,
    stopping: bool,
    is_virtual: bool,
    want_real: bool,
    /// Has been mixed at least once (a voice that starts over the limits starts virtual,
    /// without a fade-out).
    mixed: bool,
    ended: bool,
}

impl Voice {
    fn stereo(&self) -> bool {
        self.clip.as_ref().is_some_and(|c| c.0.channels == 2)
    }

    fn targets(&self) -> [f32; 2] {
        if self.stopping {
            [0.0; 2]
        } else {
            channel_gains(self.gain, self.pan, self.stereo())
        }
    }

    fn set_gains(&mut self, frames: u32) {
        let [l, r] = self.targets();
        self.gains[0].set(l, frames);
        self.gains[1].set(r, frames);
    }

    fn update_step(&mut self, sample_rate: u32) {
        let clip_rate = self.clip.as_ref().map_or(sample_rate, |c| c.0.sample_rate);
        let ratio = f64::from(self.pitch.clamp(1.0 / 64.0, 64.0)) * f64::from(clip_rate)
            / f64::from(sample_rate.max(1));
        self.step = ((ratio * ONE as f64).round() as u64).clamp(1, 1 << 40);
    }
}

#[derive(Clone, Copy)]
struct BusState {
    gain: f32,
    pan: f32,
    muted: bool,
    fader: [Ramp; 2],
    limit: u32,
}

impl BusState {
    fn targets(&self) -> [f32; 2] {
        if self.muted {
            [0.0; 2]
        } else {
            channel_gains(self.gain, self.pan, true)
        }
    }

    fn retarget(&mut self, frames: u32) {
        let [l, r] = self.targets();
        self.fader[0].set(l, frames);
        self.fader[1].set(r, frames);
    }
}

impl Default for BusState {
    fn default() -> Self {
        Self {
            gain: 1.0,
            pan: 0.0,
            muted: false,
            fader: [Ramp::at(1.0); 2],
            limit: u32::MAX,
        }
    }
}

/// Reads up to `left.len()` frames of `clip` from `pos` (32.32 frames), advancing by `step`
/// per frame, with linear interpolation. Mono clips fill only `left`. Returns the frames
/// produced: fewer than requested when a one-shot clip ends.
fn resample(
    clip: &ClipData,
    pos: &mut u64,
    step: u64,
    looping: bool,
    left: &mut [f32],
    right: &mut [f32],
) -> usize {
    let frames = clip.frames as u64;
    let stereo = clip.channels == 2;
    let a = clip.channel(0);
    let b = clip.channel(1);
    let n = left.len();
    let last = (frames - 1) << 32;
    let mut i = 0;
    let mut p = *pos;
    while i < n {
        let index = p >> 32;
        if index >= frames {
            if looping {
                p %= frames << 32;
                continue;
            }
            break;
        }
        let remaining = n - i;
        if step == ONE && p & FRACTION == 0 {
            // Same rate, on a sample: a straight copy.
            let run = remaining.min((frames - index) as usize);
            let s = index as usize;
            left[i..i + run].copy_from_slice(&a[s..s + run]);
            if stereo {
                right[i..i + run].copy_from_slice(&b[s..s + run]);
            }
            i += run;
            p += (run as u64) << 32;
        } else if p < last {
            // Every position in this run has both neighbours inside the clip.
            let run = remaining.min(((last - 1 - p) / step + 1) as usize);
            let lerp = |src: &[f32], out: &mut [f32]| {
                let mut q = p;
                for o in out {
                    let s = (q >> 32) as usize;
                    let t = (q as u32) as f32 * INV_ONE;
                    *o = src[s] + (src[s + 1] - src[s]) * t;
                    q += step;
                }
            };
            lerp(a, &mut left[i..i + run]);
            if stereo {
                lerp(b, &mut right[i..i + run]);
            }
            i += run;
            p += run as u64 * step;
        } else {
            // The last frame leads into the loop start, or into silence.
            let s = (frames - 1) as usize;
            let t = (p as u32) as f32 * INV_ONE;
            let next = |src: &[f32]| if looping { src[0] } else { 0.0 };
            left[i] = a[s] + (next(a) - a[s]) * t;
            if stereo {
                right[i] = b[s] + (next(b) - b[s]) * t;
            }
            i += 1;
            p += step;
        }
    }
    *pos = p;
    i
}

/// Moves `pos` forward by `n` output frames without reading; `false` if a one-shot ended.
fn advance(clip: &ClipData, pos: &mut u64, step: u64, looping: bool, n: u64) -> bool {
    let end = (clip.frames as u64) << 32;
    let p = pos.saturating_add(step.saturating_mul(n));
    if p < end {
        *pos = p;
        true
    } else if looping {
        *pos = p % end;
        true
    } else {
        false
    }
}

/// Mutable views of two different buses' buffers.
fn two_regions(buf: &mut [f32], a: usize, b: usize) -> (&mut [f32], &mut [f32]) {
    if a < b {
        let (lo, hi) = buf.split_at_mut(b * REGION);
        (&mut lo[a * REGION..(a + 1) * REGION], &mut hi[..REGION])
    } else {
        let (lo, hi) = buf.split_at_mut(a * REGION);
        (&mut hi[..REGION], &mut lo[b * REGION..(b + 1) * REGION])
    }
}

fn region(buf: &mut [f32], bus: usize) -> &mut [f32] {
    &mut buf[bus * REGION..(bus + 1) * REGION]
}

fn add(out: &mut [f32], src: &[f32]) {
    for (o, s) in out.iter_mut().zip(src) {
        *o += s;
    }
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, x| m.max(x.abs()))
}

/// The audio-thread half of the mixer. Created with [`crate::Audio::new`]; move it to the
/// thread that feeds the output device (on Windows, `output::Output` does this) and call
/// [`Renderer::render`] from there.
pub struct Renderer {
    commands: Consumer<Command>,
    returns: Producer<Return>,
    /// Returns that did not fit in the ring; drained first next time.
    pending: Vec<Return>,
    shared: Arc<Shared>,
    sample_rate: u32,
    clock: u64,
    voices: Box<[Voice]>,
    /// Voice slots in the order they started (kept in order so mixing order, and so
    /// rounding, never depends on how render calls are sized).
    active: Vec<u32>,
    max_real: u32,
    /// Sort keys when voices compete for the real-voice limits.
    candidates: Vec<u64>,
    bus_counts: Box<[u32]>,
    /// Effective gain of each bus down to the master (for audibility).
    bus_gains: Box<[f32]>,
    buses: Box<[BusState]>,
    bus_live: Box<[bool]>,
    sends: Box<[Ramp]>,
    effects: Box<[Option<Box<dyn Effect>>]>,
    schedule: Box<Schedule>,
    buffers: Box<[f32]>,
    scratch: Box<[f32]>,
    /// Returns dropped without freeing because both the ring and `pending` were full
    /// (should stay 0; the rings are sized for the worst case).
    leaked: u64,
}

pub(crate) struct RendererParts {
    pub(crate) commands: Consumer<Command>,
    pub(crate) returns: Producer<Return>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) max_voices: usize,
    pub(crate) max_real: u32,
    pub(crate) max_buses: usize,
    pub(crate) max_sends: usize,
    pub(crate) max_effects: usize,
    pub(crate) schedule: Box<Schedule>,
}

impl Renderer {
    pub(crate) fn new(parts: RendererParts) -> Self {
        let RendererParts {
            commands,
            returns,
            shared,
            max_voices,
            max_real,
            max_buses,
            max_sends,
            max_effects,
            schedule,
        } = parts;
        let mut bus_live = vec![false; max_buses].into_boxed_slice();
        for step in &schedule.steps {
            bus_live[usize::from(step.bus)] = true;
        }
        Self {
            commands,
            returns,
            pending: Vec::with_capacity(max_voices + max_effects + 64),
            sample_rate: shared.sample_rate.load(Ordering::Relaxed).max(1),
            shared,
            clock: 0,
            voices: (0..max_voices).map(|_| Voice::default()).collect(),
            active: Vec::with_capacity(max_voices),
            max_real,
            candidates: Vec::with_capacity(max_voices),
            bus_counts: vec![0; max_buses].into_boxed_slice(),
            bus_gains: vec![0.0; max_buses].into_boxed_slice(),
            buses: vec![BusState::default(); max_buses].into_boxed_slice(),
            bus_live,
            sends: vec![Ramp::at(1.0); max_sends].into_boxed_slice(),
            effects: (0..max_effects).map(|_| None).collect(),
            schedule,
            buffers: vec![0.0; max_buses * REGION].into_boxed_slice(),
            scratch: vec![0.0; REGION].into_boxed_slice(),
            leaked: 0,
        }
    }

    /// The output sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Frames rendered so far (the mixer clock).
    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// Returns that could not be handed back and were dropped without being freed (a
    /// memory leak, never a free on the audio thread). Stays 0 unless the game thread stops
    /// calling [`crate::Audio::update`] for a very long time.
    pub fn leaked(&self) -> u64 {
        self.leaked
    }

    /// Switches to a new output rate (a device change). Re-prepares every effect, which may
    /// allocate; this is not a real-time operation.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let sample_rate = sample_rate.max(1);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        for &slot in &self.active {
            self.voices[slot as usize].update_step(sample_rate);
        }
        for effect in self.effects.iter_mut().flatten() {
            effect.prepare(sample_rate);
        }
        self.shared
            .sample_rate
            .store(sample_rate, Ordering::Relaxed);
    }

    /// Renders `out.len() / channels` frames, interleaved with `channels` channels. The
    /// mix is stereo: mono output gets the average, extra channels get silence.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        engine_profiler::zone!("audio::render");
        let channels = channels.max(1);
        self.flush_returns();
        while let Some(command) = self.commands.pop() {
            self.apply(command);
        }
        self.choose_real_voices();

        let frames = out.len() / channels;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            self.render_block(n);
            self.write_output(
                &mut out[done * channels..(done + n) * channels],
                channels,
                n,
            );
            done += n;
        }

        self.retire_ended();
        self.shared.frames.store(self.clock, Ordering::Relaxed);
    }

    fn give_back(&mut self, r: Return) {
        if !self.pending.is_empty() {
            self.keep(r);
        } else if let Err(r) = self.returns.push(r) {
            self.keep(r);
        }
    }

    fn keep(&mut self, r: Return) {
        if self.pending.len() < self.pending.capacity() {
            self.pending.push(r);
        } else {
            // Never allocate or free here: leak it instead.
            core::mem::forget(r);
            self.leaked += 1;
        }
    }

    fn flush_returns(&mut self) {
        while let Some(r) = self.pending.pop() {
            if let Err(r) = self.returns.push(r) {
                self.pending.push(r);
                break;
            }
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Play { slot, clip, start } => self.start_voice(slot, clip, start),
            Command::Voice { slot, change } => {
                let rate = self.sample_rate;
                let Some(v) = self.voices.get_mut(slot as usize) else {
                    return;
                };
                if !v.active {
                    return;
                }
                match change {
                    VoiceChange::Gain(gain, ramp) => {
                        v.gain = gain.max(0.0);
                        if !v.is_virtual {
                            v.set_gains(seconds_to_frames(ramp, rate));
                        }
                    }
                    VoiceChange::Pan(pan, ramp) => {
                        v.pan = pan;
                        if !v.is_virtual {
                            v.set_gains(seconds_to_frames(ramp, rate));
                        }
                    }
                    VoiceChange::Pitch(pitch) => {
                        v.pitch = pitch;
                        v.update_step(rate);
                    }
                    VoiceChange::Stop(fade) => {
                        v.stopping = true;
                        v.set_gains(seconds_to_frames(fade, rate));
                    }
                }
            }
            Command::Bus { bus, change } => {
                let rate = self.sample_rate;
                let Some(b) = self.buses.get_mut(usize::from(bus)) else {
                    return;
                };
                match change {
                    BusChange::Gain(gain, ramp) => {
                        b.gain = gain.max(0.0);
                        b.retarget(seconds_to_frames(ramp, rate));
                    }
                    BusChange::Pan(pan, ramp) => {
                        b.pan = pan;
                        b.retarget(seconds_to_frames(ramp, rate));
                    }
                    BusChange::Mute(muted) => {
                        b.muted = muted;
                        b.retarget(DECLICK);
                    }
                    BusChange::VoiceLimit(limit) => b.limit = limit,
                }
            }
            Command::SendGain { send, gain, ramp } => {
                let frames = seconds_to_frames(ramp, self.sample_rate);
                if let Some(s) = self.sends.get_mut(send as usize) {
                    s.set(gain.max(0.0), frames);
                }
            }
            Command::AddEffect {
                slot,
                mut effect,
                sample_rate,
            } => {
                if sample_rate != self.sample_rate {
                    effect.prepare(self.sample_rate);
                }
                if let Some(old) = self
                    .effects
                    .get_mut(slot as usize)
                    .and_then(|e| e.replace(effect))
                {
                    self.give_back(Return::Effect(old));
                }
            }
            Command::RemoveEffect { slot } => {
                if let Some(old) = self.effects.get_mut(slot as usize).and_then(Option::take) {
                    self.give_back(Return::Effect(old));
                }
            }
            Command::EffectParam { slot, index, value } => {
                if let Some(Some(effect)) = self.effects.get_mut(slot as usize) {
                    effect.set_param(index, value);
                }
            }
            Command::Schedule(schedule) => {
                self.bus_live.fill(false);
                for step in &schedule.steps {
                    self.bus_live[usize::from(step.bus)] = true;
                }
                let old = core::mem::replace(&mut self.schedule, schedule);
                self.give_back(Return::Schedule(old));
            }
        }
    }

    fn start_voice(&mut self, slot: u32, clip: Clip, start: VoiceStart) {
        let rate = self.sample_rate;
        let clock = self.clock;
        let Some(v) = self.voices.get_mut(slot as usize) else {
            self.give_back(Return::VoiceEnded { slot, clip });
            return;
        };
        let stereo = clip.0.channels == 2;
        *v = Voice {
            active: true,
            clip: Some(clip),
            bus: start.bus,
            pos: 0,
            step: 0,
            pitch: start.pitch,
            gain: start.gain.max(0.0),
            pan: start.pan,
            gains: [Ramp::at(0.0); 2],
            looping: start.looping,
            priority: start.priority,
            start_at: start.start_at,
            stopping: false,
            is_virtual: false,
            want_real: true,
            mixed: false,
            ended: false,
        };
        v.update_step(rate);
        if start.fade_in > 0.0 {
            v.set_gains(seconds_to_frames(start.fade_in, rate));
        } else {
            let [l, r] = channel_gains(v.gain, v.pan, stereo);
            v.gains = [Ramp::at(l), Ramp::at(r)];
        }
        // Started late: skip ahead so the voice stays in time with the clock.
        if let Some(at) = v.start_at
            && at < clock
        {
            v.start_at = None;
            let alive = match &v.clip {
                Some(c) => advance(&c.0, &mut v.pos, v.step, v.looping, clock - at),
                None => false,
            };
            if !alive {
                v.ended = true;
            }
        }
        self.active.push(slot);
    }

    /// Decides which voices are mixed this call: audible ones, by priority and loudness,
    /// within the global and per-bus limits. The rest stay virtual (position only).
    fn choose_real_voices(&mut self) {
        // Effective bus gains, master first (reverse schedule order visits parents first).
        for step in self.schedule.steps.iter().rev() {
            let b = &self.buses[usize::from(step.bus)];
            let own = b.fader[0].target.max(b.fader[1].target);
            let parent = if step.parent == NO_BUS {
                1.0
            } else {
                self.bus_gains[usize::from(step.parent)]
            };
            self.bus_gains[usize::from(step.bus)] = own * parent;
        }
        self.bus_counts.fill(0);
        let mut wanted = 0u32;
        let mut over_bus_limit = false;
        for &slot in &self.active {
            let v = &mut self.voices[slot as usize];
            if v.ended {
                continue;
            }
            let bus = if self.bus_live[usize::from(v.bus)] {
                v.bus
            } else {
                0
            };
            let targets = channel_gains(v.gain, v.pan, v.stereo());
            let audibility = targets[0].max(targets[1]) * self.bus_gains[usize::from(bus)];
            v.want_real = audibility >= AUDIBLE && !(v.stopping && v.is_virtual);
            if v.want_real {
                wanted += 1;
                let count = &mut self.bus_counts[usize::from(bus)];
                *count += 1;
                over_bus_limit |= *count > self.buses[usize::from(bus)].limit;
                // Priority (high first), then audibility (loud first), then slot: a total
                // order, so the choice is deterministic.
                let key = (u64::from(255 - v.priority) << 56)
                    | (u64::from(u32::MAX - audibility.to_bits()) << 24)
                    | u64::from(slot & 0xff_ffff);
                self.candidates.push(key);
            }
        }
        if wanted > self.max_real || over_bus_limit {
            self.candidates.sort_unstable();
            self.bus_counts.fill(0);
            let mut taken = 0u32;
            for &key in &self.candidates {
                let slot = (key & 0xff_ffff) as usize;
                let v = &mut self.voices[slot];
                let bus = if self.bus_live[usize::from(v.bus)] {
                    usize::from(v.bus)
                } else {
                    0
                };
                if taken < self.max_real && self.bus_counts[bus] < self.buses[bus].limit {
                    taken += 1;
                    self.bus_counts[bus] += 1;
                } else {
                    v.want_real = false;
                }
            }
            wanted = taken;
        }
        self.candidates.clear();
        let total = self.active.len() as u32;
        self.shared.real_voices.store(wanted, Ordering::Relaxed);
        self.shared
            .virtual_voices
            .store(total.saturating_sub(wanted), Ordering::Relaxed);
    }

    fn render_block(&mut self, n: usize) {
        for step in &self.schedule.steps {
            let r = region(&mut self.buffers, usize::from(step.bus));
            r[..n].fill(0.0);
            r[MAX_BLOCK..MAX_BLOCK + n].fill(0.0);
        }
        for i in 0..self.active.len() {
            let slot = self.active[i] as usize;
            self.mix_voice(slot, n);
        }
        self.process_buses(n);
        self.clock += n as u64;
    }

    fn mix_voice(&mut self, slot: usize, n: usize) {
        let block_start = self.clock;
        let v = &mut self.voices[slot];
        if v.ended {
            return;
        }
        let offset = match v.start_at {
            Some(at) if at >= block_start + n as u64 => return,
            Some(at) => {
                v.start_at = None;
                (at - block_start) as usize
            }
            None => 0,
        };
        let m = n - offset;
        let Some(clip) = v.clip.as_ref() else {
            v.ended = true;
            return;
        };

        if !v.want_real && !v.mixed {
            v.is_virtual = true;
        }
        if v.is_virtual && !v.want_real {
            if v.stopping || !advance(&clip.0, &mut v.pos, v.step, v.looping, m as u64) {
                v.ended = true;
            }
            v.gains[0].skip(m as u32);
            v.gains[1].skip(m as u32);
            return;
        }
        if v.is_virtual {
            // Back from virtual: fade in from silence.
            v.is_virtual = false;
            let targets = if v.stopping {
                [0.0; 2]
            } else {
                channel_gains(v.gain, v.pan, clip.0.channels == 2)
            };
            v.gains = [Ramp::at(0.0); 2];
            v.gains[0].set(targets[0], DECLICK);
            v.gains[1].set(targets[1], DECLICK);
        } else if !v.want_real {
            // Becoming virtual: fade out over this block, then stop mixing.
            v.gains[0].set(0.0, m as u32);
            v.gains[1].set(0.0, m as u32);
            v.is_virtual = true;
        }

        v.mixed = true;
        let (left, right) = self.scratch.split_at_mut(MAX_BLOCK);
        let produced = resample(
            &clip.0,
            &mut v.pos,
            v.step,
            v.looping,
            &mut left[..m],
            &mut right[..m],
        );
        let stereo = clip.0.channels == 2;
        let bus = if self.bus_live[usize::from(v.bus)] {
            usize::from(v.bus)
        } else {
            0
        };
        let out = region(&mut self.buffers, bus);
        let (out_l, out_r) = out.split_at_mut(MAX_BLOCK);
        let src_r: &[f32] = if stereo {
            &right[..produced]
        } else {
            &left[..produced]
        };
        v.gains[0].add_scaled(&mut out_l[offset..offset + produced], &left[..produced]);
        v.gains[1].add_scaled(&mut out_r[offset..offset + produced], src_r);

        if produced < m || (v.stopping && v.gains[0].left == 0 && v.gains[1].left == 0) {
            v.ended = true;
        }
    }

    fn process_buses(&mut self, n: usize) {
        let Self {
            schedule,
            buffers,
            effects,
            buses,
            bus_live,
            sends,
            shared,
            ..
        } = self;
        for step in &schedule.steps {
            let b = usize::from(step.bus);

            for fx in &schedule.effects[step.effects.0 as usize..step.effects.1 as usize] {
                let Some(Some(effect)) = effects.get_mut(fx.slot as usize) else {
                    continue;
                };
                let side = usize::from(fx.sidechain);
                if fx.sidechain != NO_BUS && side != b && bus_live[side] {
                    let (dst, src) = two_regions(buffers, b, side);
                    let (l, r) = dst.split_at_mut(MAX_BLOCK);
                    let (sl, sr) = src.split_at(MAX_BLOCK);
                    effect.process(Block {
                        left: &mut l[..n],
                        right: &mut r[..n],
                        sidechain: Some([&sl[..n], &sr[..n]]),
                    });
                } else {
                    let (l, r) = region(buffers, b).split_at_mut(MAX_BLOCK);
                    effect.process(Block {
                        left: &mut l[..n],
                        right: &mut r[..n],
                        sidechain: None,
                    });
                }
            }

            let bus_sends = &schedule.sends[step.sends.0 as usize..step.sends.1 as usize];
            let mut send = |pre_fader: bool, buffers: &mut [f32]| {
                for s in bus_sends.iter().filter(|s| s.pre_fader == pre_fader) {
                    let target = usize::from(s.target);
                    if target == b || !bus_live[target] {
                        continue;
                    }
                    let Some(gain) = sends.get_mut(s.send as usize) else {
                        continue;
                    };
                    let (src, dst) = two_regions(buffers, b, target);
                    let (sl, sr) = src.split_at(MAX_BLOCK);
                    let (dl, dr) = dst.split_at_mut(MAX_BLOCK);
                    let mut right_gain = *gain;
                    gain.add_scaled(&mut dl[..n], &sl[..n]);
                    right_gain.add_scaled(&mut dr[..n], &sr[..n]);
                }
            };
            send(true, buffers);

            let state = &mut buses[b];
            let (l, r) = region(buffers, b).split_at_mut(MAX_BLOCK);
            state.fader[0].scale(&mut l[..n]);
            state.fader[1].scale(&mut r[..n]);
            shared.raise_peak(b, 0, peak(&l[..n]));
            shared.raise_peak(b, 1, peak(&r[..n]));

            send(false, buffers);

            if step.parent != NO_BUS {
                let parent = usize::from(step.parent);
                if parent != b && bus_live[parent] {
                    let (src, dst) = two_regions(buffers, b, parent);
                    let (sl, sr) = src.split_at(MAX_BLOCK);
                    let (dl, dr) = dst.split_at_mut(MAX_BLOCK);
                    add(&mut dl[..n], &sl[..n]);
                    add(&mut dr[..n], &sr[..n]);
                }
            }
        }
    }

    fn write_output(&mut self, out: &mut [f32], channels: usize, n: usize) {
        let (l, r) = self.buffers[..REGION].split_at(MAX_BLOCK);
        let clean = |x: f32| {
            if x.is_nan() {
                0.0
            } else {
                x.clamp(-OUTPUT_LIMIT, OUTPUT_LIMIT)
            }
        };
        match channels {
            1 => {
                for (i, o) in out[..n].iter_mut().enumerate() {
                    *o = clean(0.5 * (l[i] + r[i]));
                }
            }
            2 => {
                for (i, o) in out[..2 * n].chunks_exact_mut(2).enumerate() {
                    o[0] = clean(l[i]);
                    o[1] = clean(r[i]);
                }
            }
            c => {
                for (i, o) in out[..c * n].chunks_exact_mut(c).enumerate() {
                    o[0] = clean(l[i]);
                    o[1] = clean(r[i]);
                    o[2..].fill(0.0);
                }
            }
        }
    }

    fn retire_ended(&mut self) {
        let mut i = 0;
        while i < self.active.len() {
            let slot = self.active[i];
            let v = &mut self.voices[slot as usize];
            if v.ended {
                v.active = false;
                let clip = v.clip.take();
                // Order-preserving removal (see `active`).
                self.active.remove(i);
                if let Some(clip) = clip {
                    self.give_back(Return::VoiceEnded { slot, clip });
                }
            } else {
                i += 1;
            }
        }
    }
}
