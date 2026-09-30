//! Algorithmic reverb: diffusion into a feedback delay network (ADR 0049,
//! docs/research/reverb.md).
//!
//! The stereo input goes through a pre-delay, is spread over [`CHANNELS`] channels and
//! diffused by a few steps of short delays, polarity flips and a Hadamard mix (a dense
//! echo pattern within tens of milliseconds: the early reflections). The diffused signal
//! feeds a feedback delay network: [`CHANNELS`] delay lines of different lengths, mixed
//! each round trip by a Householder matrix (energy-preserving), each line scaled so that
//! any path loses 60 dB in the decay time, with a one-pole low-pass in the loop so high
//! frequencies die sooner (air, soft walls). Both matrices are orthogonal, so the loop's
//! gain is exactly what the decay time asks for. Jot's feedback delay networks (1991) and
//! Luff's "Let's Write a Reverb" (ADC 2021) describe the design; no code was copied.

use super::{Block, Effect, time_coefficient};

/// Channels of the diffuser and the feedback network.
const CHANNELS: usize = 8;
/// Diffusion steps before the feedback network.
const DIFFUSION_STEPS: usize = 4;
/// Longest room (SIZE = 1): the diffusion span and the network's longest line.
const MAX_DIFFUSION_SECONDS: f32 = 0.08;
const MAX_LINE_SECONDS: f32 = 0.2;
/// Longest pre-delay.
const MAX_PRE_DELAY: f32 = 0.5;

/// A delay line with a fixed capacity and a variable length (read `length` samples back).
#[derive(Debug, Clone)]
struct Line {
    buffer: Box<[f32]>,
    write: usize,
}

impl Line {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![0.0; capacity.max(2)].into_boxed_slice(),
            write: 0,
        }
    }

    /// The sample `length` writes ago (1 ≤ length < capacity).
    #[inline]
    fn read(&self, length: usize) -> f32 {
        // Wrap by a compare instead of `%`: a division per access would cost more than
        // the rest of the reverb.
        let read = if self.write >= length {
            self.write - length
        } else {
            self.write + self.buffer.len() - length
        };
        self.buffer[read]
    }

    #[inline]
    fn write(&mut self, x: f32) {
        self.buffer[self.write] = x;
        self.write += 1;
        if self.write == self.buffer.len() {
            self.write = 0;
        }
    }

    /// Writes `x` and returns the sample `length` writes ago (1 ≤ length < capacity).
    #[inline]
    fn push(&mut self, x: f32, length: usize) -> f32 {
        let out = self.read(length);
        self.write(x);
        out
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }
}

/// In-place orthonormal Hadamard transform of 8 values (unitary: energy is kept).
#[inline]
fn hadamard(x: &mut [f32; CHANNELS]) {
    let mut h = 1;
    while h < CHANNELS {
        let mut i = 0;
        while i < CHANNELS {
            for j in i..i + h {
                let (a, b) = (x[j], x[j + h]);
                x[j] = a + b;
                x[j + h] = a - b;
            }
            i += h * 2;
        }
        h *= 2;
    }
    let scale = (1.0 / CHANNELS as f32).sqrt();
    for v in x.iter_mut() {
        *v *= scale;
    }
}

/// In-place Householder reflection I − 2/N · 1·1ᵀ (unitary, every line feeds every line).
#[inline]
fn householder(x: &mut [f32; CHANNELS]) {
    let sum: f32 = x.iter().sum();
    let k = sum * (2.0 / CHANNELS as f32);
    for v in x.iter_mut() {
        *v -= k;
    }
}

/// A small deterministic generator (xorshift), so every reverb of the same size sounds the
/// same.
struct Random(u32);

impl Random {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 >> 8) as f32 / (1 << 24) as f32
    }
}

/// A stereo algorithmic reverb (see the module documentation).
///
/// Parameters: [`Reverb::DECAY`] (RT60 in seconds, 0.1..30), [`Reverb::SIZE`] (0..1, how
/// long the echoes are; meant to be set per room, changes may click while the tail rings),
/// [`Reverb::DAMPING`] (Hz where the loop's low-pass starts; 20 kHz and above: none),
/// [`Reverb::PRE_DELAY`] (seconds, up to 0.5), [`Reverb::EARLY`] (level of the early
/// reflections, 0..1), [`Reverb::MIX`] (0 dry .. 1 wet; a reverb on a send bus is fully
/// wet).
#[derive(Debug, Clone)]
pub struct Reverb {
    decay: f32,
    size: f32,
    damping: f32,
    pre_delay: f32,
    early: f32,
    mix: f32,
    sample_rate: u32,
    pre: [Line; 2],
    diffusers: Vec<[Line; CHANNELS]>,
    /// Per diffusion step: delay lengths (samples) at SIZE = 1, and polarity flips.
    diffuser_lengths: [[f32; CHANNELS]; DIFFUSION_STEPS],
    flips: [[f32; CHANNELS]; DIFFUSION_STEPS],
    lines: [Line; CHANNELS],
    /// Feedback lines' lengths (samples) at SIZE = 1.
    line_lengths: [f32; CHANNELS],
    /// Per line: the loop low-pass state.
    lowpass: [f32; CHANNELS],
    /// Derived from the parameters on the next block.
    dirty: bool,
    current: Derived,
}

/// What the parameters come to at the current sample rate.
#[derive(Debug, Clone, Copy, Default)]
struct Derived {
    pre: usize,
    diffuser: [[usize; CHANNELS]; DIFFUSION_STEPS],
    line: [usize; CHANNELS],
    gain: [f32; CHANNELS],
    /// Loop low-pass: y += (x − y) · (1 − pole).
    pole: f32,
}

impl Reverb {
    /// Parameter index: decay time (RT60) in seconds.
    pub const DECAY: u32 = 0;
    /// Parameter index: room size (0..1).
    pub const SIZE: u32 = 1;
    /// Parameter index: damping frequency in Hz.
    pub const DAMPING: u32 = 2;
    /// Parameter index: pre-delay in seconds.
    pub const PRE_DELAY: u32 = 3;
    /// Parameter index: early reflections level (0..1).
    pub const EARLY: u32 = 4;
    /// Parameter index: wet/dry mix (0..1).
    pub const MIX: u32 = 5;

    /// A reverb decaying 60 dB in `decay` seconds, of room size `size` (0..1), fully wet
    /// (for a send bus), with a gentle 8 kHz damping, 20 ms pre-delay and half-level early
    /// reflections.
    pub fn new(decay: f32, size: f32) -> Self {
        let mut random = Random(0x2545_F491);
        // Diffusion steps span growing ranges (each twice the one before), a random length
        // within each channel's slot so the echoes do not line up.
        let mut diffuser_lengths = [[0.0; CHANNELS]; DIFFUSION_STEPS];
        let mut flips = [[1.0; CHANNELS]; DIFFUSION_STEPS];
        let total: f32 = (0..DIFFUSION_STEPS).map(|s| (1 << s) as f32).sum();
        for (step, (lengths, flips)) in diffuser_lengths.iter_mut().zip(&mut flips).enumerate() {
            let span = MAX_DIFFUSION_SECONDS * (1 << step) as f32 / total;
            for (c, (length, flip)) in lengths.iter_mut().zip(flips.iter_mut()).enumerate() {
                let slot = span / CHANNELS as f32;
                *length = slot * (c as f32 + random.next());
                *flip = if random.next() < 0.5 { -1.0 } else { 1.0 };
            }
        }
        // Feedback lines between 40% and 100% of the longest, spread exponentially.
        let mut line_lengths = [0.0; CHANNELS];
        for (c, length) in line_lengths.iter_mut().enumerate() {
            let t = (c as f32 + random.next() * 0.8) / CHANNELS as f32;
            *length = MAX_LINE_SECONDS * 0.4f32.powf(1.0 - t);
        }
        let mut reverb = Self {
            decay,
            size,
            damping: 8_000.0,
            pre_delay: 0.02,
            early: 0.5,
            mix: 1.0,
            sample_rate: 0,
            pre: [Line::new(2), Line::new(2)],
            diffusers: Vec::new(),
            diffuser_lengths,
            flips,
            lines: core::array::from_fn(|_| Line::new(2)),
            line_lengths,
            lowpass: [0.0; CHANNELS],
            dirty: true,
            current: Derived::default(),
        };
        reverb.prepare(48_000);
        reverb
    }

    /// Recomputes lengths and gains from the parameters (no allocation).
    fn derive(&mut self) {
        let fs = self.sample_rate as f32;
        let size = self.size.clamp(0.0, 1.0);
        // Size scales every length between 30% and 100% of the longest room.
        let scale = 0.3 + 0.7 * size;
        let decay = self.decay.clamp(0.1, 30.0);
        let mut d = Derived {
            pre: ((self.pre_delay.clamp(0.0, MAX_PRE_DELAY) * fs) as usize).max(1),
            ..Derived::default()
        };
        for (step, lengths) in self.diffuser_lengths.iter().enumerate() {
            for (c, &seconds) in lengths.iter().enumerate() {
                d.diffuser[step][c] = ((seconds * scale * fs) as usize).max(1);
            }
        }
        for (c, &seconds) in self.line_lengths.iter().enumerate() {
            let samples = ((seconds * scale * fs) as usize).max(1);
            d.line[c] = samples;
            // 60 dB over `decay` seconds, whatever the path: 10^(−3 · length / RT60).
            d.gain[c] = 10f32.powf(-3.0 * samples as f32 / (decay * fs));
        }
        d.pole = if self.damping >= 20_000.0 {
            0.0
        } else {
            time_coefficient(
                1.0 / (2.0 * core::f32::consts::PI * self.damping.max(20.0)),
                self.sample_rate,
            )
        };
        self.current = d;
        self.dirty = false;
    }
}

impl Effect for Reverb {
    fn prepare(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        let fs = self.sample_rate as f32;
        let pre = (MAX_PRE_DELAY * fs) as usize + 2;
        self.pre = [Line::new(pre), Line::new(pre)];
        self.diffusers = self
            .diffuser_lengths
            .iter()
            .map(|lengths| core::array::from_fn(|c| Line::new((lengths[c] * fs) as usize + 2)))
            .collect();
        self.lines = core::array::from_fn(|c| Line::new((self.line_lengths[c] * fs) as usize + 2));
        self.reset();
        self.dirty = true;
    }

    fn set_param(&mut self, index: u32, value: f32) {
        match index {
            Self::DECAY => self.decay = value,
            Self::SIZE => self.size = value,
            Self::DAMPING => self.damping = value,
            Self::PRE_DELAY => self.pre_delay = value,
            Self::EARLY => self.early = value,
            Self::MIX => self.mix = value,
            _ => return,
        }
        self.dirty = true;
    }

    fn process(&mut self, block: Block<'_>) {
        if self.dirty {
            self.derive();
        }
        let d = self.current;
        let mix = self.mix.clamp(0.0, 1.0);
        let dry = 1.0 - mix;
        let early = self.early.clamp(0.0, 1.0);
        // Output: the channels summed to two, alternating, back to the input's level.
        let out_scale = (2.0 / CHANNELS as f32).sqrt();
        let one_minus_pole = 1.0 - d.pole;
        for (l, r) in block.left.iter_mut().zip(block.right.iter_mut()) {
            let in_l = self.pre[0].push(*l, d.pre);
            let in_r = self.pre[1].push(*r, d.pre);
            // Spread over the channels: even ones from the left, odd from the right.
            let mut x: [f32; CHANNELS] =
                core::array::from_fn(|c| if c % 2 == 0 { in_l } else { in_r });
            for (step, lines) in self.diffusers.iter_mut().enumerate() {
                for c in 0..CHANNELS {
                    x[c] = lines[c].push(x[c], d.diffuser[step][c]) * self.flips[step][c];
                }
                hadamard(&mut x);
            }
            // The feedback network: read the lines, damp, mix, scale, write back with the
            // diffused input.
            for (line, (lowpass, &length)) in
                self.lines.iter().zip(self.lowpass.iter_mut().zip(&d.line))
            {
                *lowpass += (line.read(length) - *lowpass) * one_minus_pole;
            }
            let tail = self.lowpass;
            let mut fb = tail;
            householder(&mut fb);
            for c in 0..CHANNELS {
                // The tiny offset keeps a decaying tail out of the denormal range.
                self.lines[c].write(fb[c] * d.gain[c] + x[c] + 1e-20);
            }
            let (mut wet_l, mut wet_r) = (0.0, 0.0);
            for c in 0..CHANNELS {
                let v = tail[c] + x[c] * early;
                if c % 2 == 0 {
                    wet_l += v;
                } else {
                    wet_r += v;
                }
            }
            *l = *l * dry + wet_l * out_scale * mix;
            *r = *r * dry + wet_r * out_scale * mix;
        }
    }

    fn reset(&mut self) {
        for line in self.pre.iter_mut().chain(self.lines.iter_mut()) {
            line.clear();
        }
        for step in &mut self.diffusers {
            for line in step.iter_mut() {
                line.clear();
            }
        }
        self.lowpass = [0.0; CHANNELS];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn run(reverb: &mut Reverb, left: &mut [f32], right: &mut [f32]) {
        for (l, r) in left.chunks_mut(256).zip(right.chunks_mut(256)) {
            reverb.process(Block {
                left: l,
                right: r,
                sidechain: None,
            });
        }
    }

    /// RT60 from an impulse response by Schroeder's backward integration: the time the
    /// energy decay curve takes from −5 dB to −35 dB, doubled (T30).
    fn rt60(ir: &[f32]) -> f32 {
        let mut energy: Vec<f64> = ir.iter().map(|&x| f64::from(x) * f64::from(x)).collect();
        for i in (0..energy.len() - 1).rev() {
            energy[i] += energy[i + 1];
        }
        let total = energy[0];
        let db = |i: usize| 10.0 * (energy[i] / total).log10();
        let at = |level: f64| {
            (0..energy.len())
                .find(|&i| db(i) <= level)
                .unwrap_or(energy.len())
        };
        let (t5, t35) = (at(-5.0), at(-35.0));
        2.0 * (t35 - t5) as f32 / RATE as f32
    }

    fn impulse_response(reverb: &mut Reverb, seconds: f32) -> Vec<f32> {
        let n = (seconds * RATE as f32) as usize;
        let mut left = vec![0.0; n];
        let mut right = vec![0.0; n];
        left[0] = 1.0;
        run(reverb, &mut left, &mut right);
        left.iter().zip(&right).map(|(l, r)| l + r).collect()
    }

    #[test]
    fn the_tail_decays_in_the_decay_time() {
        for (decay, size) in [(0.5, 0.3), (1.5, 0.6), (4.0, 1.0)] {
            let mut reverb = Reverb::new(decay, size);
            reverb.prepare(RATE);
            reverb.set_param(Reverb::DAMPING, 20_000.0);
            reverb.set_param(Reverb::EARLY, 0.0);
            let ir = impulse_response(&mut reverb, decay * 1.5 + 0.5);
            let measured = rt60(&ir);
            assert!(
                (measured - decay).abs() < decay * 0.1,
                "decay {decay}, size {size}: measured RT60 {measured}"
            );
        }
    }

    #[test]
    fn damping_shortens_the_decay_of_high_frequencies() {
        let mut bright = Reverb::new(2.0, 0.6);
        bright.prepare(RATE);
        bright.set_param(Reverb::DAMPING, 20_000.0);
        let mut dark = Reverb::new(2.0, 0.6);
        dark.prepare(RATE);
        dark.set_param(Reverb::DAMPING, 2_000.0);
        let (b, d) = (
            impulse_response(&mut bright, 3.0),
            impulse_response(&mut dark, 3.0),
        );
        assert!(rt60(&d) < rt60(&b) * 0.9, "{} vs {}", rt60(&d), rt60(&b));
    }

    #[test]
    fn dry_mix_is_the_input_and_silence_stays_silent() {
        let mut reverb = Reverb::new(2.0, 0.5);
        reverb.prepare(RATE);
        reverb.set_param(Reverb::MIX, 0.0);
        let input: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.05).sin()).collect();
        let (mut l, mut r) = (input.clone(), input.clone());
        run(&mut reverb, &mut l, &mut r);
        assert_eq!(l, input);
        assert_eq!(r, input);
        let mut quiet = Reverb::new(2.0, 0.5);
        quiet.prepare(RATE);
        let (mut l, mut r) = (vec![0.0; 48_000], vec![0.0; 48_000]);
        run(&mut quiet, &mut l, &mut r);
        assert!(l.iter().chain(&r).all(|v| v.abs() < 1e-15));
    }

    #[test]
    fn long_decays_stay_bounded_and_spread_to_both_sides() {
        // Loud noise into the longest room with the longest decay: no blow-up.
        let mut reverb = Reverb::new(30.0, 1.0);
        reverb.prepare(RATE);
        reverb.set_param(Reverb::DAMPING, 20_000.0);
        let mut state = 1u32;
        let mut noise = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 8) as f32 / (1 << 23) as f32 - 1.0
        };
        let n = RATE as usize * 10;
        let mut l: Vec<f32> = (0..n).map(|_| noise()).collect();
        let mut r: Vec<f32> = (0..n).map(|_| noise()).collect();
        run(&mut reverb, &mut l, &mut r);
        let peak = l.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak.is_finite() && peak < 100.0, "{peak}");
        // An impulse on the left comes out on both sides.
        let mut reverb = Reverb::new(1.0, 0.5);
        reverb.prepare(RATE);
        let (mut l, mut r) = (vec![0.0; RATE as usize], vec![0.0; RATE as usize]);
        l[0] = 1.0;
        run(&mut reverb, &mut l, &mut r);
        let energy = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>();
        let (el, er) = (energy(&l), energy(&r));
        assert!(er > el * 0.5 && er < el * 2.0, "{el} {er}");
    }
}
