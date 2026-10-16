//! State the audio thread publishes for the game thread: the mixer clock, voice counts and
//! bus peak meters. Plain atomics, written once per render call.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub(crate) struct Shared {
    /// Frames rendered so far (the mixer clock).
    pub(crate) frames: AtomicU64,
    /// Output sample rate (0 until an output has set it; the renderer starts at 48 kHz).
    pub(crate) sample_rate: AtomicU32,
    pub(crate) real_voices: AtomicU32,
    pub(crate) virtual_voices: AtomicU32,
    /// Blocks in which a stream voice ran out of decoded frames.
    pub(crate) stream_underruns: AtomicU64,
    /// Peak per bus and channel since the game last read it, as `f32` bits (non-negative
    /// floats order like their bits, so `fetch_max` works).
    pub(crate) peaks: Box<[AtomicU32]>,
    /// Each clip voice's frame in its clip at the end of the last render call, by slot
    /// ([`crate::Audio::position`]). The game thread writes a slot's start before the voice
    /// plays; the audio thread writes it only while the voice is active.
    pub(crate) positions: Box<[AtomicU64]>,
}

impl Shared {
    pub(crate) fn new(max_buses: usize, max_voices: usize) -> Self {
        Self {
            frames: AtomicU64::new(0),
            sample_rate: AtomicU32::new(48_000),
            real_voices: AtomicU32::new(0),
            virtual_voices: AtomicU32::new(0),
            stream_underruns: AtomicU64::new(0),
            peaks: (0..max_buses * 2).map(|_| AtomicU32::new(0)).collect(),
            positions: (0..max_voices).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    #[inline]
    pub(crate) fn raise_peak(&self, bus: usize, channel: usize, peak: f32) {
        if let Some(p) = self.peaks.get(bus * 2 + channel) {
            p.fetch_max(peak.max(0.0).to_bits(), Ordering::Relaxed);
        }
    }

    pub(crate) fn take_peak(&self, bus: usize, channel: usize) -> f32 {
        self.peaks
            .get(bus * 2 + channel)
            .map_or(0.0, |p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
    }
}
