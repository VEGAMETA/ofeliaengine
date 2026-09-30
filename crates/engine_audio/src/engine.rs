//! The game-thread side: [`Audio`].

use crate::clip::Clip;
use crate::command::{BusChange, Command, NO_BUS, Return, VoiceChange, VoiceStart};
use crate::dsp::Effect;
use crate::graph::{BusNode, EffectNode, Graph, SendNode};
use crate::mixer::{Renderer, RendererParts};
use crate::shared::Shared;
use crate::spatial::{Emitter, Listener};
use core::fmt;
use engine_core::math::Vec3;
use engine_core::spsc::{self, Consumer, Producer};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Capacities, fixed at creation so the audio thread never grows anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    /// Logical voices (playing sounds, mixed or virtual). Default 1024.
    pub max_voices: u32,
    /// Voices mixed at once; the rest are virtual (their position advances, nothing is
    /// mixed). Default 128.
    pub max_real_voices: u32,
    /// Buses including the master. Default 64.
    pub max_buses: u32,
    /// Sends. Default 128.
    pub max_sends: u32,
    /// Effects on all buses together. Default 128.
    pub max_effects: u32,
    /// Commands queued between two render calls before they spill into a game-side
    /// overflow queue. Default 2048.
    pub command_capacity: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            max_voices: 1024,
            max_real_voices: 128,
            max_buses: 64,
            max_sends: 128,
            max_effects: 128,
            command_capacity: 2048,
        }
    }
}

/// A playing sound. Stays valid until the sound ends; afterwards every call with it is
/// ignored and [`Audio::is_playing`] returns `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VoiceHandle {
    index: u32,
    generation: u32,
}

/// A bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BusId(u16);

impl BusId {
    /// The master bus: every other bus ends up here, and it feeds the output.
    pub const MASTER: Self = Self(0);

    /// Index of the bus (for meters and tables).
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// A send from one bus to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SendId {
    index: u32,
    generation: u32,
}

/// An effect on a bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EffectId {
    index: u32,
    generation: u32,
}

/// Where a send taps its bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SendTap {
    /// After the bus's effects, before its fader: the send ignores the bus's volume.
    PreFader,
    /// After the fader (the usual choice for reverb sends).
    #[default]
    PostFader,
}

/// How to play a clip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayParams {
    /// Bus to play on.
    pub bus: BusId,
    /// Linear gain.
    pub gain: f32,
    /// -1 (left) .. 1 (right). Mono clips pan with constant power (-3 dB each side at the
    /// centre); stereo clips balance.
    pub pan: f32,
    /// Playback rate ratio: 2 is an octave up and twice as fast.
    pub pitch: f32,
    /// Loop until stopped.
    pub looping: bool,
    /// Higher wins when voices compete for the real-voice limits. Default 128.
    pub priority: u8,
    /// Mixer frame to start at ([`Audio::clock`]), for sample-accurate timing. A time
    /// already past when the command arrives starts at once, skipping ahead so the sound
    /// stays in time. `None` starts with the next rendered block.
    pub start_at: Option<u64>,
    /// Fade-in in seconds.
    pub fade_in: f32,
    /// Place the sound in the world: gain, pan, pitch and a low-pass then follow the
    /// emitter as heard by the [`Listener`] ([`crate::spatial`]). `gain`, `pan` and `pitch`
    /// still apply on top. `None` plays it as is (music, UI).
    pub spatial: Option<Emitter>,
}

impl Default for PlayParams {
    fn default() -> Self {
        Self {
            bus: BusId::MASTER,
            gain: 1.0,
            pan: 0.0,
            pitch: 1.0,
            looping: false,
            priority: 128,
            start_at: None,
            fade_in: 0.0,
            spatial: None,
        }
    }
}

/// The mixer clock as last published by the audio thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioClock {
    /// Frames rendered so far.
    pub frames: u64,
    /// Output sample rate in Hz.
    pub sample_rate: u32,
}

impl AudioClock {
    /// The clock in seconds.
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate.max(1))
    }

    /// The frame `seconds` after this clock reading.
    pub fn frames_after(&self, seconds: f64) -> u64 {
        self.frames + (seconds.max(0.0) * f64::from(self.sample_rate)).round() as u64
    }
}

/// Errors from [`Audio`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// All voices are in use ([`AudioConfig::max_voices`]).
    VoicesExhausted,
    /// All bus, send or effect slots are in use.
    TooMany(&'static str),
    /// The bus, send or effect does not exist (or was removed).
    Unknown(&'static str),
    /// The master bus cannot be removed or given a parent.
    Master,
    /// The change would make the graph cyclic (a bus feeding itself through parents,
    /// sends or sidechains). The graph is left as it was.
    Cycle,
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VoicesExhausted => f.write_str("all voices are in use"),
            Self::TooMany(what) => write!(f, "no free {what} slot"),
            Self::Unknown(what) => write!(f, "unknown {what}"),
            Self::Master => f.write_str("the master bus cannot be removed or re-parented"),
            Self::Cycle => f.write_str("the change would make the bus graph cyclic"),
        }
    }
}

impl std::error::Error for AudioError {}

#[derive(Clone, Copy, Default)]
struct VoiceSlot {
    generation: u32,
    playing: bool,
}

/// The game-thread half of the mixer: plays sounds and edits buses, sends and effects.
/// Every call returns immediately; the audio thread applies the changes at its next block.
/// Call [`Audio::update`] once per frame to reclaim finished voices.
pub struct Audio {
    commands: Producer<Command>,
    /// Commands that did not fit in the ring, in order; sent before anything newer.
    overflow: VecDeque<Command>,
    returns: Consumer<Return>,
    voices: Vec<VoiceSlot>,
    free_voices: Vec<u32>,
    graph: Graph,
    send_generations: Vec<u32>,
    effect_generations: Vec<u32>,
    bus_limit: usize,
    send_limit: usize,
    effect_limit: usize,
    shared: Arc<Shared>,
}

impl Audio {
    /// Creates the mixer: the game side and the [`Renderer`] for the audio thread.
    pub fn new(config: &AudioConfig) -> (Self, Renderer) {
        let max_voices = config.max_voices.clamp(1, 1 << 24) as usize;
        let max_buses = config.max_buses.clamp(1, u32::from(NO_BUS)) as usize;
        let max_sends = config.max_sends as usize;
        let max_effects = config.max_effects as usize;
        let (commands, command_rx) = spsc::channel(config.command_capacity.max(16) as usize);
        // Every returned object is a finished voice, a replaced schedule or a removed
        // effect; room for all of them at once plus schedules means the renderer never
        // has to hold on to one.
        let (returns_tx, returns) = spsc::channel(max_voices + max_effects + 256);
        let shared = Arc::new(Shared::new(max_buses));
        let graph = Graph::new();
        let schedule = Box::new(graph.compile().unwrap_or_default());
        let renderer = Renderer::new(RendererParts {
            commands: command_rx,
            returns: returns_tx,
            shared: shared.clone(),
            max_voices,
            max_real: config.max_real_voices.max(1),
            max_buses,
            max_sends,
            max_effects,
            schedule,
        });
        let audio = Self {
            commands,
            overflow: VecDeque::with_capacity(256),
            returns,
            voices: vec![VoiceSlot::default(); max_voices],
            free_voices: (0..max_voices as u32).rev().collect(),
            graph,
            send_generations: Vec::new(),
            effect_generations: Vec::new(),
            bus_limit: max_buses,
            send_limit: max_sends,
            effect_limit: max_effects,
            shared,
        };
        (audio, renderer)
    }

    fn send(&mut self, command: Command) {
        if self.overflow.is_empty() {
            if let Err(command) = self.commands.push(command) {
                self.overflow.push_back(command);
            }
        } else {
            self.overflow.push_back(command);
        }
    }

    /// Reclaims finished voices and frees what the audio thread handed back; sends
    /// commands that overflowed the ring. Call once per frame.
    pub fn update(&mut self) {
        while let Some(r) = self.returns.pop() {
            if let Return::VoiceEnded { slot, .. } = r
                && let Some(v) = self.voices.get_mut(slot as usize)
            {
                v.playing = false;
                v.generation = v.generation.wrapping_add(1);
                self.free_voices.push(slot);
            }
            // Clips, schedules and effects are dropped here, on the game thread.
        }
        while let Some(command) = self.overflow.pop_front() {
            if let Err(command) = self.commands.push(command) {
                self.overflow.push_front(command);
                break;
            }
        }
    }

    /// Plays `clip` on the master bus.
    pub fn play(&mut self, clip: &Clip) -> Result<VoiceHandle, AudioError> {
        self.play_with(clip, &PlayParams::default())
    }

    /// Plays `clip` with `params`.
    pub fn play_with(
        &mut self,
        clip: &Clip,
        params: &PlayParams,
    ) -> Result<VoiceHandle, AudioError> {
        let bus = if self.graph.bus(params.bus.0).is_some() {
            params.bus.0
        } else {
            return Err(AudioError::Unknown("bus"));
        };
        let index = self.free_voices.pop().ok_or(AudioError::VoicesExhausted)?;
        let slot = &mut self.voices[index as usize];
        slot.playing = true;
        let handle = VoiceHandle {
            index,
            generation: slot.generation,
        };
        self.send(Command::Play {
            slot: index,
            clip: clip.clone(),
            start: VoiceStart {
                bus,
                gain: params.gain,
                pan: params.pan,
                pitch: params.pitch,
                looping: params.looping,
                priority: params.priority,
                start_at: params.start_at,
                fade_in: params.fade_in,
                spatial: params.spatial,
            },
        });
        Ok(handle)
    }

    /// Whether the voice is still playing (until the audio thread reports its end and
    /// [`Audio::update`] has run).
    pub fn is_playing(&self, voice: VoiceHandle) -> bool {
        self.voices
            .get(voice.index as usize)
            .is_some_and(|v| v.playing && v.generation == voice.generation)
    }

    fn voice_command(&mut self, voice: VoiceHandle, change: VoiceChange) {
        if self.is_playing(voice) {
            self.send(Command::Voice {
                slot: voice.index,
                change,
            });
        }
    }

    /// Fades the voice out over `fade` seconds (at least about 1 ms, so it never clicks),
    /// then ends it.
    pub fn stop(&mut self, voice: VoiceHandle, fade: f32) {
        self.voice_command(voice, VoiceChange::Stop(fade));
    }

    /// Changes the voice's gain over `ramp` seconds.
    pub fn set_voice_gain(&mut self, voice: VoiceHandle, gain: f32, ramp: f32) {
        self.voice_command(voice, VoiceChange::Gain(gain, ramp));
    }

    /// Changes the voice's pan over `ramp` seconds.
    pub fn set_voice_pan(&mut self, voice: VoiceHandle, pan: f32, ramp: f32) {
        self.voice_command(voice, VoiceChange::Pan(pan, ramp));
    }

    /// Changes the voice's pitch (playback rate ratio).
    pub fn set_voice_pitch(&mut self, voice: VoiceHandle, pitch: f32) {
        self.voice_command(voice, VoiceChange::Pitch(pitch));
    }

    /// Moves a spatial voice (one played with [`PlayParams::spatial`]); `velocity` in m/s
    /// drives the Doppler effect. Call each frame for sounds that move.
    pub fn set_voice_position(&mut self, voice: VoiceHandle, position: Vec3, velocity: Vec3) {
        self.voice_command(voice, VoiceChange::Position(position, velocity));
    }

    /// How much is between a spatial voice and the listener, 0 (nothing) .. 1 (a wall):
    /// full occlusion is 15 dB quieter and muffled to about 600 Hz. Usually from a ray
    /// cast, a few times per second.
    pub fn set_voice_occlusion(&mut self, voice: VoiceHandle, occlusion: f32) {
        self.voice_command(voice, VoiceChange::Occlusion(occlusion));
    }

    /// Where spatial voices are heard from. Call once per frame (usually from the camera).
    pub fn set_listener(&mut self, listener: &Listener) {
        self.send(Command::Listener(*listener));
    }

    fn compile_and_send(&mut self) -> bool {
        match self.graph.compile() {
            Some(schedule) => {
                self.send(Command::Schedule(Box::new(schedule)));
                true
            }
            None => false,
        }
    }

    /// Adds a bus feeding `parent`.
    pub fn add_bus(&mut self, name: &str, parent: BusId) -> Result<BusId, AudioError> {
        if self.graph.bus(parent.0).is_none() {
            return Err(AudioError::Unknown("bus"));
        }
        let node = BusNode {
            name: name.to_owned(),
            parent: parent.0,
            effects: Vec::new(),
        };
        let index = match self.graph.buses.iter().position(Option::is_none) {
            Some(i) => {
                self.graph.buses[i] = Some(node);
                i
            }
            None if self.graph.buses.len() < self.bus_limit => {
                self.graph.buses.push(Some(node));
                self.graph.buses.len() - 1
            }
            None => return Err(AudioError::TooMany("bus")),
        };
        let bus = index as u16;
        // A reused slot starts with default fader settings.
        self.send(Command::Bus {
            bus,
            change: BusChange::Gain(1.0, 0.0),
        });
        self.send(Command::Bus {
            bus,
            change: BusChange::Pan(0.0, 0.0),
        });
        self.send(Command::Bus {
            bus,
            change: BusChange::Mute(false),
        });
        self.send(Command::Bus {
            bus,
            change: BusChange::VoiceLimit(u32::MAX),
        });
        self.compile_and_send();
        Ok(BusId(bus))
    }

    /// Removes a bus with its effects and sends. Its children move to its parent; voices
    /// still playing on it continue on the master.
    pub fn remove_bus(&mut self, bus: BusId) -> Result<(), AudioError> {
        if bus == BusId::MASTER {
            return Err(AudioError::Master);
        }
        let node = self
            .graph
            .buses
            .get_mut(usize::from(bus.0))
            .and_then(Option::take)
            .ok_or(AudioError::Unknown("bus"))?;
        for child in self.graph.buses.iter_mut().flatten() {
            if child.parent == bus.0 {
                child.parent = node.parent;
            }
        }
        for &slot in &node.effects {
            self.graph.effects[slot as usize] = None;
            self.effect_generations[slot as usize] += 1;
            self.send(Command::RemoveEffect { slot });
        }
        for (i, send) in self.graph.sends.iter_mut().enumerate() {
            if send
                .as_ref()
                .is_some_and(|s| s.from == bus.0 || s.to == bus.0)
            {
                *send = None;
                self.send_generations[i] += 1;
            }
        }
        for effect in self.graph.effects.iter_mut().flatten() {
            if effect.sidechain == bus.0 {
                effect.sidechain = NO_BUS;
            }
        }
        self.compile_and_send();
        Ok(())
    }

    /// Moves `bus` under `parent`.
    pub fn set_bus_parent(&mut self, bus: BusId, parent: BusId) -> Result<(), AudioError> {
        if bus == BusId::MASTER {
            return Err(AudioError::Master);
        }
        if self.graph.bus(parent.0).is_none() {
            return Err(AudioError::Unknown("bus"));
        }
        let node = self
            .graph
            .buses
            .get_mut(usize::from(bus.0))
            .and_then(Option::as_mut)
            .ok_or(AudioError::Unknown("bus"))?;
        let old = core::mem::replace(&mut node.parent, parent.0);
        if !self.compile_and_send() {
            if let Some(node) = self.graph.buses[usize::from(bus.0)].as_mut() {
                node.parent = old;
            }
            return Err(AudioError::Cycle);
        }
        Ok(())
    }

    /// The bus called `name`.
    pub fn bus_by_name(&self, name: &str) -> Option<BusId> {
        self.graph
            .buses
            .iter()
            .position(|b| b.as_ref().is_some_and(|b| b.name == name))
            .map(|i| BusId(i as u16))
    }

    /// The bus's name.
    pub fn bus_name(&self, bus: BusId) -> Option<&str> {
        self.graph.bus(bus.0).map(|b| b.name.as_str())
    }

    fn bus_command(&mut self, bus: BusId, change: BusChange) {
        if self.graph.bus(bus.0).is_some() {
            self.send(Command::Bus { bus: bus.0, change });
        }
    }

    /// Changes the bus's volume over `ramp` seconds.
    pub fn set_bus_gain(&mut self, bus: BusId, gain: f32, ramp: f32) {
        self.bus_command(bus, BusChange::Gain(gain, ramp));
    }

    /// Changes the bus's balance (-1..1) over `ramp` seconds.
    pub fn set_bus_pan(&mut self, bus: BusId, pan: f32, ramp: f32) {
        self.bus_command(bus, BusChange::Pan(pan, ramp));
    }

    /// Mutes or unmutes the bus. Voices on a muted bus become virtual (not mixed).
    pub fn set_bus_mute(&mut self, bus: BusId, muted: bool) {
        self.bus_command(bus, BusChange::Mute(muted));
    }

    /// Limits the voices mixed on this bus at once; the least important become virtual.
    pub fn set_bus_voice_limit(&mut self, bus: BusId, limit: Option<u32>) {
        self.bus_command(bus, BusChange::VoiceLimit(limit.unwrap_or(u32::MAX)));
    }

    /// Adds a send from `from` to `to` with `gain`.
    pub fn add_send(
        &mut self,
        from: BusId,
        to: BusId,
        gain: f32,
        tap: SendTap,
    ) -> Result<SendId, AudioError> {
        if self.graph.bus(from.0).is_none() || self.graph.bus(to.0).is_none() {
            return Err(AudioError::Unknown("bus"));
        }
        if from == to {
            return Err(AudioError::Cycle);
        }
        let node = SendNode {
            from: from.0,
            to: to.0,
            pre_fader: tap == SendTap::PreFader,
        };
        let index = match self.graph.sends.iter().position(Option::is_none) {
            Some(i) => i,
            None if self.graph.sends.len() < self.send_limit => {
                self.graph.sends.push(None);
                self.send_generations.push(0);
                self.graph.sends.len() - 1
            }
            None => return Err(AudioError::TooMany("send")),
        };
        self.graph.sends[index] = Some(node);
        if !self.compile_and_send() {
            self.graph.sends[index] = None;
            return Err(AudioError::Cycle);
        }
        self.send(Command::SendGain {
            send: index as u32,
            gain,
            ramp: 0.0,
        });
        Ok(SendId {
            index: index as u32,
            generation: self.send_generations[index],
        })
    }

    fn send_node(&self, send: SendId) -> Option<&SendNode> {
        (self.send_generations.get(send.index as usize) == Some(&send.generation))
            .then(|| self.graph.sends[send.index as usize].as_ref())
            .flatten()
    }

    /// Changes a send's gain over `ramp` seconds.
    pub fn set_send_gain(&mut self, send: SendId, gain: f32, ramp: f32) {
        if self.send_node(send).is_some() {
            self.send(Command::SendGain {
                send: send.index,
                gain,
                ramp,
            });
        }
    }

    /// Removes a send.
    pub fn remove_send(&mut self, send: SendId) -> Result<(), AudioError> {
        if self.send_node(send).is_none() {
            return Err(AudioError::Unknown("send"));
        }
        self.graph.sends[send.index as usize] = None;
        self.send_generations[send.index as usize] += 1;
        self.compile_and_send();
        Ok(())
    }

    /// Appends `effect` to the end of the bus's effect chain.
    pub fn add_effect<E: Effect>(&mut self, bus: BusId, effect: E) -> Result<EffectId, AudioError> {
        self.add_boxed_effect(bus, Box::new(effect))
    }

    /// Like [`Audio::add_effect`] for an effect that is already boxed (plugins, editor).
    pub fn add_boxed_effect(
        &mut self,
        bus: BusId,
        mut effect: Box<dyn Effect>,
    ) -> Result<EffectId, AudioError> {
        if self.graph.bus(bus.0).is_none() {
            return Err(AudioError::Unknown("bus"));
        }
        let index = match self.graph.effects.iter().position(Option::is_none) {
            Some(i) => i,
            None if self.graph.effects.len() < self.effect_limit => {
                self.graph.effects.push(None);
                self.effect_generations.push(0);
                self.graph.effects.len() - 1
            }
            None => return Err(AudioError::TooMany("effect")),
        };
        let sample_rate = self.shared.sample_rate.load(Ordering::Relaxed);
        effect.prepare(sample_rate);
        self.graph.effects[index] = Some(EffectNode {
            bus: bus.0,
            sidechain: NO_BUS,
        });
        if let Some(node) = self.graph.buses[usize::from(bus.0)].as_mut() {
            node.effects.push(index as u32);
        }
        // The effect must exist on the audio side before a schedule refers to it.
        self.send(Command::AddEffect {
            slot: index as u32,
            effect,
            sample_rate,
        });
        self.compile_and_send();
        Ok(EffectId {
            index: index as u32,
            generation: self.effect_generations[index],
        })
    }

    fn effect_node(&self, effect: EffectId) -> Option<&EffectNode> {
        (self.effect_generations.get(effect.index as usize) == Some(&effect.generation))
            .then(|| self.graph.effects[effect.index as usize].as_ref())
            .flatten()
    }

    /// Removes an effect; it is freed on the game thread after the audio thread lets go.
    pub fn remove_effect(&mut self, effect: EffectId) -> Result<(), AudioError> {
        let bus = self
            .effect_node(effect)
            .ok_or(AudioError::Unknown("effect"))?
            .bus;
        self.graph.effects[effect.index as usize] = None;
        self.effect_generations[effect.index as usize] += 1;
        if let Some(node) = self.graph.buses[usize::from(bus)].as_mut() {
            node.effects.retain(|&e| e != effect.index);
        }
        // Stop referring to it before it goes away.
        self.compile_and_send();
        self.send(Command::RemoveEffect { slot: effect.index });
        Ok(())
    }

    /// Sets an effect parameter (indices are the effect's associated constants).
    pub fn set_effect_param(&mut self, effect: EffectId, index: u32, value: f32) {
        if self.effect_node(effect).is_some() {
            self.send(Command::EffectParam {
                slot: effect.index,
                index,
                value,
            });
        }
    }

    /// Keys the effect by another bus's signal (for ducking), or by its own input with
    /// `None`.
    pub fn set_sidechain(
        &mut self,
        effect: EffectId,
        source: Option<BusId>,
    ) -> Result<(), AudioError> {
        if self.effect_node(effect).is_none() {
            return Err(AudioError::Unknown("effect"));
        }
        let source = match source {
            Some(bus) if self.graph.bus(bus.0).is_none() => {
                return Err(AudioError::Unknown("bus"));
            }
            Some(bus) => bus.0,
            None => NO_BUS,
        };
        let node = self.graph.effects[effect.index as usize]
            .as_mut()
            .ok_or(AudioError::Unknown("effect"))?;
        let old = core::mem::replace(&mut node.sidechain, source);
        if !self.compile_and_send() {
            if let Some(node) = self.graph.effects[effect.index as usize].as_mut() {
                node.sidechain = old;
            }
            return Err(AudioError::Cycle);
        }
        Ok(())
    }

    /// The mixer clock as of the audio thread's last block.
    pub fn clock(&self) -> AudioClock {
        AudioClock {
            frames: self.shared.frames.load(Ordering::Relaxed),
            sample_rate: self.shared.sample_rate.load(Ordering::Relaxed),
        }
    }

    /// Peak level of the bus per channel since the last call (linear, after the fader).
    pub fn take_peak(&self, bus: BusId) -> [f32; 2] {
        [
            self.shared.take_peak(bus.index(), 0),
            self.shared.take_peak(bus.index(), 1),
        ]
    }

    /// `(mixed, virtual)` voices at the audio thread's last render call.
    pub fn voice_counts(&self) -> (u32, u32) {
        (
            self.shared.real_voices.load(Ordering::Relaxed),
            self.shared.virtual_voices.load(Ordering::Relaxed),
        )
    }
}
