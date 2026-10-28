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
mod layout;
mod loader;
mod waveform;

pub use components::{
    AudioListener, AudioSource, AudioSource3D, Falloff, PlayMode, ReverbZone, Room,
};
use layout::linear;
pub use layout::{
    BusLayout, EffectLayout, MIXER_FILE_EXTENSION, MasterLayout, MixerFile, PlacedBus, RoomLayout,
    Routes, SendLayout, SnapshotLayout, SnapshotTarget, snapshot_target,
};
pub use waveform::{Waveform, waveform};

use acoustics::{Acoustics, PROBE_RAYS_PER_FRAME};
use engine_audio::dsp::{Reverb, effect_info};
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
use loader::{Done, Loaded, Loader, PreviewSound, Request, Store};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// The mixer's layout (ADR 0121): the `.mixer` file at this path under the project
    /// (`main.mixer`), its buses and faders. Without the file, the built-in four
    /// ([`MixerFile::builtin`]): music, effects, voice and ui.
    pub layout: PathBuf,
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
            layout: PathBuf::from("main.mixer"),
            stream_from: 10.0,
            mixer: AudioConfig::default(),
            acoustics: AcousticSettings::default(),
        }
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
    /// The layout's snapshots (ADR 0122): their weights as heard, and what the game asked.
    snapshots: Vec<MixerSnapshot>,
    /// Surface → absorption, sorted by surface.
    absorption: Vec<(u16, f32)>,
    absorption_changed: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct MixerSnapshot {
    name: String,
    /// Its weight as heard now.
    weight: f32,
    /// A blend asked since the sound last read it: the weight, and the seconds (`None`:
    /// the snapshot's fade).
    asked: Option<(f32, Option<f32>)>,
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

    /// A bus's volume set by the game (a linear factor, 1 to start with), on top of the
    /// layout's fader.
    pub fn volume(&self, bus: &str) -> Option<f32> {
        self.bus(bus).map(|b| b.volume)
    }

    /// Sets a bus's volume (a linear factor) on top of the layout's fader: an options
    /// menu's 0.8 keeps the mix's −6 dB under it. `false` for a name no bus has.
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

    /// The layout's snapshots by name, in the order their layers stack (ADR 0122).
    pub fn snapshots(&self) -> impl Iterator<Item = &str> {
        self.snapshots.iter().map(|s| s.name.as_str())
    }

    /// Moves snapshot `name`'s weight to `weight` (0: not heard, 1: all of it) over
    /// `seconds` (`None`: the snapshot's own fade), from where it is now: a pause menu's
    /// `blend("paused", 1.0, None)`, and `0.0` to leave it. `false` for a name the layout
    /// has no snapshot for.
    pub fn blend(&mut self, name: &str, weight: f32, seconds: Option<f32>) -> bool {
        let Some(s) = self.snapshots.iter_mut().find(|s| s.name == name) else {
            return false;
        };
        let weight = if weight.is_nan() {
            0.0
        } else {
            weight.clamp(0.0, 1.0)
        };
        s.asked = Some((weight, seconds.map(|t| t.max(0.0))));
        true
    }

    /// Snapshot `name`'s weight as heard now.
    pub fn weight(&self, name: &str) -> Option<f32> {
        self.snapshots
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.weight)
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

/// A bus of the mixer as the layout made it, and the child its reverberating 3D sounds
/// play on.
struct Bus {
    name: String,
    id: BusId,
    /// Feeds `id` and the reverb (a bus with `reverb`).
    world: Option<BusId>,
    /// The bus it feeds as made (`None`: the master; the master's own: `None`).
    parent: Option<String>,
    /// Its fader as a linear factor, under the game's volume.
    fader: f32,
    /// What the layout gave it last.
    layout: BusLayout,
    /// Its effects as made, one for each of the layout's in its order.
    effects: Vec<LiveEffect>,
    /// Its sends as made.
    sends: Vec<LiveSend>,
}

/// An effect of a bus as the layout made it.
struct LiveEffect {
    /// What the layout gave it last.
    layout: EffectLayout,
    /// The effect (`None`: a kind the engine does not have, holding its place).
    id: Option<EffectId>,
    /// The bus keying it, as made.
    keyed_by: Option<String>,
}

/// A send of a bus as the layout made it.
struct LiveSend {
    /// The bus it feeds.
    to: String,
    id: SendId,
    pre_fader: bool,
    /// Its level in dB, as given.
    gain: f32,
}

/// For each effect of `wanted`, the one of `live` it carries on, its state with it: one
/// given the same, else of the same kind at the same place, else the first of its kind
/// left; `None`: a new one.
fn match_effects(live: &[LiveEffect], wanted: &[EffectLayout]) -> Vec<Option<usize>> {
    let mut taken = vec![false; live.len()];
    let mut out = vec![None; wanted.len()];
    let rules: [&dyn Fn(usize, usize) -> bool; 3] = [
        &|i, j| live[j].layout == wanted[i],
        &|i, j| i == j && live[j].layout.kind == wanted[i].kind,
        &|i, j| live[j].layout.kind == wanted[i].kind,
    ];
    for fits in rules {
        for (i, slot) in out.iter_mut().enumerate() {
            if slot.is_none()
                && let Some(j) = (0..live.len()).find(|&j| !taken[j] && fits(i, j))
            {
                *slot = Some(j);
                taken[j] = true;
            }
        }
    }
    out
}

/// Effect `id`'s parameters and bypass from `before` to `after` (of one kind), where they
/// changed.
fn set_effect(audio: &mut Audio, id: EffectId, before: &EffectLayout, after: &EffectLayout) {
    if let Some(info) = effect_info(&after.kind) {
        for p in info.params {
            let value = after.param(p.name);
            if value != before.param(p.name)
                && let Some(value) = value
            {
                audio.set_effect_param(id, p.index, p.effect_value(value));
            }
        }
    }
    if before.bypass != after.bypass {
        let _ = audio.set_effect_bypass(id, after.bypass);
    }
}

/// The values of `blended` (the layout under its snapshots, ADR 0122) given to what is laid
/// out, where they changed: faders (whether one changed: the game's volumes then multiply
/// them again), balances, sends' levels, effects' parameters and bypass, the room's trim.
/// Nothing is made or moved.
fn give_blended(
    audio: &mut Audio,
    buses: &mut [Bus],
    blended: &MixerFile,
    reverb: Option<&ReverbBus>,
    room_trim: &mut f32,
) -> bool {
    let mut refader = false;
    for bus in buses.iter_mut() {
        let (gain, balance) = if bus.id == BusId::MASTER {
            (blended.master.gain, blended.master.balance)
        } else if let Some(l) = blended.buses.get(&bus.name) {
            (l.gain, l.balance)
        } else {
            continue;
        };
        if bus.layout.gain != gain {
            bus.layout.gain = gain;
            bus.fader = linear(gain);
            refader = true;
        }
        if bus.layout.balance != balance {
            bus.layout.balance = balance;
            audio.set_bus_pan(bus.id, balance, FADE);
        }
        if let Some(l) = blended.buses.get(&bus.name) {
            for send in &mut bus.sends {
                if let Some(s) = l.sends.get(&send.to)
                    && s.gain != send.gain
                {
                    audio.set_send_gain(send.id, linear(s.gain), FADE);
                    send.gain = s.gain;
                }
            }
        }
        let chain = blended.effects(&bus.name).unwrap_or(&[]);
        for (live, want) in bus.effects.iter_mut().zip(chain) {
            let Some(id) = live.id else {
                continue;
            };
            if let Some(info) = effect_info(&want.kind) {
                for p in info.params {
                    let value = want.param(p.name);
                    if value != live.layout.param(p.name)
                        && let Some(value) = value
                    {
                        audio.set_effect_param(id, p.index, p.effect_value(value));
                        match live.layout.params.get_mut(p.name) {
                            Some(v) => *v = value,
                            None => {
                                live.layout.params.insert(p.name.to_owned(), value);
                            }
                        }
                    }
                }
            }
            if live.layout.bypass != want.bypass {
                let _ = audio.set_effect_bypass(id, want.bypass);
                live.layout.bypass = want.bypass;
            }
        }
    }
    let trim = linear(blended.room.gain);
    if *room_trim != trim {
        *room_trim = trim;
        if let Some(reverb) = reverb {
            audio.set_bus_gain(reverb.bus, trim, FADE);
        }
    }
    refader
}

/// Bus `bus`'s effects laid out as `wanted`, carrying on those `matches` gives (the others
/// are gone already): new ones made, each moved to its place, parameters and bypass set
/// where they changed.
fn lay_effects(
    audio: &mut Audio,
    bus: &mut Bus,
    wanted: &[EffectLayout],
    matches: &[Option<usize>],
) {
    let mut old: Vec<Option<LiveEffect>> = bus.effects.drain(..).map(Some).collect();
    // The chain as the engine has it: what carries on, in its order, then what is new.
    let mut chain: Vec<EffectId> = old.iter().flatten().filter_map(|e| e.id).collect();
    let mut effects = Vec::with_capacity(wanted.len());
    for (i, w) in wanted.iter().enumerate() {
        let carried = matches
            .get(i)
            .copied()
            .flatten()
            .and_then(|j| old.get_mut(j)?.take());
        let effect = match carried {
            Some(mut e) => {
                if let Some(id) = e.id {
                    set_effect(audio, id, &e.layout, w);
                }
                w.clone_into(&mut e.layout);
                e
            }
            None => {
                let id = effect_info(&w.kind).and_then(|info| {
                    match audio.add_boxed_effect(bus.id, info.make()) {
                        Ok(id) => Some(id),
                        Err(e) => {
                            log::warn!(target: "engine_app::sound", "an effect on `{}`: {e}", bus.name);
                            None
                        }
                    }
                });
                if let Some(id) = id {
                    chain.push(id);
                    set_effect(audio, id, &EffectLayout::new(&w.kind), w);
                }
                LiveEffect {
                    layout: w.clone(),
                    id,
                    keyed_by: None,
                }
            }
        };
        effects.push(effect);
    }
    // Each to its place.
    for (p, id) in effects.iter().filter_map(|e| e.id).enumerate() {
        if chain.get(p) != Some(&id)
            && let Some(q) = chain.iter().position(|&c| c == id)
        {
            let _ = audio.move_effect(id, p);
            let moved = chain.remove(q);
            chain.insert(p, moved);
        }
    }
    bus.effects = effects;
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
    zones: QueryState<(Entity, &'static ReverbZone, &'static GlobalTransform)>,
}

/// The room the listener hears now (ADR 0105's acoustics, E11-2c): the room's reverb as it
/// plays, and what it follows; what the editor's mixer shows of its room strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoomHeard {
    /// The reverb's room now: it follows what it hears smoothly.
    pub room: Room,
    /// What it follows.
    pub from: RoomFrom,
    /// The reverb is made: some room was heard since the sound started.
    pub made: bool,
}

/// What the room's reverb follows ([`RoomHeard`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RoomFrom {
    /// Nothing around the listener: no reverb zone, no walls heard (the reverb holds the
    /// last room it had).
    Nothing,
    /// The walls around the listener (rays through the physics world).
    Walls,
    /// A reverb zone: its entity and its weight where the listener is (0..1), over the
    /// walls when they are heard too.
    Zone {
        /// The zone's entity.
        entity: Entity,
        /// Its weight there.
        weight: f32,
        /// The walls are heard under it.
        walls: bool,
    },
}

/// A snapshot's weight (ADR 0122): heard now, where it goes, and how fast (per second).
#[derive(Debug, Clone)]
struct Snap {
    name: String,
    fade: f32,
    current: f32,
    target: f32,
    rate: f32,
}

/// A path the snapshots set: the mix's value, and each snapshot's that sets it (by its
/// place in the list).
#[derive(Debug, Clone)]
struct SnapPath {
    path: String,
    base: f32,
    sets: Vec<(usize, f32)>,
}

/// The room's reverb: a bus with the reverb effect, fed by the reverberant buses' world
/// children.
struct ReverbBus {
    effect: EffectId,
    /// Bus id: the reverbing world child each send comes from.
    sends: Vec<(BusId, SendId)>,
    /// The reverb's own bus.
    bus: BusId,
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
    /// The buses, the master first, then as the layout orders them.
    buses: Vec<Bus>,
    /// The layout changed since the world's [`Mixer`] last followed it.
    layout_changed: bool,
    /// The editor's listening aids (E11-1b): buses it mutes and solos, on top of the
    /// game's mutes, and whether they changed since the mixer was told.
    listen_muted: Vec<String>,
    listen_soloed: Vec<String>,
    listening_changed: bool,
    reverb: Option<ReverbBus>,
    /// The room reverb's fader from the layout (linear), and what the room follows now
    /// (E11-2c).
    room_trim: f32,
    room_from: RoomFrom,
    /// The layout's snapshots (ADR 0122): each one's weight, every path they set with the
    /// mix's value under it, the layout as given and as blended (what was laid out), and
    /// whether a weight moved since the values were given.
    snaps: Vec<Snap>,
    snap_paths: Vec<SnapPath>,
    base: MixerFile,
    blended: MixerFile,
    snaps_moved: bool,
    /// The snapshot the editor hears alone at full weight while it is chosen (E11-5b),
    /// over whatever the game blended.
    heard_alone: Option<String>,
    /// A fader changed under the game's volumes: every bus's gain is given again.
    refader: bool,
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
    /// The editor's hearing in place (E11-4b): while the world stands still, these
    /// entities' sources play where they are, looping, heard from the camera; each from a
    /// copy of its component kept here, so nothing is written back into the world.
    audition: Vec<Entity>,
    audition_plain: Vec<(Entity, AudioSource)>,
    audition_placed: Vec<(Entity, AudioSource3D)>,
    /// The editor's preview of a file (E11-3b), its bus (made with the first), and the
    /// last preview's ticket.
    preview: Option<Preview>,
    preview_bus: Option<BusId>,
    previews: u64,
}

/// A file the editor plays as it is (E11-3b).
struct Preview {
    file: PathBuf,
    ticket: u64,
    /// Where it starts, in seconds, and whether it loops.
    from: f64,
    looping: bool,
    /// Its voice once loaded, its rate, its length in frames when known, and why it did
    /// not play.
    voice: Option<VoiceHandle>,
    rate: u32,
    frames: Option<u64>,
    failed: Option<String>,
}

/// The editor's preview of a sound file as it plays (E11-3b; `Player::sound_preview`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewState<'a> {
    /// The file.
    pub file: &'a Path,
    /// Where it plays now, in seconds from the sound's start (where it starts, while it
    /// loads).
    pub position: f64,
    /// Its length in seconds, when known.
    pub length: Option<f64>,
    /// It is heard now: not loading, ended or failed.
    pub playing: bool,
    /// Why it could not play.
    pub failed: Option<&'a str>,
}

/// Whether the editor's listening aids silence bus `name`: muted, or while a bus is soloed,
/// neither soloed, nor fed by a soloed bus (on its way to the master), nor feeding one.
/// The master never.
fn silenced(buses: &[Bus], muted: &[String], soloed: &[String], name: &str) -> bool {
    if name == "master" {
        return false;
    }
    if muted.iter().any(|m| m == name) {
        return true;
    }
    if soloed.is_empty() {
        return false;
    }
    // Whether `bus` feeds `into`, through its parents.
    let feeds = |bus: &str, into: &str| {
        let mut at = bus;
        for _ in 0..buses.len() {
            match buses
                .iter()
                .find(|b| b.name == at)
                .and_then(|b| b.parent.as_deref())
            {
                Some(p) if p == into => return true,
                Some(p) => at = p,
                None => return false,
            }
        }
        false
    };
    !soloed
        .iter()
        .any(|s| s == name || feeds(s, name) || feeds(name, s))
}

/// The reverb's send from bus `world` forgotten (the bus went, and its sends with it).
fn drop_sends(reverb: &mut Option<ReverbBus>, world: BusId) {
    if let Some(reverb) = reverb {
        reverb.sends.retain(|&(from, _)| from != world);
    }
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
    pub(crate) fn new(settings: &AudioSettings, layout: &MixerFile) -> Self {
        Self::build(settings, layout, false)
    }

    /// Without a device: the caller renders the mixer (tests), the built-in buses.
    #[cfg(test)]
    pub(crate) fn manual(settings: &AudioSettings) -> (Self, Option<Renderer>) {
        Self::manual_with(settings, &MixerFile::builtin())
    }

    /// Without a device, with `layout`'s buses.
    #[cfg(test)]
    pub(crate) fn manual_with(
        settings: &AudioSettings,
        layout: &MixerFile,
    ) -> (Self, Option<Renderer>) {
        let mut sound = Self::build(settings, layout, true);
        let renderer = sound.renderer.take();
        (sound, renderer)
    }

    fn build(settings: &AudioSettings, layout: &MixerFile, manual: bool) -> Self {
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
            layout_changed: false,
            listen_muted: Vec::new(),
            listen_soloed: Vec::new(),
            listening_changed: false,
            reverb: None,
            room_trim: 1.0,
            room_from: RoomFrom::Nothing,
            snaps: Vec::new(),
            snap_paths: Vec::new(),
            base: MixerFile::default(),
            blended: MixerFile::default(),
            snaps_moved: false,
            heard_alone: None,
            refader: false,
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
            audition: Vec::new(),
            audition_plain: Vec::new(),
            audition_placed: Vec::new(),
            preview: None,
            preview_bus: None,
            previews: 0,
        };
        if !settings.enabled {
            return sound;
        }
        let (audio, renderer) = Audio::new(&settings.mixer);
        sound.buses.push(Bus {
            name: String::from("master"),
            id: BusId::MASTER,
            world: None,
            parent: None,
            fader: 1.0,
            layout: BusLayout::new(),
            effects: Vec::new(),
            sends: Vec::new(),
        });
        sound.audio = Some(audio);
        sound.renderer = Some(renderer);
        sound.set_layout(layout);
        sound.layout_changed = false;
        sound
    }

    /// The editor's hearing in place (E11-4b): while the world stands still, `entities`'
    /// sources play where they are, looping, heard from the camera (the zones too; walls and
    /// the room from geometry wait for the game's physics in Play); none: silence again.
    /// Nothing is written back into the world.
    pub(crate) fn set_audition(&mut self, entities: &[Entity]) {
        if self.audition != entities {
            self.audition.clear();
            self.audition.extend_from_slice(entities);
            self.audition_plain.retain(|(e, _)| entities.contains(e));
            self.audition_placed.retain(|(e, _)| entities.contains(e));
        }
    }

    /// The entities heard in place.
    pub(crate) fn audition(&self) -> &[Entity] {
        &self.audition
    }

    /// The editor's listening aids (E11-1b): `muted` buses silent, and while any bus is in
    /// `soloed`, only those, the buses they feed (on to the master) and the buses that feed
    /// them heard. On top of the game's own mutes, which they leave as they are.
    pub(crate) fn set_listening(&mut self, muted: &[String], soloed: &[String]) {
        if self.listen_muted != muted || self.listen_soloed != soloed {
            self.listen_muted = muted.to_vec();
            self.listen_soloed = soloed.to_vec();
            self.listening_changed = true;
        }
    }

    /// Bus `bus`'s peak level per channel since the last call (linear, after its fader):
    /// the editor's meters.
    pub(crate) fn take_peak(&self, bus: &str) -> Option<[f32; 2]> {
        let id = if bus == "reverb" {
            self.reverb.as_ref()?.bus
        } else {
            self.buses.iter().find(|b| b.name == bus)?.id
        };
        Some(self.audio.as_ref()?.take_peak(id))
    }

    /// The room the listener hears now, and what it follows (E11-2c).
    pub(crate) fn room_heard(&self) -> RoomHeard {
        RoomHeard {
            room: self.room,
            from: self.room_from,
            made: self.reverb.is_some(),
        }
    }

    /// How far effect `effect` (its place in the layout's chain) of bus `bus` (`master` for
    /// the master's) lowered the gain since the last call, in dB: the editor's gain
    /// reduction meters.
    pub(crate) fn take_reduction(&self, bus: &str, effect: usize) -> Option<f32> {
        let id = self
            .buses
            .iter()
            .find(|b| b.name == bus)?
            .effects
            .get(effect)?
            .id?;
        Some(self.audio.as_ref()?.take_reduction(id))
    }

    /// The mixer laid out as `layout` says (ADR 0121), at the start and again whenever it
    /// changes: buses it no longer has go (their voices on to the master), new ones are
    /// made, the others moved, faded, balanced and limited where they stand, so what plays
    /// goes on. Effects carry on where the layout keeps them (a reverb's tail rings on),
    /// new ones are made and each is moved to its place; sends and sidechains follow. The
    /// world's [`Mixer`] follows with the next update, keeping the game's volumes of the
    /// buses that stay.
    pub(crate) fn set_layout(&mut self, layout: &MixerFile) {
        for problem in layout.problems() {
            log::warn!(target: "engine_app::sound", "mixer layout: {problem}");
        }
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        let order = layout.order();
        let routes = layout.routes();
        // What carries on of each bus's effects; what does not goes now, with the sends
        // and sidechains the layout no longer has, so no move below loops through them.
        let mut plans: Vec<(String, Vec<Option<usize>>)> = Vec::with_capacity(self.buses.len());
        for bus in &mut self.buses {
            let wanted = layout.effects(&bus.name).unwrap_or(&[]);
            let matches = match_effects(&bus.effects, wanted);
            for (j, effect) in bus.effects.iter_mut().enumerate() {
                let Some(id) = effect.id else {
                    continue;
                };
                match matches.iter().position(|&m| m == Some(j)) {
                    None => {
                        let _ = audio.remove_effect(id);
                        effect.id = None;
                    }
                    Some(i) => {
                        let source = routes
                            .sidechains
                            .iter()
                            .find(|r| r.0 == bus.name && r.1 == i)
                            .map(|r| r.2);
                        if effect.keyed_by.is_some() && effect.keyed_by.as_deref() != source {
                            let _ = audio.set_sidechain(id, None);
                            effect.keyed_by = None;
                        }
                    }
                }
            }
            bus.sends.retain(|s| {
                let keep = routes.sends.iter().any(|(from, to, send)| {
                    *from == bus.name && *to == s.to && send.pre_fader == s.pre_fader
                });
                if !keep {
                    let _ = audio.remove_send(s.id);
                }
                keep
            });
            plans.push((bus.name.clone(), matches));
        }
        // The master's fader and balance.
        self.buses[0].fader = linear(layout.master.gain);
        audio.set_bus_gain(BusId::MASTER, self.buses[0].fader, FADE);
        audio.set_bus_pan(BusId::MASTER, layout.master.balance, FADE);
        self.buses[0].layout.gain = layout.master.gain;
        self.buses[0].layout.balance = layout.master.balance;
        // Buses the layout no longer has, children first (their voices go on).
        let gone: Vec<usize> = (1..self.buses.len())
            .rev()
            .filter(|&k| order.iter().all(|b| b.name != self.buses[k].name))
            .collect();
        for k in gone {
            let bus = self.buses.remove(k);
            if let Some(world) = bus.world {
                let _ = audio.remove_bus(world);
                drop_sends(&mut self.reverb, world);
            }
            let _ = audio.remove_bus(bus.id);
        }
        // Then every bus of the layout, each after the bus it feeds.
        let mut retry: Vec<(BusId, BusId)> = Vec::new();
        let mut made: Vec<Bus> = Vec::with_capacity(order.len() + 1);
        made.push(self.buses.remove(0));
        for placed in &order {
            let parent = placed
                .parent
                .and_then(|p| made.iter().find(|b| b.name == p))
                .map_or(BusId::MASTER, |b| b.id);
            let existing = self
                .buses
                .iter()
                .position(|b| b.name == placed.name)
                .map(|k| self.buses.remove(k));
            let mut bus = match existing {
                Some(mut bus) => {
                    if bus.parent.as_deref() != placed.parent
                        && audio.set_bus_parent(bus.id, parent) == Err(AudioError::Cycle)
                    {
                        retry.push((bus.id, parent));
                    }
                    bus.parent = placed.parent.map(str::to_owned);
                    bus
                }
                None => match audio.add_bus(placed.name, parent) {
                    Ok(id) => Bus {
                        name: placed.name.to_owned(),
                        id,
                        world: None,
                        parent: placed.parent.map(str::to_owned),
                        fader: 1.0,
                        layout: BusLayout::new(),
                        effects: Vec::new(),
                        sends: Vec::new(),
                    },
                    Err(e) => {
                        log::warn!(target: "engine_app::sound", "bus {}: {e}", placed.name);
                        continue;
                    }
                },
            };
            bus.fader = linear(placed.layout.gain);
            audio.set_bus_gain(bus.id, bus.fader, FADE);
            audio.set_bus_pan(bus.id, placed.layout.balance, FADE);
            let limit = placed.layout.voices;
            audio.set_bus_voice_limit(bus.id, (limit > 0).then_some(limit));
            // Its reverberating 3D sounds play on a child: the bus's fader still
            // applies, and its plain sounds stay dry.
            match (placed.layout.reverb, bus.world) {
                (true, None) => {
                    bus.world = audio.add_bus(&format!("{}.world", bus.name), bus.id).ok();
                    if let (Some(world), Some(reverb)) = (bus.world, self.reverb.as_mut())
                        && let Ok(send) =
                            audio.add_send(world, reverb.bus, reverb.sent.level, SendTap::PostFader)
                    {
                        reverb.sends.push((world, send));
                    }
                }
                (false, Some(world)) => {
                    let _ = audio.remove_bus(world);
                    drop_sends(&mut self.reverb, world);
                    bus.world = None;
                }
                _ => {}
            }
            bus.layout = placed.layout.clone();
            made.push(bus);
        }
        self.buses = made;
        if !retry.is_empty() {
            // A move that would loop through a send or a sidechain the layout keeps: they
            // go, the move is made, and they come back below.
            for bus in &mut self.buses {
                for send in bus.sends.drain(..) {
                    let _ = audio.remove_send(send.id);
                }
                for effect in &mut bus.effects {
                    if let (Some(id), Some(_)) = (effect.id, effect.keyed_by.take()) {
                        let _ = audio.set_sidechain(id, None);
                    }
                }
            }
            for (bus, parent) in retry {
                if let Err(e) = audio.set_bus_parent(bus, parent) {
                    log::warn!(target: "engine_app::sound", "mixer layout: a bus not moved: {e}");
                }
            }
        }
        // Each bus's effects.
        for bus in &mut self.buses {
            let wanted = layout.effects(&bus.name).unwrap_or(&[]);
            let matches = plans
                .iter()
                .find(|(name, _)| *name == bus.name)
                .map_or(&[][..], |(_, m)| m.as_slice());
            lay_effects(audio, bus, wanted, matches);
        }
        // The sends, coming in over a fade.
        for &(from, to, send) in &routes.sends {
            let Some(target) = self.buses.iter().find(|b| b.name == to).map(|b| b.id) else {
                continue;
            };
            let Some(bus) = self.buses.iter_mut().find(|b| b.name == from) else {
                continue;
            };
            let live = bus
                .sends
                .iter_mut()
                .find(|s| s.to == to && s.pre_fader == send.pre_fader);
            match live {
                Some(live) => {
                    if live.gain != send.gain {
                        audio.set_send_gain(live.id, linear(send.gain), FADE);
                        live.gain = send.gain;
                    }
                }
                None => {
                    let tap = if send.pre_fader {
                        SendTap::PreFader
                    } else {
                        SendTap::PostFader
                    };
                    match audio.add_send(bus.id, target, 0.0, tap) {
                        Ok(id) => {
                            audio.set_send_gain(id, linear(send.gain), FADE);
                            bus.sends.push(LiveSend {
                                to: to.to_owned(),
                                id,
                                pre_fader: send.pre_fader,
                                gain: send.gain,
                            });
                        }
                        Err(e) => {
                            log::warn!(target: "engine_app::sound", "mixer layout: `{from}`'s send into `{to}`: {e}");
                        }
                    }
                }
            }
        }
        // The sidechains.
        for &(keyed, k, source) in &routes.sidechains {
            let Some(source_id) = self.buses.iter().find(|b| b.name == source).map(|b| b.id) else {
                continue;
            };
            let Some(effect) = self
                .buses
                .iter_mut()
                .find(|b| b.name == keyed)
                .and_then(|b| b.effects.get_mut(k))
            else {
                continue;
            };
            if let Some(id) = effect.id
                && effect.keyed_by.as_deref() != Some(source)
            {
                match audio.set_sidechain(id, Some(source_id)) {
                    Ok(()) => effect.keyed_by = Some(source.to_owned()),
                    Err(e) => {
                        log::warn!(target: "engine_app::sound", "mixer layout: the sidechain `{source}` on `{keyed}`: {e}");
                    }
                }
            }
        }
        // The room's reverb's fader (version 3).
        self.room_trim = linear(layout.room.gain);
        if let Some(reverb) = &self.reverb {
            audio.set_bus_gain(reverb.bus, self.room_trim, 0.05);
        }
        self.lay_snapshots(layout);
        self.layout_changed = true;
    }

    /// The layout's snapshots laid out (ADR 0122): each keeps its weight by its name, and
    /// every path they set is found with the mix's value under it; one that names nothing
    /// is left out (the layout's problems say so). Blended again at the next update.
    fn lay_snapshots(&mut self, layout: &MixerFile) {
        let old = core::mem::take(&mut self.snaps);
        self.snaps = layout
            .snapshots
            .iter()
            .map(|s| {
                let kept = old.iter().find(|o| o.name == s.name);
                Snap {
                    name: s.name.clone(),
                    fade: s.fade.max(0.0),
                    current: kept.map_or(0.0, |o| o.current),
                    target: kept.map_or(0.0, |o| o.target),
                    rate: kept.map_or(0.0, |o| o.rate),
                }
            })
            .collect();
        self.snap_paths.clear();
        for (k, s) in layout.snapshots.iter().enumerate() {
            for (path, &value) in &s.values {
                let Some(base) = snapshot_target(path).and_then(|t| layout.value_at(t)) else {
                    continue;
                };
                match self.snap_paths.iter_mut().find(|p| p.path == *path) {
                    Some(p) => p.sets.push((k, value)),
                    None => self.snap_paths.push(SnapPath {
                        path: path.clone(),
                        base,
                        sets: vec![(k, value)],
                    }),
                }
            }
        }
        self.base.clone_from(layout);
        self.blended.clone_from(layout);
        self.snaps_moved = true;
    }

    /// The game's blends taken, the weights moved over `dt`, and when one moved, every
    /// value the snapshots set blended up their list and given where it changed (ADR 0122).
    fn blend_snapshots(&mut self, mixer: &mut Mixer, dt: f32) {
        for asked in &mut mixer.snapshots {
            let Some((weight, seconds)) = asked.asked.take() else {
                continue;
            };
            let Some(s) = self.snaps.iter_mut().find(|s| s.name == asked.name) else {
                continue;
            };
            let seconds = seconds.unwrap_or(s.fade);
            s.target = weight;
            s.rate = if seconds > 0.0 {
                (weight - s.current).abs() / seconds
            } else {
                f32::INFINITY
            };
        }
        for s in &mut self.snaps {
            if s.current == s.target {
                continue;
            }
            let step = s.rate * dt;
            s.current = if step >= (s.target - s.current).abs() {
                s.target
            } else {
                s.current + step.copysign(s.target - s.current)
            };
            self.snaps_moved = true;
        }
        if !core::mem::take(&mut self.snaps_moved) {
            return;
        }
        // The editor's chosen snapshot is heard alone, at full weight (E11-5b).
        let alone = self.heard_alone.as_deref();
        let weight = |s: &Snap| match alone {
            Some(name) if s.name == name => 1.0,
            Some(_) => 0.0,
            None => s.current,
        };
        for p in &self.snap_paths {
            let Some(target) = snapshot_target(&p.path) else {
                continue;
            };
            let mut v = p.base;
            for &(k, x) in &p.sets {
                let w = self.snaps.get(k).map_or(0.0, weight);
                if w > 0.0 {
                    v += (x - v) * w;
                }
            }
            self.blended.set_value(target, v);
        }
        if let Some(audio) = self.audio.as_mut() {
            self.refader |= give_blended(
                audio,
                &mut self.buses,
                &self.blended,
                self.reverb.as_ref(),
                &mut self.room_trim,
            );
        }
        for (shown, s) in mixer.snapshots.iter_mut().zip(&self.snaps) {
            shown.weight = weight(s);
        }
    }

    /// The editor's chosen snapshot heard alone at full weight over the mix (E11-5b); none:
    /// the game's weights again.
    pub(crate) fn hear_snapshot(&mut self, name: Option<&str>) {
        if self.heard_alone.as_deref() != name {
            self.heard_alone = name.map(str::to_owned);
            self.snaps_moved = true;
        }
    }

    /// The world's [`Mixer`] for these buses: the game's volumes at 1, on top of the
    /// layout's faders.
    pub(crate) fn mixer(&self) -> Mixer {
        Mixer {
            buses: self
                .buses
                .iter()
                .map(|b| MixerBus {
                    name: b.name.clone(),
                    id: b.id,
                    volume: 1.0,
                    muted: false,
                    changed: false,
                })
                .collect(),
            snapshots: self
                .snaps
                .iter()
                .map(|s| MixerSnapshot {
                    name: s.name.clone(),
                    weight: s.current,
                    asked: None,
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
        self.apply_mixer(world, dt as f32);
        // The world kept still (the editor while editing, a pause): its sources are silent,
        // and where each one is stays as it was; but those heard in place play.
        if !simulating {
            if self.audition.is_empty() {
                if let Some(audio) = self.audio.as_mut() {
                    self.plain.clear(audio);
                    self.placed.clear(audio);
                }
                return;
            }
            self.make_queries(world);
            let live = self.manual || self.output.is_some();
            // The camera hears, whatever listener the scene has.
            let listener = self.listen(world, camera, dt, live, true);
            self.stamp = self.stamp.wrapping_add(1);
            if self.follow_audition(world, dt, live) {
                self.want_output();
            }
            if let Some(audio) = self.audio.as_mut() {
                self.plain.drop_unseen(self.stamp, audio);
                self.placed.drop_unseen(self.stamp, audio);
            }
            if live {
                self.hear_the_world(world, listener, None, dt);
            }
            return;
        }
        self.make_queries(world);
        let live = self.manual || self.output.is_some();
        let listener = self.listen(world, camera, dt, live, false);
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
                Done::Preview { ticket, result } => self.start_preview(ticket, result),
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

    /// The game's changes to the world's [`Mixer`] (its volumes, mutes, absorption and
    /// snapshots' blends over `dt` seconds), and the error into it.
    fn apply_mixer(&mut self, world: &mut World, dt: f32) {
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
        // A new layout: the buses that stay keep the game's volumes, new ones start at 1,
        // and every bus's gain is its fader again under them.
        // The editor's listening aids changed: every bus's mute again.
        let listening = core::mem::take(&mut self.listening_changed);
        let laid_out = self.layout_changed;
        if laid_out {
            // The layout's snapshots, each keeping what was asked of it by its name.
            let old = core::mem::take(&mut mixer.snapshots);
            mixer.snapshots = self
                .snaps
                .iter()
                .map(|s| MixerSnapshot {
                    name: s.name.clone(),
                    weight: s.current,
                    asked: old.iter().find(|o| o.name == s.name).and_then(|o| o.asked),
                })
                .collect();
        }
        self.blend_snapshots(mixer, dt);
        let refader = core::mem::take(&mut self.refader);
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        if core::mem::take(&mut self.layout_changed) {
            let old = core::mem::take(&mut mixer.buses);
            mixer.buses = self
                .buses
                .iter()
                .map(|b| {
                    let kept = old.iter().find(|m| m.name == b.name);
                    MixerBus {
                        name: b.name.clone(),
                        id: b.id,
                        volume: kept.map_or(1.0, |m| m.volume),
                        muted: kept.is_some_and(|m| m.muted),
                        changed: true,
                    }
                })
                .collect();
        }
        for bus in &mut mixer.buses {
            if core::mem::take(&mut bus.changed) || listening || refader {
                let fader = self
                    .buses
                    .iter()
                    .find(|b| b.id == bus.id)
                    .map_or(1.0, |b| b.fader);
                audio.set_bus_gain(bus.id, fader * bus.volume, 0.05);
                let aids = silenced(
                    &self.buses,
                    &self.listen_muted,
                    &self.listen_soloed,
                    &bus.name,
                );
                audio.set_bus_mute(bus.id, bus.muted || aids);
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
        camera_hears: bool,
    ) -> Option<Vec3> {
        let queries = self.queries.as_mut()?;
        let mut found: Option<Affine3A> = None;
        if !camera_hears {
            queries.listeners.for_each_readonly(world, |(_, global)| {
                if found.is_none() {
                    found = Some(global.0);
                }
            });
        }
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

    /// The queries the frame reads, made once.
    fn make_queries(&mut self, world: &mut World) {
        if self.queries.is_none() {
            self.queries = Some(Queries {
                plain: QueryState::new(world),
                placed: QueryState::new(world),
                listeners: QueryState::new(world),
                zones: QueryState::new(world),
            });
        }
    }

    /// The sources heard in place (E11-4b), each from its copy: the component's settings
    /// every frame, playing and looping where the copy has got to; the world itself is
    /// only read. Whether any wants to play.
    fn follow_audition(&mut self, world: &World, dt: f64, live: bool) -> bool {
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
            audition,
            audition_plain,
            audition_placed,
            ..
        } = self;
        let Some(audio) = audio.as_mut() else {
            return false;
        };
        // A copy loops, so none ends by itself; and none would despawn anything.
        let mut nothing = Vec::new();
        let mut ctx = Ctx {
            audio,
            live,
            buses,
            loader,
            sounds,
            tickets,
            prepared,
            despawn: &mut nothing,
            stream_from: settings.stream_from,
            stamp: *stamp,
            dt,
            wanted: false,
        };
        // Where the copy has got to stays its own; the rest follows the component.
        fn heard(copy: &mut AudioSource, from: &AudioSource) {
            let at = copy.position;
            copy.clone_from(from);
            copy.position = at;
            copy.playing = true;
            copy.mode = PlayMode::Loop;
        }
        for &entity in audition.iter() {
            if let Some(source) = world.get::<AudioSource>(entity) {
                let k = match audition_plain.iter().position(|(e, _)| *e == entity) {
                    Some(k) => k,
                    None => {
                        let mut copy = source.clone();
                        copy.position = 0.0;
                        audition_plain.push((entity, copy));
                        audition_plain.len() - 1
                    }
                };
                let copy = &mut audition_plain[k].1;
                heard(copy, source);
                let track = plain.get_or_insert(entity, copy.position);
                follow(&mut ctx, track, entity, copy, None);
            } else if let Some(source) = world.get::<AudioSource3D>(entity) {
                let k = match audition_placed.iter().position(|(e, _)| *e == entity) {
                    Some(k) => k,
                    None => {
                        let mut copy = source.clone();
                        copy.source.position = 0.0;
                        audition_placed.push((entity, copy));
                        audition_placed.len() - 1
                    }
                };
                let copy = &mut audition_placed[k].1;
                let at = copy.source.position;
                copy.clone_from(source);
                copy.source.position = at;
                copy.source.playing = true;
                copy.source.mode = PlayMode::Loop;
                let global = world.get::<GlobalTransform>(entity);
                let placement = Placement::of(copy, global.map(GlobalTransform::translation));
                let track = placed.get_or_insert(entity, copy.source.position);
                follow(&mut ctx, track, entity, &mut copy.source, Some(placement));
            }
        }
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
        self.room_from = match (zone, geometry.is_some()) {
            (Some((_, weight, entity)), walls) => RoomFrom::Zone {
                entity,
                weight,
                walls,
            },
            (None, true) => RoomFrom::Walls,
            (None, false) => RoomFrom::Nothing,
        };
        if geometry.is_none() && zone.is_none() && self.reverb.is_none() {
            return;
        }
        let base = geometry.unwrap_or(Room {
            level: 0.0,
            ..self.room
        });
        let target = match zone {
            Some((room, weight, _)) => base.lerp(&room, weight),
            None => base,
        };
        let follow = (1.0 - (-dt / ROOM_FOLLOW).exp()) as f32;
        self.room = self.room.lerp(&target, follow);
        self.send_room();
    }

    /// The zone the listener at `at` is in: its room and weight (the highest priority,
    /// then the strongest).
    fn zone(&mut self, world: &World, at: Vec3) -> Option<(Room, f32, Entity)> {
        let queries = self.queries.as_mut()?;
        let mut best: Option<(i32, f32, Room, Entity)> = None;
        queries
            .zones
            .for_each_readonly(world, |(entity, zone, global)| {
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
                let better = best.as_ref().is_none_or(|&(priority, w, _, _)| {
                    zone.priority > priority || (zone.priority == priority && weight > w)
                });
                if better {
                    best = Some((zone.priority, weight, zone.room, entity));
                }
            });
        best.map(|(_, weight, room, entity)| (room, weight, entity))
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
            audio.set_bus_gain(bus, self.room_trim, 0.0);
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
                    let send = audio
                        .add_send(world, bus, room.level, SendTap::PostFader)
                        .ok()?;
                    Some((world, send))
                })
                .collect();
            self.reverb = Some(ReverbBus {
                bus,
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
            for &(_, send) in &reverb.sends {
                audio.set_send_gain(send, room.level, 0.1);
            }
            sent.level = room.level;
        }
    }

    /// A save was loaded: every voice stops, and each playing source starts again where the
    /// save had it (from the start when it asks to).
    /// Plays `file` (an absolute path) as it is, from `from` seconds, looping or not: the
    /// editor's preview (E11-3b). On a bus of its own straight into the master, dry; heard
    /// while the world stands still; one at a time, a new one stopping the last with the
    /// usual fade. The file is read and decoded on the loading thread (a long one opened as
    /// a stream where it starts), so the frame never waits.
    pub(crate) fn preview(&mut self, file: &Path, from: f64, looping: bool) {
        self.stop_preview();
        if self.audio.is_none() {
            return;
        }
        self.previews += 1;
        let ticket = self.previews;
        let buffer_ms = self.audio.as_ref().map_or(0, Audio::stream_buffer_ms);
        let sent = self.loader.send(Request::Preview {
            ticket,
            path: file.to_owned(),
            stream_from: self.settings.stream_from,
            from,
            looping,
            buffer_ms,
        });
        self.want_output();
        self.preview = Some(Preview {
            file: file.to_owned(),
            ticket,
            from,
            looping,
            voice: None,
            rate: 0,
            frames: None,
            failed: (!sent).then(|| String::from("the sounds' loading thread is not running")),
        });
    }

    /// The preview stopped, faded out.
    pub(crate) fn stop_preview(&mut self) {
        if let Some(p) = self.preview.take()
            && let (Some(voice), Some(audio)) = (p.voice, self.audio.as_mut())
        {
            audio.stop(voice, FADE);
        }
    }

    /// The preview as it plays, if there is one.
    pub(crate) fn preview_state(&self) -> Option<PreviewState<'_>> {
        let p = self.preview.as_ref()?;
        let rate = f64::from(p.rate.max(1));
        let at = p
            .voice
            .and_then(|voice| self.audio.as_ref()?.position(voice));
        Some(PreviewState {
            file: &p.file,
            position: at.map_or(p.from, |frames| frames as f64 / rate),
            length: p.frames.map(|frames| frames as f64 / rate),
            playing: at.is_some(),
            failed: p.failed.as_deref(),
        })
    }

    /// The loader's answer for preview `ticket` (a later preview's tickets win): its voice
    /// started on the preview bus.
    fn start_preview(&mut self, ticket: u64, result: Result<PreviewSound, String>) {
        let Some(p) = self.preview.as_mut().filter(|p| p.ticket == ticket) else {
            return;
        };
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        let bus = match self.preview_bus {
            Some(bus) => bus,
            None => match audio.add_bus("editor preview", BusId::MASTER) {
                Ok(bus) => *self.preview_bus.insert(bus),
                Err(e) => {
                    p.failed = Some(e.to_string());
                    return;
                }
            },
        };
        let params = |offset: u64| PlayParams {
            bus,
            looping: p.looping,
            offset,
            ..PlayParams::default()
        };
        let started = match result {
            Ok(PreviewSound::Clip(clip)) => {
                p.rate = clip.sample_rate();
                p.frames = Some(clip.frames() as u64);
                let offset = (p.from.max(0.0) * f64::from(p.rate)) as u64;
                audio.play_with(&clip, &params(offset))
            }
            Ok(PreviewSound::Stream {
                stream,
                rate,
                frames,
            }) => {
                p.rate = rate;
                p.frames = frames;
                audio.play_prepared(stream, &params(0))
            }
            Err(why) => {
                log::warn!(target: "engine_app::sound", "preview: {why}");
                p.failed = Some(why);
                return;
            }
        };
        match started {
            Ok(voice) => p.voice = Some(voice),
            Err(e) => p.failed = Some(e.to_string()),
        }
    }

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
