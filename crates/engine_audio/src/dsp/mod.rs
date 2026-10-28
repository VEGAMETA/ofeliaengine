//! Effects that run on buses: the [`Effect`] trait and the built-in effects.
//!
//! An effect is created and [prepared](Effect::prepare) on the game thread, then moved to
//! the audio thread, where only [`Effect::set_param`] and [`Effect::process`] are called.
//! Those two must not allocate, lock, or block. Parameters are addressed by index (like
//! VST3 parameters), so the game side changes them with one small command and no dynamic
//! dispatch on its side.

mod biquad;
mod delay;
mod dynamics;
mod info;
mod reverb;

pub use biquad::{Biquad, FilterKind};
pub use delay::Delay;
pub use dynamics::{Compressor, Limiter};
pub use info::{EFFECTS, EffectInfo, ParamInfo, Unit, effect_info};
pub use reverb::Reverb;

/// One sub-block of a bus's stereo signal, processed in place.
pub struct Block<'a> {
    /// Left channel.
    pub left: &'a mut [f32],
    /// Right channel (same length as `left`).
    pub right: &'a mut [f32],
    /// Another bus's signal from the same sub-block, when the effect has a sidechain
    /// (for example a compressor on music keyed by dialogue).
    pub sidechain: Option<[&'a [f32]; 2]>,
}

/// An effect on a bus.
pub trait Effect: Send + 'static {
    /// Sets up for `sample_rate`: sizes buffers and clears state. Called on the game thread
    /// before the effect is first used, and again on the audio thread only if the output's
    /// sample rate changes (a device switch), so it may allocate.
    fn prepare(&mut self, sample_rate: u32);

    /// Sets parameter `index` (each effect documents its indices as associated constants).
    /// Unknown indices are ignored. Must not allocate.
    fn set_param(&mut self, index: u32, value: f32);

    /// Processes one sub-block (at most [`crate::MAX_BLOCK`] frames) in place. Must not
    /// allocate, lock or block.
    fn process(&mut self, block: Block<'_>);

    /// Clears internal state (tails, envelopes) without changing parameters.
    fn reset(&mut self) {}

    /// Delay the effect adds, in frames (for delay compensation).
    fn latency(&self) -> u32 {
        0
    }

    /// How far it lowers the gain right now, in dB (0 or more): a compressor's or a
    /// limiter's gain reduction, read after each sub-block for the meters
    /// ([`crate::Audio::take_reduction`]). Must not allocate.
    fn reduction_db(&self) -> f32 {
        0.0
    }
}

/// Decibels to linear gain.
#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    // 10^(db / 20) = 2^(db * log2(10) / 20)
    (db * 0.166_096_4).exp2()
}

/// Linear gain to decibels (-200 dB for zero).
#[inline]
pub fn gain_to_db(gain: f32) -> f32 {
    // 20 * log10(g) = 20 * log10(2) * log2(g)
    6.020_6 * gain.max(1e-10).log2()
}

/// One-pole smoothing coefficient for a time constant in seconds.
#[inline]
pub(crate) fn time_coefficient(seconds: f32, sample_rate: u32) -> f32 {
    if seconds <= 0.0 {
        0.0
    } else {
        (-1.0 / (seconds * sample_rate as f32)).exp()
    }
}

/// Values this small in a recursive state decay into denormals, which are slow on x86.
/// Flushing once per block is cheap and deterministic (unlike changing MXCSR).
#[inline]
pub(crate) fn flush_denormal(x: &mut f32) {
    if x.abs() < 1e-20 {
        *x = 0.0;
    }
}
