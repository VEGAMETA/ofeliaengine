//! Dynamics: a feed-forward compressor with sidechain, and a lookahead peak limiter.
//!
//! The compressor follows Giannoulis, Massberg and Reiss, "Digital Dynamic Range Compressor
//! Design — A Tutorial and Analysis" (JAES 60(6), 2012): a soft-knee gain computer in the
//! log domain followed by a smooth branching peak detector on the gain reduction.

use super::{Block, Effect, db_to_gain, flush_denormal, gain_to_db, time_coefficient};

/// A stereo-linked feed-forward compressor. With a sidechain the gain reduction follows
/// the sidechain bus instead of the input (ducking).
///
/// Parameters: [`Compressor::THRESHOLD_DB`], [`Compressor::RATIO`],
/// [`Compressor::ATTACK_MS`], [`Compressor::RELEASE_MS`], [`Compressor::KNEE_DB`],
/// [`Compressor::MAKEUP_DB`].
#[derive(Debug, Clone)]
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    knee_db: f32,
    makeup_db: f32,
    sample_rate: u32,
    attack: f32,
    release: f32,
    /// Smoothed gain reduction in dB (≥ 0).
    reduction_db: f32,
}

impl Compressor {
    /// Parameter index: threshold in dBFS.
    pub const THRESHOLD_DB: u32 = 0;
    /// Parameter index: ratio (4 means 4:1; clamped to ≥ 1).
    pub const RATIO: u32 = 1;
    /// Parameter index: attack time in milliseconds.
    pub const ATTACK_MS: u32 = 2;
    /// Parameter index: release time in milliseconds.
    pub const RELEASE_MS: u32 = 3;
    /// Parameter index: knee width in dB (0 is a hard knee).
    pub const KNEE_DB: u32 = 4;
    /// Parameter index: makeup gain in dB.
    pub const MAKEUP_DB: u32 = 5;

    /// A compressor with the given threshold and ratio, 5 ms attack, 120 ms release,
    /// 6 dB knee and no makeup gain.
    pub fn new(threshold_db: f32, ratio: f32) -> Self {
        let mut c = Self {
            threshold_db,
            ratio,
            attack_ms: 5.0,
            release_ms: 120.0,
            knee_db: 6.0,
            makeup_db: 0.0,
            sample_rate: 48_000,
            attack: 0.0,
            release: 0.0,
            reduction_db: 0.0,
        };
        c.update_times();
        c
    }

    /// Current gain reduction in dB (for meters).
    pub fn reduction_db(&self) -> f32 {
        self.reduction_db
    }

    fn update_times(&mut self) {
        self.attack = time_coefficient(self.attack_ms * 1e-3, self.sample_rate);
        self.release = time_coefficient(self.release_ms * 1e-3, self.sample_rate);
    }

    /// Static curve: output level for input level `x` (dB).
    #[inline]
    fn curve(&self, x: f32) -> f32 {
        let t = self.threshold_db;
        let w = self.knee_db.max(0.0);
        let r = self.ratio.max(1.0);
        let over = x - t;
        if 2.0 * over < -w {
            x
        } else if w > 0.0 && 2.0 * over.abs() <= w {
            let k = over + w * 0.5;
            x + (1.0 / r - 1.0) * k * k / (2.0 * w)
        } else {
            t + over / r
        }
    }
}

impl Effect for Compressor {
    fn prepare(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        self.update_times();
        self.reset();
    }

    fn set_param(&mut self, index: u32, value: f32) {
        match index {
            Self::THRESHOLD_DB => self.threshold_db = value,
            Self::RATIO => self.ratio = value,
            Self::ATTACK_MS => self.attack_ms = value,
            Self::RELEASE_MS => self.release_ms = value,
            Self::KNEE_DB => self.knee_db = value,
            Self::MAKEUP_DB => self.makeup_db = value,
            _ => return,
        }
        self.update_times();
    }

    fn process(&mut self, block: Block<'_>) {
        let (attack, release) = (self.attack, self.release);
        let mut y = self.reduction_db;
        let Block {
            left,
            right,
            sidechain,
        } = block;
        for i in 0..left.len() {
            let level = match sidechain {
                Some([a, b]) => a[i].abs().max(b[i].abs()),
                None => left[i].abs().max(right[i].abs()),
            };
            let x = gain_to_db(level);
            let target = x - self.curve(x);
            let a = if target > y { attack } else { release };
            y = a * y + (1.0 - a) * target;
            let g = db_to_gain(self.makeup_db - y);
            left[i] *= g;
            right[i] *= g;
        }
        flush_denormal(&mut y);
        self.reduction_db = y;
    }

    fn reset(&mut self) {
        self.reduction_db = 0.0;
    }
}

/// A lookahead peak limiter: the output never exceeds the ceiling, with no clipping.
///
/// The gain needed per frame goes through a sliding minimum over the lookahead window, an
/// instant-attack / exponential-release smoother, and a moving average over the same
/// window. Every value in that average is at most the gain needed by the frame leaving the
/// delay line, so the smoothed gain never lets a peak through while ramping down over the
/// whole lookahead (no clicks). Latency: the lookahead.
///
/// Parameters: [`Limiter::CEILING_DB`], [`Limiter::RELEASE_MS`].
#[derive(Debug, Clone)]
pub struct Limiter {
    ceiling_db: f32,
    release_ms: f32,
    lookahead_ms: f32,
    sample_rate: u32,
    ceiling: f32,
    release: f32,
    /// Lookahead in frames (the delay).
    delay: usize,
    /// Delayed input, `delay` frames per channel, ring.
    lines: [Box<[f32]>; 2],
    /// Sliding minimum: ring of (frame, gain) with increasing gains.
    min_frames: Box<[u64]>,
    min_gains: Box<[f32]>,
    min_head: usize,
    min_len: usize,
    /// Moving average of the smoothed gain over `delay + 1` frames.
    window: Box<[f32]>,
    sum: f64,
    smoothed: f32,
    frame: u64,
}

impl Limiter {
    /// Parameter index: ceiling in dBFS.
    pub const CEILING_DB: u32 = 0;
    /// Parameter index: release time in milliseconds.
    pub const RELEASE_MS: u32 = 1;

    /// A limiter with `ceiling_db` (for example -1 dBFS), 1.5 ms lookahead and 60 ms release.
    pub fn new(ceiling_db: f32) -> Self {
        Self::with_lookahead(ceiling_db, 1.5)
    }

    /// Like [`Limiter::new`] with a lookahead in milliseconds (fixed after creation, since it
    /// is the effect's latency).
    pub fn with_lookahead(ceiling_db: f32, lookahead_ms: f32) -> Self {
        let mut l = Self {
            ceiling_db,
            release_ms: 60.0,
            lookahead_ms,
            sample_rate: 0,
            ceiling: 1.0,
            release: 0.0,
            delay: 0,
            lines: [Box::new([]), Box::new([])],
            min_frames: Box::new([]),
            min_gains: Box::new([]),
            min_head: 0,
            min_len: 0,
            window: Box::new([]),
            sum: 0.0,
            smoothed: 1.0,
            frame: 0,
        };
        l.prepare(48_000);
        l
    }

    fn update_params(&mut self) {
        self.ceiling = db_to_gain(self.ceiling_db);
        self.release = time_coefficient(self.release_ms * 1e-3, self.sample_rate);
    }
}

impl Effect for Limiter {
    fn prepare(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        self.delay = ((self.lookahead_ms * 1e-3 * self.sample_rate as f32).round() as usize).max(1);
        let window = self.delay + 1;
        self.lines = [
            vec![0.0; self.delay].into_boxed_slice(),
            vec![0.0; self.delay].into_boxed_slice(),
        ];
        self.min_frames = vec![0; window].into_boxed_slice();
        self.min_gains = vec![1.0; window].into_boxed_slice();
        self.window = vec![1.0; window].into_boxed_slice();
        self.update_params();
        self.reset();
    }

    fn set_param(&mut self, index: u32, value: f32) {
        match index {
            Self::CEILING_DB => self.ceiling_db = value,
            Self::RELEASE_MS => self.release_ms = value,
            _ => return,
        }
        self.update_params();
    }

    fn process(&mut self, block: Block<'_>) {
        let window = self.delay + 1;
        let cap = self.min_gains.len();
        let ceiling = self.ceiling;
        let release = self.release;
        let inv_window = 1.0 / window as f64;
        for i in 0..block.left.len() {
            let (l, r) = (block.left[i], block.right[i]);
            let peak = l.abs().max(r.abs());
            let need = if peak > ceiling { ceiling / peak } else { 1.0 };
            let t = self.frame;

            // Sliding minimum over the last `window` frames (monotonic deque).
            while self.min_len > 0 {
                let back = (self.min_head + self.min_len - 1) % cap;
                if self.min_gains[back] >= need {
                    self.min_len -= 1;
                } else {
                    break;
                }
            }
            let slot = (self.min_head + self.min_len) % cap;
            self.min_frames[slot] = t;
            self.min_gains[slot] = need;
            self.min_len += 1;
            if t >= window as u64 && self.min_frames[self.min_head] <= t - window as u64 {
                self.min_head = (self.min_head + 1) % cap;
                self.min_len -= 1;
            }
            let m = self.min_gains[self.min_head];

            // Instant attack, exponential release; never above `m`.
            self.smoothed = if m < self.smoothed {
                m
            } else {
                m + release * (self.smoothed - m)
            };

            // Moving average over `window` frames.
            let w = (t % window as u64) as usize;
            self.sum += f64::from(self.smoothed) - f64::from(self.window[w]);
            self.window[w] = self.smoothed;
            let g = (self.sum * inv_window) as f32;

            // Output the frame from `delay` frames ago.
            let d = (t % self.delay as u64) as usize;
            let out_l = self.lines[0][d] * g;
            let out_r = self.lines[1][d] * g;
            self.lines[0][d] = l;
            self.lines[1][d] = r;
            // Rounding in the running sum can leave the gain a hair too high.
            block.left[i] = out_l.clamp(-ceiling, ceiling);
            block.right[i] = out_r.clamp(-ceiling, ceiling);
            self.frame = t + 1;
        }
        // Resynchronize the running sum now and then so rounding cannot drift.
        if self.frame % 65_536 < block.left.len() as u64 {
            self.sum = self.window.iter().map(|&x| f64::from(x)).sum();
        }
    }

    fn reset(&mut self) {
        for line in &mut self.lines {
            line.fill(0.0);
        }
        self.window.fill(1.0);
        self.sum = self.window.len() as f64;
        self.min_head = 0;
        self.min_len = 0;
        self.smoothed = 1.0;
        self.frame = 0;
    }

    fn latency(&self) -> u32 {
        self.delay as u32
    }
}
