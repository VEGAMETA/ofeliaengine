//! Second-order IIR filter with the coefficient formulas from Robert Bristow-Johnson's
//! "Audio EQ Cookbook" (public), in transposed direct form II.

use super::{Block, Effect, db_to_gain, flush_denormal};

/// Filter response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    /// Passes below the frequency.
    LowPass,
    /// Passes above the frequency.
    HighPass,
    /// Passes around the frequency (0 dB peak).
    BandPass,
    /// Removes the frequency.
    Notch,
    /// Boosts or cuts around the frequency by the gain.
    Peak,
    /// Boosts or cuts below the frequency by the gain.
    LowShelf,
    /// Boosts or cuts above the frequency by the gain.
    HighShelf,
    /// Changes phase only.
    AllPass,
}

impl FilterKind {
    const ALL: [Self; 8] = [
        Self::LowPass,
        Self::HighPass,
        Self::BandPass,
        Self::Notch,
        Self::Peak,
        Self::LowShelf,
        Self::HighShelf,
        Self::AllPass,
    ];
}

/// A stereo biquad filter.
///
/// Parameters: [`Biquad::KIND`] (the [`FilterKind`] as its index), [`Biquad::FREQUENCY`]
/// (Hz), [`Biquad::Q`], [`Biquad::GAIN_DB`] (peak and shelf only). Coefficients are
/// recomputed at the start of the next block after a change.
#[derive(Debug, Clone)]
pub struct Biquad {
    kind: FilterKind,
    frequency: f32,
    q: f32,
    gain_db: f32,
    sample_rate: u32,
    dirty: bool,
    /// b0, b1, b2, a1, a2 (normalized by a0).
    c: [f32; 5],
    /// z1, z2 per channel.
    z: [[f32; 2]; 2],
}

impl Biquad {
    /// Parameter index: filter kind (`FilterKind` index as `f32`).
    pub const KIND: u32 = 0;
    /// Parameter index: frequency in Hz.
    pub const FREQUENCY: u32 = 1;
    /// Parameter index: Q (resonance / bandwidth; 0.707 is maximally flat for low/high pass).
    pub const Q: u32 = 2;
    /// Parameter index: gain in dB (peak and shelf filters).
    pub const GAIN_DB: u32 = 3;

    /// A filter of any kind.
    pub fn new(kind: FilterKind, frequency: f32, q: f32, gain_db: f32) -> Self {
        Self {
            kind,
            frequency,
            q,
            gain_db,
            sample_rate: 48_000,
            dirty: true,
            c: [1.0, 0.0, 0.0, 0.0, 0.0],
            z: [[0.0; 2]; 2],
        }
    }

    /// Low-pass at `frequency` Hz.
    pub fn low_pass(frequency: f32, q: f32) -> Self {
        Self::new(FilterKind::LowPass, frequency, q, 0.0)
    }

    /// High-pass at `frequency` Hz.
    pub fn high_pass(frequency: f32, q: f32) -> Self {
        Self::new(FilterKind::HighPass, frequency, q, 0.0)
    }

    /// Peaking EQ band.
    pub fn peak(frequency: f32, q: f32, gain_db: f32) -> Self {
        Self::new(FilterKind::Peak, frequency, q, gain_db)
    }

    fn update(&mut self) {
        self.dirty = false;
        let fs = f64::from(self.sample_rate);
        let f0 = f64::from(self.frequency).clamp(10.0, fs * 0.49);
        let q = f64::from(self.q).clamp(0.05, 50.0);
        let w0 = 2.0 * core::f64::consts::PI * f0 / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a = f64::from(db_to_gain(self.gain_db * 0.5));
        let sa = 2.0 * a.sqrt() * alpha;
        let (b0, b1, b2, a0, a1, a2) = match self.kind {
            FilterKind::LowPass => (
                (1.0 - cos) / 2.0,
                1.0 - cos,
                (1.0 - cos) / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            FilterKind::HighPass => (
                (1.0 + cos) / 2.0,
                -(1.0 + cos),
                (1.0 + cos) / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            FilterKind::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::AllPass => (
                1.0 - alpha,
                -2.0 * cos,
                1.0 + alpha,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            FilterKind::Peak => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            FilterKind::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cos + sa),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - sa),
                (a + 1.0) + (a - 1.0) * cos + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - sa,
            ),
            FilterKind::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cos + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - sa),
                (a + 1.0) - (a - 1.0) * cos + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - sa,
            ),
        };
        self.c = [
            (b0 / a0) as f32,
            (b1 / a0) as f32,
            (b2 / a0) as f32,
            (a1 / a0) as f32,
            (a2 / a0) as f32,
        ];
    }

    /// Filters one channel in place.
    #[inline]
    fn run(c: &[f32; 5], z: &mut [f32; 2], samples: &mut [f32]) {
        let [b0, b1, b2, a1, a2] = *c;
        let [mut z1, mut z2] = *z;
        for x in samples {
            let input = *x;
            let y = b0 * input + z1;
            z1 = b1 * input - a1 * y + z2;
            z2 = b2 * input - a2 * y;
            *x = y;
        }
        flush_denormal(&mut z1);
        flush_denormal(&mut z2);
        *z = [z1, z2];
    }
}

impl Effect for Biquad {
    fn prepare(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        self.dirty = true;
        self.reset();
    }

    fn set_param(&mut self, index: u32, value: f32) {
        match index {
            Self::KIND => {
                if let Some(&kind) = FilterKind::ALL.get(value.max(0.0) as usize) {
                    self.kind = kind;
                }
            }
            Self::FREQUENCY => self.frequency = value,
            Self::Q => self.q = value,
            Self::GAIN_DB => self.gain_db = value,
            _ => return,
        }
        self.dirty = true;
    }

    fn process(&mut self, block: Block<'_>) {
        if self.dirty {
            self.update();
        }
        let [zl, zr] = &mut self.z;
        Self::run(&self.c, zl, block.left);
        Self::run(&self.c, zr, block.right);
    }

    fn reset(&mut self) {
        self.z = [[0.0; 2]; 2];
    }
}
