//! Audio: a lock-free mixer with buses, sends, virtual voices and effects, and WASAPI
//! output (CLAUDE.md §11, ADR 0024).
//!
//! The game thread owns an [`Audio`]; the audio thread owns the [`Renderer`] it came
//! with. They talk through two single-producer rings: commands go to the renderer,
//! finished voices and replaced objects come back to be freed on the game thread. Neither
//! side ever waits for the other, and the renderer never allocates or frees.
//!
//! ```
//! use engine_audio::{Audio, AudioConfig, BusId, Clip, PlayParams, dsp::Biquad};
//!
//! let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
//! let music = audio.add_bus("music", BusId::MASTER).unwrap();
//! audio.add_effect(music, Biquad::low_pass(2_000.0, 0.707)).unwrap();
//!
//! let tone: Vec<f32> = (0..4_800).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
//! let clip = Clip::from_interleaved(48_000, 1, &tone).unwrap();
//! let voice = audio.play_with(&clip, &PlayParams { bus: music, ..PlayParams::default() }).unwrap();
//!
//! // On the audio thread (normally inside the output device's callback):
//! let mut out = vec![0.0f32; 256 * 2];
//! renderer.render(&mut out, 2);
//!
//! audio.update();
//! assert!(audio.is_playing(voice));
//! ```

mod clip;
mod command;
pub mod dsp;
mod engine;
mod graph;
mod mixer;
#[cfg(windows)]
pub mod output;
mod shared;
pub mod spatial;
pub mod stream;
mod wav;

pub use clip::{Clip, ClipError};
pub use engine::{
    Audio, AudioClock, AudioConfig, AudioError, BusId, EffectId, PlayParams, SendId, SendTap,
    VoiceHandle,
};
pub use mixer::{MAX_BLOCK, Renderer};
pub use spatial::{Emitter, Listener, Rolloff};
pub use stream::{DecodeError, Decoder, PreparedStream};

#[cfg(test)]
mod tests;
