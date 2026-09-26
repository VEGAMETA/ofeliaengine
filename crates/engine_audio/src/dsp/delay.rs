//! Feedback delay (echo).

use super::{Block, Effect, time_coefficient};

/// A stereo feedback delay. The delay time glides to a new value instead of jumping (no
/// clicks), and reads between samples with linear interpolation.
///
/// Parameters: [`Delay::TIME`] (seconds, up to the maximum given at creation),
/// [`Delay::FEEDBACK`] (0..0.98), [`Delay::MIX`] (0 dry .. 1 wet).
#[derive(Debug, Clone)]
pub struct Delay {
    max_time: f32,
    time: f32,
    feedback: f32,
    mix: f32,
    sample_rate: u32,
    lines: [Box<[f32]>; 2],
    write: usize,
    /// Current delay in frames, gliding towards `time * sample_rate`.
    current: f32,
    glide: f32,
}

impl Delay {
    /// Parameter index: delay time in seconds.
    pub const TIME: u32 = 0;
    /// Parameter index: feedback (0..0.98).
    pub const FEEDBACK: u32 = 1;
    /// Parameter index: wet/dry mix (0..1).
    pub const MIX: u32 = 2;

    /// A delay of `time` seconds (at most 2 s; see [`Delay::with_max_time`]).
    pub fn new(time: f32, feedback: f32, mix: f32) -> Self {
        Self::with_max_time(2.0, time, feedback, mix)
    }

    /// Like [`Delay::new`] with the maximum delay time in seconds (sizes the buffers).
    pub fn with_max_time(max_time: f32, time: f32, feedback: f32, mix: f32) -> Self {
        let mut d = Self {
            max_time: max_time.max(0.001),
            time,
            feedback,
            mix,
            sample_rate: 0,
            lines: [Box::new([]), Box::new([])],
            write: 0,
            current: 0.0,
            glide: 0.0,
        };
        d.prepare(48_000);
        d
    }

    fn target(&self) -> f32 {
        let len = self.lines[0].len() as f32;
        (self.time.clamp(0.0, self.max_time) * self.sample_rate as f32).clamp(1.0, len - 2.0)
    }
}

impl Effect for Delay {
    fn prepare(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        let len = (self.max_time * self.sample_rate as f32) as usize + 3;
        self.lines = [
            vec![0.0; len].into_boxed_slice(),
            vec![0.0; len].into_boxed_slice(),
        ];
        // The delay time glides with a 50 ms time constant.
        self.glide = time_coefficient(0.05, self.sample_rate);
        self.reset();
    }

    fn set_param(&mut self, index: u32, value: f32) {
        match index {
            Self::TIME => self.time = value,
            Self::FEEDBACK => self.feedback = value,
            Self::MIX => self.mix = value,
            _ => {}
        }
    }

    fn process(&mut self, block: Block<'_>) {
        let len = self.lines[0].len();
        let target = self.target();
        let feedback = self.feedback.clamp(0.0, 0.98);
        let mix = self.mix.clamp(0.0, 1.0);
        let dry = 1.0 - mix;
        let glide = self.glide;
        let mut current = self.current;
        let mut write = self.write;
        let [line_l, line_r] = &mut self.lines;
        for (l, r) in block.left.iter_mut().zip(block.right.iter_mut()) {
            current = target + glide * (current - target);
            // Between the frames `whole` and `whole + 1` ago.
            let whole = current as usize;
            let frac = current - whole as f32;
            let i0 = (write + len - whole) % len;
            let i1 = (write + len - whole - 1) % len;
            let dl = line_l[i0] + (line_l[i1] - line_l[i0]) * frac;
            let dr = line_r[i0] + (line_r[i1] - line_r[i0]) * frac;
            // The tiny offset keeps decaying feedback out of the denormal range.
            line_l[write] = *l + dl * feedback + 1e-20;
            line_r[write] = *r + dr * feedback + 1e-20;
            *l = *l * dry + dl * mix;
            *r = *r * dry + dr * mix;
            write = (write + 1) % len;
        }
        self.current = current;
        self.write = write;
    }

    fn reset(&mut self) {
        for line in &mut self.lines {
            line.fill(0.0);
        }
        self.write = 0;
        self.current = self.target();
    }
}
