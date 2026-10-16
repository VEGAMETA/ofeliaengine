//! Sound in the player (ADR 0105): what entities play, heard from the listener, through
//! the world.
//!
//! - **[`AudioSource`]** on an entity plays a sound file (under `sounds/` by default):
//!   music, a voice line, a footstep where it was made. Every frame the player starts,
//!   stops, tunes and moves its voice to match the component, and writes back where the
//!   sound is (`position`): saves hold every sound's place, and a load resumes it there.
//! - **[`AudioListener`]** marks the entity that hears; without one, the main camera does.
//! - **Through the world**, with the game's physics ([`crate::Game::acoustics`]): walls
//!   between a sound and the listener muffle it, and the room around the listener gives
//!   the reverb (rays, [`AcousticSettings`]); a [`ReverbZone`] gives a place a reverb of
//!   its own.
//! - **The world's [`Mixer`]** sets the buses' volumes (a settings menu) and what the
//!   level's surfaces absorb.
//!
//! Sounds are read and decoded on a thread of their own. Shorter than
//! [`AudioSettings::stream_from`] seconds they are decoded once and kept; longer ones
//! stream from their files. A source starts once its sound is ready (a frame or two the
//! first time). The audio device opens on another thread too, with the first source or
//! video sound: a game without sound opens none.

mod acoustics;
mod components;
mod loader;

pub use components::{AudioListener, AudioSource, Falloff, PlayMode, ReverbZone, Room, Spatial};

use acoustics::{Acoustics, PROBE_RAYS_PER_FRAME};
use engine_audio::dsp::Reverb;
use engine_audio::output::{Output, OutputConfig};
use engine_audio::spatial::{Emitter, Listener, Rolloff};
use engine_audio::{
    Audio, AudioConfig, AudioError, BusId, Clip, EffectId, PlayParams, PreparedStream, Renderer,
    SendId, SendTap, VoiceHandle,
};
use engine_core::math::{Affine3A, Quat, Vec3};
use engine_ecs::{Entity, GlobalTransform, QueryState, World};
use engine_frame::ViewCamera;
use engine_physics::{PhysicsBackend, QueryFilter, RayHit};
use loader::{Done, Loaded, Loader, Request};
use std::collections::HashMap;
use std::path::PathBuf;
use std::thread::JoinHandle;

/// A voice stopped by the system fades out over this long (seconds), never clicking.
const FADE: f32 = 0.02;
/// Faster than this (m/s), a source or the listener jumped: no Doppler from it.
const TELEPORT: f32 = 120.0;
/// Seconds the room's reverb takes to follow a change (the listener walking into a hall).
const ROOM_FOLLOW: f64 = 0.4;
/// Seconds occlusion takes to follow a wall coming between.
const OCCLUSION_FOLLOW: f64 = 0.1;

/// What [`PlayerSettings::audio`](crate::PlayerSettings::audio) holds (ADR 0105).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSettings {
    /// Sound at all. `false` opens no audio device; videos play silent.
    pub enabled: bool,
    /// The folder [`AudioSource::sound`] paths are under, relative to the current
    /// directory as [`PlayerSettings::pack`](crate::PlayerSettings::pack) is.
    pub folder: PathBuf,
    /// The buses under the master, in order: a bus's parent is named before it.
    pub buses: Vec<BusSettings>,
    /// Sounds this long or longer (seconds) stream from their files; shorter ones are
    /// decoded once and kept in memory (a 10 s stereo sound takes 3.8 MB). WAV files are
    /// always decoded whole.
    pub stream_from: f32,
    /// The mixer's capacities.
    pub mixer: AudioConfig,
    /// Walls muffling sounds and the room's reverb, with the game's physics.
    pub acoustics: AcousticSettings,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            folder: PathBuf::from("sounds"),
            buses: vec![
                BusSettings::new("music"),
                BusSettings::new("effects").reverberant(),
                BusSettings::new("voice").reverberant(),
                BusSettings::new("ui"),
            ],
            stream_from: 10.0,
            mixer: AudioConfig::default(),
            acoustics: AcousticSettings::default(),
        }
    }
}

/// A bus of the player's mixer.
#[derive(Debug, Clone, PartialEq)]
pub struct BusSettings {
    /// Its name: sources and the [`Mixer`] name it.
    pub name: String,
    /// Linear gain to start with.
    pub volume: f32,
    /// The bus it feeds, by name (named before it); empty: the master.
    pub parent: String,
    /// Sent to the reverb of the room around the listener: effects and voices are, music
    /// and the UI are not.
    pub reverb: bool,
}

impl BusSettings {
    /// A bus at full volume into the master, dry.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            volume: 1.0,
            parent: String::new(),
            reverb: false,
        }
    }

    /// Sent to the room's reverb.
    pub fn reverberant(mut self) -> Self {
        self.reverb = true;
        self
    }
}

/// Sound through the world (ADR 0105), with the physics world the game gives
/// ([`crate::Game::acoustics`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticSettings {
    /// Walls between a placed sound and the listener muffle it.
    pub occlusion: bool,
    /// The room around the listener gives the reverb.
    pub reverb: bool,
    /// Rays a frame at most, for both together (a few microseconds each).
    pub rays_per_frame: u32,
    /// How far the room's rays reach, metres: one that reaches no wall is open sky.
    pub probe_distance: f32,
    /// The share of sound a surface absorbs at each bounce where the game sets none
    /// ([`Mixer::set_absorption`]): about 0.02 for stone, 0.1 for wood, 0.5 for heavy
    /// cloth.
    pub absorption: f32,
    /// The physics layers that stop and reflect sound (bit `n`: layer `n`): the static
    /// layer (0) by default, so characters and props do not muffle what is behind them.
    pub layers: u64,
}

impl Default for AcousticSettings {
    fn default() -> Self {
        Self {
            occlusion: true,
            reverb: true,
            rays_per_frame: 64,
            probe_distance: 60.0,
            absorption: 0.1,
            layers: 1,
        }
    }
}

/// The world's resource for the game's sound options (ADR 0105): the buses' volumes for
/// a settings menu, what the level's surfaces absorb, and why there is no sound if there
/// is none. Changes take effect at the next frame.
#[derive(Debug, Clone, Default)]
pub struct Mixer {
    buses: Vec<MixerBus>,
    /// Surface → absorption, sorted by surface.
    absorption: Vec<(u16, f32)>,
    absorption_changed: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct MixerBus {
    name: String,
    id: BusId,
    volume: f32,
    muted: bool,
    changed: bool,
}

impl Mixer {
    /// The buses' names, the master's (`master`) first.
    pub fn buses(&self) -> impl Iterator<Item = &str> {
        self.buses.iter().map(|b| b.name.as_str())
    }

    /// A bus's volume (linear gain).
    pub fn volume(&self, bus: &str) -> Option<f32> {
        self.bus(bus).map(|b| b.volume)
    }

    /// Sets a bus's volume (linear gain); `false` for a name no bus has.
    pub fn set_volume(&mut self, bus: &str, volume: f32) -> bool {
        self.bus_mut(bus)
            .map(|b| b.volume = volume.max(0.0))
            .is_some()
    }

    /// Whether a bus is muted.
    pub fn is_muted(&self, bus: &str) -> Option<bool> {
        self.bus(bus).map(|b| b.muted)
    }

    /// Mutes or unmutes a bus; `false` for a name no bus has.
    pub fn set_muted(&mut self, bus: &str, muted: bool) -> bool {
        self.bus_mut(bus).map(|b| b.muted = muted).is_some()
    }

    /// A bus's id, for what plays outside sources (a video's sound, `VideoDesc::bus`).
    pub fn bus_id(&self, bus: &str) -> Option<BusId> {
        self.bus(bus).map(|b| b.id)
    }

    /// What surface `surface` absorbs at each bounce, 0..1 (stone about 0.02, heavy cloth
    /// 0.5): a level's surfaces are its glTF material indices (`PhysicsBackend::surface_of`).
    /// Others absorb [`AcousticSettings::absorption`].
    pub fn set_absorption(&mut self, surface: u16, absorption: f32) {
        let absorption = absorption.clamp(0.0, 1.0);
        match self.absorption.binary_search_by_key(&surface, |&(s, _)| s) {
            Ok(i) => self.absorption[i].1 = absorption,
            Err(i) => self.absorption.insert(i, (surface, absorption)),
        }
        self.absorption_changed = true;
    }

    /// Why the player has no sound (sound is off, or the audio device could not start).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn bus(&self, name: &str) -> Option<&MixerBus> {
        self.buses.iter().find(|b| b.name == name)
    }

    fn bus_mut(&mut self, name: &str) -> Option<&mut MixerBus> {
        let bus = self.buses.iter_mut().find(|b| b.name == name)?;
        bus.changed = true;
        Some(bus)
    }
}

/// What a sound of the cache is.
enum State {
    Loading,
    Clip(Clip),
    Long { rate: u32 },
    Failed,
}

/// A sound, by its path.
struct SoundEntry {
    state: State,
    /// A play of it failed: said once.
    warned: bool,
}

/// The sounds read so far, by path.
#[derive(Default)]
struct Sounds {
    entries: Vec<SoundEntry>,
    by_path: HashMap<String, u32>,
}

impl Sounds {
    /// The sound at `path` (under `folder`), asked of the loader the first time.
    fn find_or_load(
        &mut self,
        path: &str,
        loader: &Loader,
        folder: &std::path::Path,
        stream_from: f32,
    ) -> Option<u32> {
        if path.is_empty() {
            return None;
        }
        if let Some(&id) = self.by_path.get(path) {
            return Some(id);
        }
        let id = self.entries.len() as u32;
        let asked = loader.send(Request::Load {
            id,
            path: folder.join(path),
            stream_from,
        });
        self.entries.push(SoundEntry {
            state: if asked { State::Loading } else { State::Failed },
            warned: false,
        });
        self.by_path.insert(path.to_owned(), id);
        Some(id)
    }
}

/// A source the system follows.
struct Track {
    entity: Entity,
    /// The sound it plays, and its path (a change of path is a new sound).
    path: String,
    sound: Option<u32>,
    voice: Option<VoiceHandle>,
    /// A long sound being opened for it by the loader.
    preparing: Option<u64>,
    /// The sound's sample rate (positions in seconds).
    rate: u32,
    /// The position written into the component last: another one is the game seeking.
    written: f64,
    /// What the voice was given.
    volume: f32,
    pitch: f32,
    velocity: Vec3,
    /// Where its entity was this frame, when placed.
    at: Option<Vec3>,
    /// Placed, and muffled by walls.
    occludable: bool,
    /// Occlusion now (followed smoothly), where it heads, and what the voice has.
    occlusion: f32,
    occlusion_target: f32,
    occlusion_sent: f32,
    /// The frame it was last seen in.
    seen: u32,
    /// A missing bus was reported.
    warned: bool,
}

impl Track {
    fn new(entity: Entity, written: f64) -> Self {
        Self {
            entity,
            path: String::new(),
            sound: None,
            voice: None,
            preparing: None,
            rate: 48_000,
            written,
            volume: 1.0,
            pitch: 1.0,
            velocity: Vec3::ZERO,
            at: None,
            occludable: false,
            occlusion: 0.0,
            occlusion_target: 0.0,
            occlusion_sent: 0.0,
            seen: 0,
            warned: false,
        }
    }

    fn stop(&mut self, audio: &mut Audio) {
        if let Some(voice) = self.voice.take() {
            audio.stop(voice, FADE);
        }
        self.preparing = None;
    }
}

/// The tracks, found by entity.
#[derive(Default)]
struct Tracks {
    list: Vec<Track>,
    /// Index into `list` by entity index (`u32::MAX`: none).
    of: Vec<u32>,
}

impl Tracks {
    fn get_or_insert(&mut self, entity: Entity, position: f64) -> &mut Track {
        let e = entity.index() as usize;
        if e >= self.of.len() {
            self.of.resize(e + 1, u32::MAX);
        }
        let i = self.of[e];
        if let Some(track) = self.list.get(i as usize)
            && track.entity == entity
        {
            return &mut self.list[i as usize];
        }
        self.of[e] = self.list.len() as u32;
        self.list.push(Track::new(entity, position));
        let last = self.list.len() - 1;
        &mut self.list[last]
    }

    /// Drops the tracks not seen in frame `stamp`, stopping their voices.
    fn drop_unseen(&mut self, stamp: u32, audio: &mut Audio) {
        let mut i = 0;
        while i < self.list.len() {
            if self.list[i].seen == stamp {
                i += 1;
                continue;
            }
            let mut gone = self.list.swap_remove(i);
            gone.stop(audio);
            if let Some(slot) = self.of.get_mut(gone.entity.index() as usize) {
                *slot = u32::MAX;
            }
            if let Some(moved) = self.list.get(i)
                && let Some(slot) = self.of.get_mut(moved.entity.index() as usize)
            {
                *slot = i as u32;
            }
        }
    }

    fn clear(&mut self, audio: &mut Audio) {
        for track in &mut self.list {
            track.stop(audio);
        }
        self.list.clear();
        self.of.fill(u32::MAX);
    }
}

type SourceQuery = QueryState<(
    Entity,
    &'static mut AudioSource,
    Option<&'static GlobalTransform>,
)>;

struct Queries {
    sources: SourceQuery,
    listeners: QueryState<(&'static AudioListener, &'static GlobalTransform)>,
    zones: QueryState<(&'static ReverbZone, &'static GlobalTransform)>,
}

/// The room's reverb: a bus with the reverb effect, fed by the reverberant buses.
struct ReverbBus {
    effect: EffectId,
    sends: Vec<SendId>,
    /// What the effect and the sends were last given.
    sent: Room,
}

/// The player's sound (see the module): the mixer, its output, the sources' voices.
pub(crate) struct Sound {
    settings: AudioSettings,
    /// The mixer; `None` with sound off.
    audio: Option<Audio>,
    /// The audio thread's side, until the output starts with it.
    renderer: Option<Renderer>,
    output: Option<Output>,
    /// The output starting on a thread of its own.
    starting: Option<JoinHandle<std::io::Result<Output>>>,
    /// Tests render the mixer themselves.
    manual: bool,
    error: Option<String>,
    /// The buses by name, the master first.
    buses: Vec<(String, BusId)>,
    reverb: Option<ReverbBus>,
    loader: Loader,
    sounds: Sounds,
    tickets: u64,
    /// Long sounds the loader opened, by ticket, until their tracks take them.
    prepared: Vec<(u64, PreparedStream)>,
    tracks: Tracks,
    stamp: u32,
    queries: Option<Queries>,
    /// The listener this frame, and last frame's position (its velocity).
    heard_at: Option<Vec3>,
    acoustics: Acoustics,
    /// The room heard now, followed smoothly.
    room: Room,
    /// Surface → absorption ([`Mixer::set_absorption`]).
    absorption: Vec<(u16, f32)>,
    /// The next track an occlusion ray goes to.
    next_occluded: usize,
    despawn: Vec<Entity>,
}

impl Sound {
    /// The player's sound: the mixer and its buses; the device opens with the first sound.
    pub(crate) fn new(settings: &AudioSettings) -> Self {
        Self::build(settings, false)
    }

    /// Without a device: the caller renders the mixer (tests).
    #[cfg(test)]
    pub(crate) fn manual(settings: &AudioSettings) -> (Self, Option<Renderer>) {
        let mut sound = Self::build(settings, true);
        let renderer = sound.renderer.take();
        (sound, renderer)
    }

    fn build(settings: &AudioSettings, manual: bool) -> Self {
        let mut sound = Self {
            settings: settings.clone(),
            audio: None,
            renderer: None,
            output: None,
            starting: None,
            manual,
            error: (!settings.enabled)
                .then(|| String::from("sound is off (AudioSettings::enabled)")),
            buses: Vec::new(),
            reverb: None,
            loader: Loader::start(),
            sounds: Sounds::default(),
            tickets: 0,
            prepared: Vec::new(),
            tracks: Tracks::default(),
            stamp: 0,
            queries: None,
            heard_at: None,
            acoustics: Acoustics::new(),
            room: Room {
                level: 0.0,
                ..Room::ROOM
            },
            absorption: Vec::new(),
            next_occluded: 0,
            despawn: Vec::new(),
        };
        if !settings.enabled {
            return sound;
        }
        let (mut audio, renderer) = Audio::new(&settings.mixer);
        sound.buses.push((String::from("master"), BusId::MASTER));
        for bus in &settings.buses {
            let parent = sound
                .buses
                .iter()
                .find(|(name, _)| !bus.parent.is_empty() && *name == bus.parent)
                .map_or(BusId::MASTER, |&(_, id)| id);
            match audio.add_bus(&bus.name, parent) {
                Ok(id) => {
                    audio.set_bus_gain(id, bus.volume, 0.0);
                    sound.buses.push((bus.name.clone(), id));
                }
                Err(e) => log::warn!(target: "engine_app::sound", "bus {}: {e}", bus.name),
            }
        }
        sound.audio = Some(audio);
        sound.renderer = Some(renderer);
        sound
    }

    /// The world's [`Mixer`] for these buses.
    pub(crate) fn mixer(&self) -> Mixer {
        let volume = |name: &str| {
            self.settings
                .buses
                .iter()
                .find(|b| b.name == name)
                .map_or(1.0, |b| b.volume)
        };
        Mixer {
            buses: self
                .buses
                .iter()
                .map(|(name, id)| MixerBus {
                    name: name.clone(),
                    id: *id,
                    volume: volume(name),
                    muted: false,
                    changed: false,
                })
                .collect(),
            absorption: Vec::new(),
            absorption_changed: false,
            error: self.error.clone(),
        }
    }

    /// Whether sound can play (it is on and the device did not fail).
    pub(crate) fn available(&self) -> bool {
        self.audio.is_some() && self.error.is_none()
    }

    /// Why there is no sound, if there is none.
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The mixer, once the device is open (videos' sound).
    pub(crate) fn live(&mut self) -> Option<&mut Audio> {
        if self.manual || self.output.is_some() {
            self.audio.as_mut()
        } else {
            None
        }
    }

    /// Opens the device, on a thread of its own, if it is not open yet.
    pub(crate) fn want_output(&mut self) {
        if self.manual || self.output.is_some() || self.starting.is_some() || self.error.is_some() {
            return;
        }
        let Some(renderer) = self.renderer.take() else {
            return;
        };
        match std::thread::Builder::new()
            .name(String::from("sound-open"))
            .spawn(move || Output::start(renderer, OutputConfig::default()))
        {
            Ok(handle) => self.starting = Some(handle),
            Err(e) => self.error = Some(format!("the audio device's thread: {e}")),
        }
    }

    /// Seconds between the mixer and the ears (the output's queue and the stream's latency).
    pub(crate) fn latency(&mut self) -> f64 {
        self.output.as_mut().map_or(0.0, |output| {
            let info = output.info();
            f64::from(info.queue_frames + info.stream_latency_frames)
                / f64::from(info.sample_rate.max(1))
        })
    }

    fn poll_output(&mut self) {
        if !self.starting.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        match self.starting.take().map(JoinHandle::join) {
            Some(Ok(Ok(mut output))) => {
                let info = output.info();
                log::info!(target: "engine_app::sound", "audio output: {} at {} Hz", info.device, info.sample_rate);
                self.output = Some(output);
            }
            Some(Ok(Err(e))) => self.error = Some(format!("the audio device: {e}")),
            Some(Err(_)) => self.error = Some(String::from("the audio device's start panicked")),
            None => {}
        }
    }

    /// One frame (after the ticks, before the videos): the mixer's returns, the loader's
    /// sounds, the listener, every source, the acoustics. `camera` hears without an
    /// [`AudioListener`]; `physics` is the game's world for the acoustics; `dt` is the
    /// frame's length in seconds.
    pub(crate) fn update(
        &mut self,
        world: &mut World,
        camera: Option<&ViewCamera>,
        physics: Option<&dyn PhysicsBackend>,
        dt: f64,
    ) {
        if self.audio.is_none() {
            return;
        }
        engine_profiler::zone!("app::Sound::update");
        self.poll_output();
        if let Some(audio) = self.audio.as_mut() {
            audio.update();
        }
        self.receive();
        self.apply_mixer(world);
        if self.queries.is_none() {
            self.queries = Some(Queries {
                sources: QueryState::new(world),
                listeners: QueryState::new(world),
                zones: QueryState::new(world),
            });
        }
        let live = self.manual || self.output.is_some();
        let listener = self.listen(world, camera, dt, live);
        self.stamp = self.stamp.wrapping_add(1);
        let wanted = self.follow_sources(world, dt, live);
        if wanted {
            self.want_output();
        }
        if let Some(audio) = self.audio.as_mut() {
            self.tracks.drop_unseen(self.stamp, audio);
        }
        for entity in self.despawn.drain(..) {
            world.despawn(entity);
        }
        if live {
            self.hear_the_world(world, listener, physics, dt);
        }
    }

    /// Takes what the loader made.
    fn receive(&mut self) {
        while let Some(done) = self.loader.next() {
            match done {
                Done::Loaded { id, result } => {
                    let Some(entry) = self.sounds.entries.get_mut(id as usize) else {
                        continue;
                    };
                    entry.state = match result {
                        Ok(Loaded::Clip(clip)) => State::Clip(clip),
                        Ok(Loaded::Long { rate }) => State::Long { rate },
                        Err(why) => {
                            log::warn!(target: "engine_app::sound", "sound {why}");
                            State::Failed
                        }
                    };
                }
                Done::Prepared { ticket, result } => match result {
                    Ok(stream) => {
                        if self.tracks.list.iter().any(|t| t.preparing == Some(ticket)) {
                            self.prepared.push((ticket, stream));
                        }
                    }
                    Err(why) => {
                        log::warn!(target: "engine_app::sound", "sound {why}");
                        for track in &mut self.tracks.list {
                            if track.preparing == Some(ticket) {
                                track.preparing = None;
                                if let Some(entry) = track
                                    .sound
                                    .and_then(|id| self.sounds.entries.get_mut(id as usize))
                                {
                                    entry.state = State::Failed;
                                }
                            }
                        }
                    }
                },
            }
        }
    }

    /// The game's changes to the world's [`Mixer`], and the error into it.
    fn apply_mixer(&mut self, world: &mut World) {
        let Some(mixer) = world.resource_mut::<Mixer>() else {
            return;
        };
        if mixer.error != self.error {
            mixer.error.clone_from(&self.error);
        }
        if mixer.absorption_changed {
            mixer.absorption_changed = false;
            self.absorption.clone_from(&mixer.absorption);
        }
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        for bus in &mut mixer.buses {
            if core::mem::take(&mut bus.changed) {
                audio.set_bus_gain(bus.id, bus.volume, 0.05);
                audio.set_bus_mute(bus.id, bus.muted);
            }
        }
    }

    /// Places the listener: the [`AudioListener`] entity's, or the camera's. Returns where
    /// it is.
    fn listen(
        &mut self,
        world: &World,
        camera: Option<&ViewCamera>,
        dt: f64,
        live: bool,
    ) -> Option<Vec3> {
        let queries = self.queries.as_mut()?;
        let mut found: Option<Affine3A> = None;
        queries.listeners.for_each_readonly(world, |(_, global)| {
            if found.is_none() {
                found = Some(global.0);
            }
        });
        let (position, rotation) = match found {
            Some(at) => {
                let (_, rotation, translation) = at.to_scale_rotation_translation();
                (translation, rotation)
            }
            None => {
                let camera = camera?;
                // The view's rotation is the camera's inverse.
                let rotation = Quat::from_mat4(&camera.view_from_world).conjugate();
                (camera.position, rotation.normalize())
            }
        };
        let velocity = velocity(self.heard_at, position, dt);
        self.heard_at = Some(position);
        if live && let Some(audio) = self.audio.as_mut() {
            audio.set_listener(&Listener {
                position,
                forward: rotation * Vec3::NEG_Z,
                up: rotation * Vec3::Y,
                velocity,
            });
        }
        Some(position)
    }

    /// Keeps every source's voice in step; returns whether any wants to play (the device
    /// is then opened).
    fn follow_sources(&mut self, world: &mut World, dt: f64, live: bool) -> bool {
        let Self {
            settings,
            audio,
            buses,
            loader,
            sounds,
            tickets,
            prepared,
            tracks,
            stamp,
            queries,
            despawn,
            ..
        } = self;
        let (Some(audio), Some(queries)) = (audio.as_mut(), queries.as_mut()) else {
            return false;
        };
        let mut ctx = Ctx {
            audio,
            live,
            buses,
            loader,
            sounds,
            tickets,
            prepared,
            despawn,
            folder: &settings.folder,
            stream_from: settings.stream_from,
            stamp: *stamp,
            dt,
            wanted: false,
        };
        queries.sources.for_each(world, |(entity, source, global)| {
            let track = tracks.get_or_insert(entity, source.position);
            follow(
                &mut ctx,
                track,
                entity,
                source,
                global.map(GlobalTransform::translation),
            );
        });
        ctx.wanted
    }

    /// Occlusion and the room's reverb from the game's physics and the zones.
    fn hear_the_world(
        &mut self,
        world: &World,
        listener: Option<Vec3>,
        physics: Option<&dyn PhysicsBackend>,
        dt: f64,
    ) {
        let acoustic = self.settings.acoustics;
        let filter = QueryFilter::layers(acoustic.layers);
        let mut budget = acoustic.rays_per_frame as usize;
        if let (Some(physics), Some(at)) = (physics, listener) {
            if acoustic.reverb {
                let table = &self.absorption;
                let default = acoustic.absorption;
                let absorption = |hit: &RayHit| {
                    physics
                        .surface_of(hit.body, hit.part)
                        .and_then(|s| {
                            table
                                .binary_search_by_key(&s, |&(surface, _)| surface)
                                .ok()
                                .map(|i| table[i].1)
                        })
                        .unwrap_or(default)
                };
                let rays = budget.min(PROBE_RAYS_PER_FRAME);
                budget -= self.acoustics.probe(
                    physics,
                    at,
                    acoustic.probe_distance,
                    rays,
                    filter,
                    &absorption,
                );
            }
            if acoustic.occlusion && !self.tracks.list.is_empty() {
                let n = self.tracks.list.len();
                let mut looked = 0;
                while budget > 0 && looked < n {
                    let i = self.next_occluded % n;
                    self.next_occluded = (i + 1) % n;
                    looked += 1;
                    let track = &mut self.tracks.list[i];
                    if let (true, Some(_), Some(to)) = (track.occludable, track.voice, track.at) {
                        track.occlusion_target = self.acoustics.occlusion(physics, at, to, filter);
                        budget -= 1;
                    }
                }
            }
        }
        // Occlusion follows smoothly.
        let follow = (1.0 - (-dt / OCCLUSION_FOLLOW).exp()) as f32;
        if let Some(audio) = self.audio.as_mut() {
            for track in &mut self.tracks.list {
                let Some(voice) = track.voice else {
                    continue;
                };
                let target = if track.occludable && physics.is_some() {
                    track.occlusion_target
                } else {
                    0.0
                };
                track.occlusion += (target - track.occlusion) * follow;
                if (track.occlusion - track.occlusion_sent).abs() > 0.01 {
                    audio.set_voice_occlusion(voice, track.occlusion);
                    track.occlusion_sent = track.occlusion;
                }
            }
        }
        // The room: the geometry's, under the zones.
        let geometry = if acoustic.reverb && physics.is_some() {
            self.acoustics.room()
        } else {
            None
        };
        let zone = listener.and_then(|at| self.zone(world, at));
        if geometry.is_none() && zone.is_none() && self.reverb.is_none() {
            return;
        }
        let base = geometry.unwrap_or(Room {
            level: 0.0,
            ..self.room
        });
        let target = match zone {
            Some((room, weight)) => base.lerp(&room, weight),
            None => base,
        };
        let follow = (1.0 - (-dt / ROOM_FOLLOW).exp()) as f32;
        self.room = self.room.lerp(&target, follow);
        self.send_room();
    }

    /// The zone the listener at `at` is in: its room and weight (the highest priority,
    /// then the strongest).
    fn zone(&mut self, world: &World, at: Vec3) -> Option<(Room, f32)> {
        let queries = self.queries.as_mut()?;
        let mut best: Option<(i32, f32, Room)> = None;
        queries.zones.for_each_readonly(world, |(zone, global)| {
            let (scale, _, _) = global.0.to_scale_rotation_translation();
            let local = global.0.inverse().transform_point3(at);
            let scale = ((scale.x.abs() + scale.y.abs() + scale.z.abs()) / 3.0).max(1e-6);
            let weight = engine_audio::spatial::ReverbZone {
                center: Vec3::ZERO,
                half_extents: zone.half_extents,
                fade: zone.fade / scale,
            }
            .weight(local);
            if weight <= 0.0 {
                return;
            }
            let better = best.as_ref().is_none_or(|&(priority, w, _)| {
                zone.priority > priority || (zone.priority == priority && weight > w)
            });
            if better {
                best = Some((zone.priority, weight, zone.room));
            }
        });
        best.map(|(_, weight, room)| (room, weight))
    }

    /// Gives the reverb the room heard now (made with the first room).
    fn send_room(&mut self) {
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        if self.reverb.is_none() {
            let room = self.room;
            let Ok(bus) = audio.add_bus("reverb", BusId::MASTER) else {
                return;
            };
            let mut effect = Reverb::new(room.decay, room.size);
            engine_audio::dsp::Effect::set_param(&mut effect, Reverb::DAMPING, room.damping);
            engine_audio::dsp::Effect::set_param(&mut effect, Reverb::PRE_DELAY, room.pre_delay);
            let Ok(effect) = audio.add_effect(bus, effect) else {
                return;
            };
            let sends = self
                .settings
                .buses
                .iter()
                .filter(|b| b.reverb)
                .filter_map(|b| self.buses.iter().find(|(name, _)| *name == b.name))
                .filter_map(|&(_, id)| audio.add_send(id, bus, room.level, SendTap::PostFader).ok())
                .collect();
            self.reverb = Some(ReverbBus {
                effect,
                sends,
                sent: room,
            });
            return;
        }
        let Some(reverb) = self.reverb.as_mut() else {
            return;
        };
        let (room, sent) = (self.room, &mut reverb.sent);
        let moved = |a: f32, b: f32, by: f32| (a - b).abs() > by * b.abs().max(1e-3);
        if moved(room.decay, sent.decay, 0.02) {
            audio.set_effect_param(reverb.effect, Reverb::DECAY, room.decay);
            sent.decay = room.decay;
        }
        if moved(room.damping, sent.damping, 0.03) {
            audio.set_effect_param(reverb.effect, Reverb::DAMPING, room.damping);
            sent.damping = room.damping;
        }
        // The lengths jump when these change: only for a real change of room.
        if (room.size - sent.size).abs() > 0.08 {
            audio.set_effect_param(reverb.effect, Reverb::SIZE, room.size);
            sent.size = room.size;
        }
        if (room.pre_delay - sent.pre_delay).abs() > 0.008 {
            audio.set_effect_param(reverb.effect, Reverb::PRE_DELAY, room.pre_delay);
            sent.pre_delay = room.pre_delay;
        }
        if (room.level - sent.level).abs() > 0.005 {
            for &send in &reverb.sends {
                audio.set_send_gain(send, room.level, 0.1);
            }
            sent.level = room.level;
        }
    }

    /// A save was loaded: every voice stops, and each playing source starts again where the
    /// save had it (from the start when it asks to).
    pub(crate) fn after_load(&mut self, world: &mut World) {
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        self.tracks.clear(audio);
        self.prepared.clear();
        self.acoustics.reset();
        if let Some(queries) = self.queries.as_mut() {
            queries.sources.for_each(world, |(_, source, _)| {
                if source.restart_on_load {
                    source.position = 0.0;
                }
            });
        }
    }

    /// Stops what plays (the player is closing).
    pub(crate) fn stop_all(&mut self) {
        if let Some(audio) = self.audio.as_mut() {
            self.tracks.clear(audio);
        }
        self.output = None;
    }
}

/// What following one source needs of the system.
struct Ctx<'a> {
    audio: &'a mut Audio,
    live: bool,
    buses: &'a [(String, BusId)],
    loader: &'a Loader,
    sounds: &'a mut Sounds,
    tickets: &'a mut u64,
    prepared: &'a mut Vec<(u64, PreparedStream)>,
    despawn: &'a mut Vec<Entity>,
    folder: &'a std::path::Path,
    stream_from: f32,
    stamp: u32,
    dt: f64,
    /// A source wants to play.
    wanted: bool,
}

/// Keeps one source's voice in step with its component.
fn follow(
    ctx: &mut Ctx<'_>,
    track: &mut Track,
    entity: Entity,
    source: &mut AudioSource,
    at: Option<Vec3>,
) {
    track.seen = ctx.stamp;
    let placed_at = source.spatial.and(at);
    if track.path != source.sound {
        track.stop(ctx.audio);
        track.path.clone_from(&source.sound);
        track.sound =
            ctx.sounds
                .find_or_load(&source.sound, ctx.loader, ctx.folder, ctx.stream_from);
        track.written = source.position;
    }
    track.occludable = source.spatial.is_some_and(|s| s.occlusion);
    if !source.playing {
        if let Some(voice) = track.voice.take() {
            if let Some(frames) = ctx.audio.position(voice) {
                source.position = frames as f64 / f64::from(track.rate.max(1));
            }
            ctx.audio.stop(voice, FADE);
        }
        track.preparing = None;
        track.written = source.position;
        track.at = placed_at;
        return;
    }
    if source.position != track.written {
        // The game moved it: it starts again there.
        track.stop(ctx.audio);
    }
    match track.voice {
        Some(voice) if ctx.audio.is_playing(voice) => {
            tune(ctx, track, voice, source, placed_at);
            if let Some(frames) = ctx.audio.position(voice) {
                source.position = frames as f64 / f64::from(track.rate.max(1));
            }
        }
        Some(_) => {
            // It ended by itself.
            track.voice = None;
            match source.mode {
                PlayMode::Once => {
                    source.playing = false;
                    source.position = 0.0;
                }
                PlayMode::Loop => source.position = 0.0,
                PlayMode::Despawn => ctx.despawn.push(entity),
            }
        }
        None => {
            ctx.wanted = true;
            if ctx.live {
                start(ctx, track, source, placed_at);
            }
        }
    }
    track.written = source.position;
    track.at = placed_at;
}

/// Starts a source's voice where its component is, once its sound is ready.
fn start(ctx: &mut Ctx<'_>, track: &mut Track, source: &AudioSource, at: Option<Vec3>) {
    let Some(id) = track.sound else {
        return;
    };
    let bus = match ctx.buses.iter().find(|(name, _)| *name == source.bus) {
        Some(&(_, id)) => id,
        None => {
            if !core::mem::replace(&mut track.warned, true) {
                log::warn!(target: "engine_app::sound", "{}: no bus {:?}; on the master", source.sound, source.bus);
            }
            BusId::MASTER
        }
    };
    let looping = source.mode == PlayMode::Loop;
    let params = PlayParams {
        bus,
        gain: source.volume,
        pan: 0.0,
        pitch: source.pitch,
        looping,
        priority: source.priority,
        start_at: None,
        fade_in: source.fade_in,
        offset: 0,
        spatial: source.spatial.map(|s| Emitter {
            position: at.unwrap_or(Vec3::ZERO),
            velocity: Vec3::ZERO,
            min_distance: s.min_distance.max(0.01),
            max_distance: s.max_distance.max(s.min_distance.max(0.01)),
            rolloff: match s.falloff {
                Falloff::Inverse => Rolloff::Inverse,
                Falloff::Linear => Rolloff::Linear,
            },
            doppler: s.doppler,
            air_absorption: 1.0,
        }),
    };
    let Some(entry) = ctx.sounds.entries.get_mut(id as usize) else {
        return;
    };
    let started = match &entry.state {
        State::Loading | State::Failed => return,
        State::Clip(clip) => {
            let offset = frames_at(source.position, clip.sample_rate());
            let rate = clip.sample_rate();
            ctx.audio
                .play_with(clip, &PlayParams { offset, ..params })
                .map(|voice| (voice, rate))
        }
        &State::Long { rate } => match track.preparing {
            None => {
                *ctx.tickets += 1;
                let ticket = *ctx.tickets;
                let asked = ctx.loader.send(Request::Prepare {
                    ticket,
                    path: ctx.folder.join(&source.sound),
                    offset: frames_at(source.position, rate),
                    looping,
                    buffer_ms: ctx.audio.stream_buffer_ms(),
                });
                if asked {
                    track.preparing = Some(ticket);
                }
                return;
            }
            Some(ticket) => {
                let Some(i) = ctx.prepared.iter().position(|&(t, _)| t == ticket) else {
                    return;
                };
                let (_, stream) = ctx.prepared.swap_remove(i);
                track.preparing = None;
                ctx.audio
                    .play_prepared(stream, &params)
                    .map(|voice| (voice, rate))
            }
        },
    };
    match started {
        Ok((voice, rate)) => {
            track.voice = Some(voice);
            track.rate = rate;
            track.volume = source.volume;
            track.pitch = source.pitch;
            track.velocity = Vec3::ZERO;
            track.occlusion_sent = 0.0;
            track.occlusion = 0.0;
        }
        // Tried again next frame.
        Err(AudioError::VoicesExhausted) => {}
        Err(e) => {
            if !core::mem::replace(&mut entry.warned, true) {
                log::warn!(target: "engine_app::sound", "{}: {e}", source.sound);
            }
        }
    }
}

/// Sends a playing voice what changed in its component.
fn tune(
    ctx: &mut Ctx<'_>,
    track: &mut Track,
    voice: VoiceHandle,
    source: &AudioSource,
    at: Option<Vec3>,
) {
    if source.volume != track.volume {
        ctx.audio.set_voice_gain(voice, source.volume, 0.05);
        track.volume = source.volume;
    }
    if source.pitch != track.pitch {
        ctx.audio.set_voice_pitch(voice, source.pitch);
        track.pitch = source.pitch;
    }
    if let Some(at) = at {
        let velocity = velocity(track.at, at, ctx.dt);
        if track.at != Some(at) || velocity != track.velocity {
            ctx.audio.set_voice_position(voice, at, velocity);
            track.velocity = velocity;
        }
    }
}

/// The velocity from `last` to `now` over `dt` seconds; none across a jump.
fn velocity(last: Option<Vec3>, now: Vec3, dt: f64) -> Vec3 {
    match last {
        Some(last) if dt > 0.0 => {
            let v = (now - last) / dt as f32;
            if v.length_squared() > TELEPORT * TELEPORT {
                Vec3::ZERO
            } else {
                v
            }
        }
        _ => Vec3::ZERO,
    }
}

/// `seconds` into a sound of `rate` Hz, in frames.
fn frames_at(seconds: f64, rate: u32) -> u64 {
    (seconds.max(0.0) * f64::from(rate)).round() as u64
}

#[cfg(test)]
mod tests;
