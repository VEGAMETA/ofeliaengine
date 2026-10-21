//! Sound in the player (ADR 0105): what entities play, heard from the listener, through
//! the world.
//!
//! - **[`AudioSource`]** on an entity plays a sound as it is: music, the UI, a narrator.
//! - **[`AudioSource3D`]** plays one in the world, at its entity: fainter with distance,
//!   from its side; with the game's physics ([`crate::Game::acoustics`]), muffled by walls
//!   and answered by the room around the listener (rays, [`AcousticSettings`]). A
//!   [`ReverbZone`] gives a place a reverb of its own.
//! - **Every frame** the player starts, stops, tunes and moves each source's voice to match
//!   its component, and writes back where the sound is (`position`): saves hold every
//!   sound's place, and a load resumes it there.
//! - **[`AudioListener`]** marks the entity that hears; without one, the main camera does.
//! - **The world's [`Mixer`]** sets the buses' volumes (a settings menu) and what the
//!   level's surfaces absorb.
//!
//! **The sounds' files** ([`SoundFiles`], the owner's choice of 2026-10-16): a folder while
//! developing, a changed file heard at once; a sound pack (`ofeliaengine sounds build`) in
//! a release. They are read and decoded on a thread of their own. Shorter than
//! [`AudioSettings::stream_from`] seconds they are decoded once and kept; longer ones
//! stream (from the pack's file in place). A source starts once its sound is ready (a frame
//! or two the first time).
//!
//! The audio device opens on another thread too, with the first source or video sound: a
//! game without sound opens none.

mod acoustics;
mod components;
mod loader;

pub use components::{
    AudioListener, AudioSource, AudioSource3D, Falloff, PlayMode, ReverbZone, Room,
};

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
use engine_platform::fs_watch::{DirectoryWatcher, FileEventKind};
use loader::{Done, Loaded, Loader, Request, Store};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A voice stopped by the system fades out over this long (seconds), never clicking.
const FADE: f32 = 0.02;
/// Faster than this (m/s), a source or the listener jumped: no Doppler from it.
const TELEPORT: f32 = 120.0;
/// Seconds the room's reverb takes to follow a change (the listener walking into a hall).
const ROOM_FOLLOW: f64 = 0.4;
/// Seconds occlusion takes to follow a wall coming between.
const OCCLUSION_FOLLOW: f64 = 0.1;
/// How long a changed sound file must stay unchanged before it is read again.
const QUIET: Duration = Duration::from_millis(50);

/// Where the sounds' files are read from (ADR 0105; the owner, 2026-10-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SoundFiles {
    /// The folder in development builds, a changed file heard at once; the pack in release
    /// builds. Each falls back to the other where it is missing.
    #[default]
    ByBuild,
    /// The folder, kept live, in every build.
    Folder,
    /// The pack, in every build.
    Pack,
}

/// What [`PlayerSettings::audio`](crate::PlayerSettings::audio) holds (ADR 0105).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSettings {
    /// Sound at all. `false` opens no audio device; videos play silent.
    pub enabled: bool,
    /// The folder sources' paths are under, relative to the current directory as
    /// [`PlayerSettings::pack`](crate::PlayerSettings::pack) is: `sounds`.
    pub folder: PathBuf,
    /// The sound pack (`ofeliaengine sounds build <folder> -o <pack>`): `sounds.pack`.
    pub pack: PathBuf,
    /// Which of the two the sounds are read from.
    pub files: SoundFiles,
    /// The buses under the master, in order: a bus's parent is named before it.
    pub buses: Vec<BusSettings>,
    /// Sounds this long or longer (seconds) stream; shorter ones are decoded once and kept
    /// in memory (a 10 s stereo sound takes 3.8 MB). WAV files are always decoded whole.
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
            pack: PathBuf::from("sounds.pack"),
            files: SoundFiles::ByBuild,
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
    /// Its [`AudioSource3D`]s reverberate with the room around the listener (effects and
    /// voices do; music and the UI do not). Its plain [`AudioSource`]s never do.
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

    /// Its 3D sounds reverberate.
    pub fn reverberant(mut self) -> Self {
        self.reverb = true;
        self
    }
}

/// Sound through the world (ADR 0105), with the physics world the game gives
/// ([`crate::Game::acoustics`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticSettings {
    /// Walls between a 3D sound and the listener muffle it.
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

/// A bus of the mixer, and the child its reverberating 3D sounds play on.
struct Bus {
    name: String,
    id: BusId,
    /// Feeds `id` and the reverb (a bus with `reverb`).
    world: Option<BusId>,
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
    /// Being read again (its file changed): what it was plays until then.
    reloading: bool,
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
    /// The sound at `path`, asked of the loader the first time.
    fn find_or_load(&mut self, path: &str, loader: &Loader, stream_from: f32) -> Option<u32> {
        if path.is_empty() {
            return None;
        }
        if let Some(&id) = self.by_path.get(path) {
            return Some(id);
        }
        let id = self.entries.len() as u32;
        let asked = loader.send(Request::Load {
            id,
            path: path.to_owned(),
            stream_from,
        });
        self.entries.push(SoundEntry {
            state: if asked { State::Loading } else { State::Failed },
            reloading: false,
            warned: false,
        });
        self.by_path.insert(path.to_owned(), id);
        Some(id)
    }
}

/// How a 3D source sits in the world, this frame.
#[derive(Debug, Clone, Copy)]
struct Placement {
    at: Option<Vec3>,
    pitch: f32,
    min_distance: f32,
    max_distance: f32,
    falloff: Falloff,
    doppler: f32,
    occlusion: bool,
    reverb: bool,
    priority: u8,
}

impl Placement {
    fn of(source: &AudioSource3D, at: Option<Vec3>) -> Self {
        Self {
            at,
            pitch: source.pitch,
            min_distance: source.min_distance,
            max_distance: source.max_distance,
            falloff: source.falloff,
            doppler: source.doppler,
            occlusion: source.occlusion,
            reverb: source.reverb,
            priority: source.priority,
        }
    }
}

/// A source the system follows.
struct Track {
    entity: Entity,
    /// The sound it plays, and its path (a change of path is a new sound).
    path: String,
    sound: Option<u32>,
    voice: Option<VoiceHandle>,
    /// The bus the voice plays on.
    bus: BusId,
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
    /// Where its entity was this frame (3D sources).
    at: Option<Vec3>,
    /// A 3D source muffled by walls.
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
            bus: BusId::MASTER,
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

/// The tracks of one kind of source, found by entity.
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

    /// Stops the voices playing sound `id`: they start again where they are, with what
    /// the sound is now.
    fn restart(&mut self, id: u32, audio: &mut Audio) {
        for track in &mut self.list {
            if track.sound == Some(id) {
                track.stop(audio);
            }
        }
    }
}

type PlainQuery = QueryState<(Entity, &'static mut AudioSource)>;
type WorldQuery = QueryState<(
    Entity,
    &'static mut AudioSource3D,
    Option<&'static GlobalTransform>,
)>;

struct Queries {
    plain: PlainQuery,
    placed: WorldQuery,
    listeners: QueryState<(&'static AudioListener, &'static GlobalTransform)>,
    zones: QueryState<(&'static ReverbZone, &'static GlobalTransform)>,
}

/// The room's reverb: a bus with the reverb effect, fed by the reverberant buses' world
/// children.
struct ReverbBus {
    effect: EffectId,
    sends: Vec<SendId>,
    /// What the effect and the sends were last given.
    sent: Room,
}

/// The sounds folder, watched: a changed file is read again.
struct LiveFolder {
    watcher: DirectoryWatcher,
    /// Paths changed (with `/`, from the folder), and when last.
    changed: Vec<(String, Instant)>,
    /// Everything changed (the watcher lost track).
    everything: Option<Instant>,
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
    /// The buses, the master first.
    buses: Vec<Bus>,
    reverb: Option<ReverbBus>,
    /// Where the sounds come from, said once at the start.
    note: String,
    live_folder: Option<LiveFolder>,
    loader: Loader,
    sounds: Sounds,
    tickets: u64,
    /// Long sounds the loader opened, by ticket, until their tracks take them.
    prepared: Vec<(u64, PreparedStream)>,
    plain: Tracks,
    placed: Tracks,
    stamp: u32,
    queries: Option<Queries>,
    /// The listener this frame, and last frame's position (its velocity).
    heard_at: Option<Vec3>,
    acoustics: Acoustics,
    /// The room heard now, followed smoothly.
    room: Room,
    /// Surface → absorption ([`Mixer::set_absorption`]).
    absorption: Vec<(u16, f32)>,
    /// The next 3D track an occlusion ray goes to.
    next_occluded: usize,
    despawn: Vec<Entity>,
}

/// Where the sounds come from: the store, whether to watch the folder, and a note.
fn choose_store(settings: &AudioSettings) -> (Store, bool, String) {
    let folder = || {
        let found = settings.folder.is_dir();
        let note = if found {
            format!("sounds from `{}`, kept live", settings.folder.display())
        } else {
            format!(
                "sounds from `{}` (not there; nor the pack `{}`)",
                settings.folder.display(),
                settings.pack.display()
            )
        };
        (Store::Folder(settings.folder.clone()), found, note)
    };
    let pack = || {
        engine_assets::sounds::SoundPack::open(&settings.pack).map(|pack| {
            let note = format!(
                "sounds from `{}` ({} sounds)",
                settings.pack.display(),
                pack.len()
            );
            (Store::Pack(Arc::new(pack)), false, note)
        })
    };
    let pack_first = match settings.files {
        SoundFiles::ByBuild => !cfg!(debug_assertions),
        SoundFiles::Folder => false,
        SoundFiles::Pack => true,
    };
    let fallback = settings.files == SoundFiles::ByBuild;
    if pack_first {
        match pack() {
            Ok(store) => store,
            Err(e) if fallback && settings.folder.is_dir() => {
                let (store, watch, note) = folder();
                (store, watch, format!("{note} (the pack: {e})"))
            }
            Err(e) => {
                log::warn!(target: "engine_app::sound", "the sound pack `{}`: {e}", settings.pack.display());
                let note = format!("no sounds: the pack `{}`: {e}", settings.pack.display());
                (Store::Folder(settings.folder.clone()), false, note)
            }
        }
    } else if !settings.folder.is_dir()
        && fallback
        && let Ok(store) = pack()
    {
        store
    } else {
        folder()
    }
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
        let (store, watch, note) = if settings.enabled {
            choose_store(settings)
        } else {
            (
                Store::Folder(settings.folder.clone()),
                false,
                String::from("sound is off"),
            )
        };
        let live_folder = watch
            .then(|| match DirectoryWatcher::new(&settings.folder, true) {
                Ok(watcher) => Some(LiveFolder {
                    watcher,
                    changed: Vec::new(),
                    everything: None,
                }),
                Err(e) => {
                    log::warn!(target: "engine_app::sound", "watching `{}`: {e}", settings.folder.display());
                    None
                }
            })
            .flatten();
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
            note,
            live_folder,
            loader: Loader::start(store),
            sounds: Sounds::default(),
            tickets: 0,
            prepared: Vec::new(),
            plain: Tracks::default(),
            placed: Tracks::default(),
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
        sound.buses.push(Bus {
            name: String::from("master"),
            id: BusId::MASTER,
            world: None,
        });
        for bus in &settings.buses {
            let parent = sound
                .buses
                .iter()
                .find(|b| !bus.parent.is_empty() && b.name == bus.parent)
                .map_or(BusId::MASTER, |b| b.id);
            let id = match audio.add_bus(&bus.name, parent) {
                Ok(id) => id,
                Err(e) => {
                    log::warn!(target: "engine_app::sound", "bus {}: {e}", bus.name);
                    continue;
                }
            };
            audio.set_bus_gain(id, bus.volume, 0.0);
            // Its reverberating 3D sounds play on a child: the parent's volume still
            // applies, and its plain sounds stay dry.
            let world = bus
                .reverb
                .then(|| audio.add_bus(&format!("{}.world", bus.name), id).ok())
                .flatten();
            sound.buses.push(Bus {
                name: bus.name.clone(),
                id,
                world,
            });
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
                .map(|b| MixerBus {
                    name: b.name.clone(),
                    id: b.id,
                    volume: volume(&b.name),
                    muted: false,
                    changed: false,
                })
                .collect(),
            absorption: Vec::new(),
            absorption_changed: false,
            error: self.error.clone(),
        }
    }

    /// Where the sounds come from (for the start-up notes).
    pub(crate) fn note(&self) -> &str {
        &self.note
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
    /// sounds, changed files, the listener, every source, the acoustics. `camera` hears
    /// without an [`AudioListener`]; `physics` is the game's world for the acoustics; `dt`
    /// is the frame's length in seconds.
    pub(crate) fn update(
        &mut self,
        world: &mut World,
        camera: Option<&ViewCamera>,
        physics: Option<&dyn PhysicsBackend>,
        dt: f64,
        simulating: bool,
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
        self.watch_folder();
        self.apply_mixer(world);
        // The world kept still (the editor while editing, a pause): its sources are silent,
        // and where each one is stays as it was.
        if !simulating {
            if let Some(audio) = self.audio.as_mut() {
                self.plain.clear(audio);
                self.placed.clear(audio);
            }
            return;
        }
        if self.queries.is_none() {
            self.queries = Some(Queries {
                plain: QueryState::new(world),
                placed: QueryState::new(world),
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
            self.plain.drop_unseen(self.stamp, audio);
            self.placed.drop_unseen(self.stamp, audio);
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
                    let reload = core::mem::take(&mut entry.reloading);
                    match result {
                        Ok(loaded) => {
                            entry.state = match loaded {
                                Loaded::Clip(clip) => State::Clip(clip),
                                Loaded::Long { rate } => State::Long { rate },
                            };
                            entry.warned = false;
                            if reload && let Some(audio) = self.audio.as_mut() {
                                self.plain.restart(id, audio);
                                self.placed.restart(id, audio);
                            }
                        }
                        // A file being saved can be read half-written: what played goes on.
                        Err(why) if reload => {
                            log::warn!(target: "engine_app::sound", "sound {why}; the previous one plays on");
                        }
                        Err(why) => {
                            log::warn!(target: "engine_app::sound", "sound {why}");
                            entry.state = State::Failed;
                        }
                    }
                }
                Done::Prepared { ticket, result } => match result {
                    Ok(stream) => {
                        let waited = |t: &Track| t.preparing == Some(ticket);
                        if self.plain.list.iter().any(waited) || self.placed.list.iter().any(waited)
                        {
                            self.prepared.push((ticket, stream));
                        }
                    }
                    Err(why) => {
                        log::warn!(target: "engine_app::sound", "sound {why}");
                        for track in self.plain.list.iter_mut().chain(&mut self.placed.list) {
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

    /// Changed files of the live folder, read again once they have been quiet.
    fn watch_folder(&mut self) {
        let Some(live) = self.live_folder.as_mut() else {
            return;
        };
        while let Some(event) = live.watcher.try_next() {
            if event.kind == FileEventKind::Rescan {
                live.everything = Some(Instant::now());
                continue;
            }
            // The watcher reports paths under its root as it holds it.
            let Ok(relative) = event.path.strip_prefix(live.watcher.root()) else {
                continue;
            };
            let name = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            match live.changed.iter_mut().find(|(n, _)| *n == name) {
                Some((_, at)) => *at = Instant::now(),
                None => live.changed.push((name, Instant::now())),
            }
        }
        let mut again = Vec::new();
        live.changed.retain(|(name, at)| {
            let quiet = at.elapsed() >= QUIET;
            if quiet {
                again.push(name.clone());
            }
            !quiet
        });
        if live.everything.is_some_and(|at| at.elapsed() >= QUIET) {
            live.everything = None;
            again = self.sounds.by_path.keys().cloned().collect();
        }
        for name in again {
            let Some(&id) = self.sounds.by_path.get(&name) else {
                continue;
            };
            let Some(entry) = self.sounds.entries.get_mut(id as usize) else {
                continue;
            };
            let asked = self.loader.send(Request::Load {
                id,
                path: name,
                stream_from: self.settings.stream_from,
            });
            if asked {
                entry.reloading = true;
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
            plain,
            placed,
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
            stream_from: settings.stream_from,
            stamp: *stamp,
            dt,
            wanted: false,
        };
        queries.plain.for_each(world, |(entity, source)| {
            let track = plain.get_or_insert(entity, source.position);
            follow(&mut ctx, track, entity, source, None);
        });
        queries.placed.for_each(world, |(entity, source, global)| {
            let track = placed.get_or_insert(entity, source.position);
            let placement = Placement::of(source, global.map(GlobalTransform::translation));
            follow(&mut ctx, track, entity, &mut source.source, Some(placement));
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
            if acoustic.occlusion && !self.placed.list.is_empty() {
                let n = self.placed.list.len();
                let mut looked = 0;
                while budget > 0 && looked < n {
                    let i = self.next_occluded % n;
                    self.next_occluded = (i + 1) % n;
                    looked += 1;
                    let track = &mut self.placed.list[i];
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
            for track in &mut self.placed.list {
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
                .buses
                .iter()
                .filter_map(|b| b.world)
                .filter_map(|world| {
                    audio
                        .add_send(world, bus, room.level, SendTap::PostFader)
                        .ok()
                })
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
        self.plain.clear(audio);
        self.placed.clear(audio);
        self.prepared.clear();
        self.acoustics.reset();
        if let Some(queries) = self.queries.as_mut() {
            queries.plain.for_each(world, |(_, source)| {
                if source.restart_on_load {
                    source.position = 0.0;
                }
            });
            queries.placed.for_each(world, |(_, source, _)| {
                if source.restart_on_load {
                    source.position = 0.0;
                }
            });
        }
    }

    /// Stops what plays (the player is closing).
    pub(crate) fn stop_all(&mut self) {
        if let Some(audio) = self.audio.as_mut() {
            self.plain.clear(audio);
            self.placed.clear(audio);
        }
        self.output = None;
    }
}

/// What following one source needs of the system.
struct Ctx<'a> {
    audio: &'a mut Audio,
    live: bool,
    buses: &'a [Bus],
    loader: &'a Loader,
    sounds: &'a mut Sounds,
    tickets: &'a mut u64,
    prepared: &'a mut Vec<(u64, PreparedStream)>,
    despawn: &'a mut Vec<Entity>,
    stream_from: f32,
    stamp: u32,
    dt: f64,
    /// A source wants to play.
    wanted: bool,
}

/// Keeps one source's voice in step with its component; `placed` for a 3D one.
fn follow(
    ctx: &mut Ctx<'_>,
    track: &mut Track,
    entity: Entity,
    source: &mut AudioSource,
    placed: Option<Placement>,
) {
    track.seen = ctx.stamp;
    let at = placed.and_then(|p| p.at);
    if track.path != source.sound {
        track.stop(ctx.audio);
        track.path.clone_from(&source.sound);
        track.sound = ctx
            .sounds
            .find_or_load(&source.sound, ctx.loader, ctx.stream_from);
        track.written = source.position;
    }
    track.occludable = placed.is_some_and(|p| p.occlusion);
    if !source.playing {
        if let Some(voice) = track.voice.take() {
            if let Some(frames) = ctx.audio.position(voice) {
                source.position = frames as f64 / f64::from(track.rate.max(1));
            }
            ctx.audio.stop(voice, FADE);
        }
        track.preparing = None;
        track.written = source.position;
        track.at = at;
        return;
    }
    if source.position != track.written {
        // The game moved it: it starts again there.
        track.stop(ctx.audio);
    }
    match track.voice {
        Some(voice) if ctx.audio.is_playing(voice) => {
            tune(ctx, track, voice, source, placed);
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
                start(ctx, track, source, placed);
            }
        }
    }
    track.written = source.position;
    track.at = at;
}

/// Starts a source's voice where its component is, once its sound is ready.
fn start(ctx: &mut Ctx<'_>, track: &mut Track, source: &AudioSource, placed: Option<Placement>) {
    let Some(id) = track.sound else {
        return;
    };
    let bus = match ctx.buses.iter().find(|b| b.name == source.bus) {
        // A 3D sound that reverberates plays on its bus's world child.
        Some(b) => match (placed, b.world) {
            (Some(p), Some(world)) if p.reverb => world,
            _ => b.id,
        },
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
        pitch: placed.map_or(1.0, |p| p.pitch),
        looping,
        priority: placed.map_or(128, |p| p.priority),
        start_at: None,
        fade_in: 0.0,
        offset: 0,
        spatial: placed.map(|p| Emitter {
            position: p.at.unwrap_or(Vec3::ZERO),
            velocity: Vec3::ZERO,
            min_distance: p.min_distance.max(0.01),
            max_distance: p.max_distance.max(p.min_distance.max(0.01)),
            rolloff: match p.falloff {
                Falloff::Inverse => Rolloff::Inverse,
                Falloff::Linear => Rolloff::Linear,
            },
            doppler: p.doppler,
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
                    path: source.sound.clone(),
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
            track.bus = bus;
            track.rate = rate;
            track.volume = source.volume;
            track.pitch = params.pitch;
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
    placed: Option<Placement>,
) {
    if source.volume != track.volume {
        ctx.audio.set_voice_gain(voice, source.volume, 0.05);
        track.volume = source.volume;
    }
    let Some(placed) = placed else {
        return;
    };
    if placed.pitch != track.pitch {
        ctx.audio.set_voice_pitch(voice, placed.pitch);
        track.pitch = placed.pitch;
    }
    if let Some(at) = placed.at {
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
