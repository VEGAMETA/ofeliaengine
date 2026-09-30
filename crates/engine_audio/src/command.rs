//! Messages between the game thread ([`crate::Audio`]) and the audio thread
//! ([`crate::Renderer`]).

use crate::clip::Clip;
use crate::dsp::Effect;
use crate::graph::Schedule;
use crate::spatial::{Emitter, Listener};
use engine_core::math::Vec3;

/// Sentinel for "no bus".
pub(crate) const NO_BUS: u16 = u16::MAX;

/// How a voice starts.
#[derive(Clone, Copy)]
pub(crate) struct VoiceStart {
    pub(crate) bus: u16,
    pub(crate) gain: f32,
    pub(crate) pan: f32,
    pub(crate) pitch: f32,
    pub(crate) looping: bool,
    pub(crate) priority: u8,
    /// Mixer frame to start at; `None` starts at the next block.
    pub(crate) start_at: Option<u64>,
    /// Fade-in in seconds.
    pub(crate) fade_in: f32,
    /// Placed in the world.
    pub(crate) spatial: Option<Emitter>,
}

#[derive(Clone, Copy)]
pub(crate) enum VoiceChange {
    /// Gain and ramp seconds.
    Gain(f32, f32),
    /// Pan (-1..1) and ramp seconds.
    Pan(f32, f32),
    /// Pitch ratio.
    Pitch(f32),
    /// Fade-out seconds, then the voice ends.
    Stop(f32),
    /// A spatial voice's position and velocity.
    Position(Vec3, Vec3),
    /// A spatial voice's occlusion (0..1).
    Occlusion(f32),
}

#[derive(Clone, Copy)]
pub(crate) enum BusChange {
    /// Gain and ramp seconds.
    Gain(f32, f32),
    /// Balance (-1..1) and ramp seconds.
    Pan(f32, f32),
    Mute(bool),
    /// Maximum real (mixed) voices on the bus; `u32::MAX` for no limit.
    VoiceLimit(u32),
}

/// Game thread → audio thread.
pub(crate) enum Command {
    Play {
        slot: u32,
        clip: Clip,
        start: VoiceStart,
    },
    Voice {
        slot: u32,
        change: VoiceChange,
    },
    Bus {
        bus: u16,
        change: BusChange,
    },
    /// Send gain and ramp seconds.
    SendGain {
        send: u32,
        gain: f32,
        ramp: f32,
    },
    /// A new effect in `slot`, prepared for `sample_rate`.
    AddEffect {
        slot: u32,
        effect: Box<dyn Effect>,
        sample_rate: u32,
    },
    RemoveEffect {
        slot: u32,
    },
    EffectParam {
        slot: u32,
        index: u32,
        value: f32,
    },
    /// The compiled bus graph; replaces the current one.
    Schedule(Box<Schedule>),
    /// Where spatial voices are heard from.
    Listener(Listener),
}

/// Audio thread → game thread: finished voices and objects to free.
pub(crate) enum Return {
    VoiceEnded {
        slot: u32,
        // Dropped on the game thread, which may free the clip's memory.
        #[allow(dead_code)]
        clip: Clip,
    },
    #[allow(dead_code)]
    Schedule(Box<Schedule>),
    #[allow(dead_code)]
    Effect(Box<dyn Effect>),
}
