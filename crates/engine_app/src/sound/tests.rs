#![allow(clippy::unwrap_used, clippy::float_cmp)]

use super::*;
use engine_ecs::Transform;
use engine_physics::{BodyDesc, PhysicsSettings, ShapeDesc};
use engine_physics_jolt::JoltBackend;
use std::path::Path;

const RATE: u32 = 48_000;
/// A mono clip at 48 kHz panned to the centre comes out at this gain on each side.
const CENTRE: f32 = core::f32::consts::FRAC_1_SQRT_2;

/// Frame `i` of the test tone.
fn value(i: u64) -> f32 {
    (i % 20_000) as f32 / 40_000.0 + 0.01
}

/// A mono 32-bit float WAV file of `frames` frames of [`value`].
fn write_wav(path: &Path, frames: u64) {
    write_wav_with(path, frames, 0.0);
}

/// A mono 32-bit float WAV file of `frames` frames of [`value`] plus `lift`.
fn write_wav_with(path: &Path, frames: u64, lift: f32) {
    let data = (frames * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&RATE.to_le_bytes());
    bytes.extend_from_slice(&(RATE * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for i in 0..frames {
        bytes.extend_from_slice(&(value(i) + lift).to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

/// Real time, for the loading thread.
#[allow(clippy::disallowed_methods)]
fn wait_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// A world, the sound system without a device, and a folder of sounds.
struct Bed {
    sound: Sound,
    renderer: Renderer,
    world: World,
    camera: ViewCamera,
    folder: PathBuf,
    physics: Option<JoltBackend>,
    /// The world's time runs (`Player::set_simulating`).
    simulating: bool,
}

impl Bed {
    fn new(name: &str, settings: AudioSettings) -> Self {
        let folder =
            std::env::temp_dir().join(format!("ofelia-sound-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let settings = AudioSettings {
            folder: if settings.folder == Path::new("sounds") {
                folder.clone()
            } else {
                settings.folder.clone()
            },
            ..settings
        };
        let (sound, renderer) = Sound::manual(&settings);
        let mut world = World::new();
        world.insert_resource(sound.mixer());
        Self {
            sound,
            renderer: renderer.unwrap(),
            world,
            camera: ViewCamera::from_rigid(Quat::IDENTITY, Vec3::ZERO, 1.0, 0.1, 100.0),
            folder,
            physics: None,
            simulating: true,
        }
    }

    /// One frame `ms` long: the sound's update, then `ms` of the mix (stereo).
    fn frame(&mut self, ms: u32) -> Vec<f32> {
        let physics = self.physics.as_ref().map(|p| p as &dyn PhysicsBackend);
        self.sound.update(
            &mut self.world,
            Some(&self.camera),
            physics,
            f64::from(ms) / 1e3,
            self.simulating,
        );
        let mut out = vec![0.0; (RATE * ms / 1000) as usize * 2];
        self.renderer.render(&mut out, 2);
        out
    }

    /// Updates every 5 ms, mixing nothing, until `done` (the loading thread runs in real
    /// time), at most 3 s: a voice started is then where it starts.
    fn until(&mut self, mut done: impl FnMut(&mut Self) -> bool) {
        for _ in 0..600 {
            let physics = self.physics.as_ref().map(|p| p as &dyn PhysicsBackend);
            self.sound.update(
                &mut self.world,
                Some(&self.camera),
                physics,
                0.005,
                self.simulating,
            );
            if done(self) {
                return;
            }
            wait_ms(5);
        }
        panic!("timed out");
    }

    fn track(&self, entity: Entity) -> Option<&Track> {
        self.sound
            .plain
            .list
            .iter()
            .chain(&self.sound.placed.list)
            .find(|t| t.entity == entity)
    }

    fn voice(&self, entity: Entity) -> Option<VoiceHandle> {
        self.track(entity).and_then(|t| t.voice)
    }

    fn source(&self, entity: Entity) -> &AudioSource {
        self.world.get::<AudioSource>(entity).unwrap()
    }

    fn source_mut(&mut self, entity: Entity) -> &mut AudioSource {
        self.world.get_mut::<AudioSource>(entity).unwrap()
    }

    fn audio(&self) -> &Audio {
        self.sound.audio.as_ref().unwrap()
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

fn left(out: &[f32]) -> Vec<f32> {
    out.chunks_exact(2).map(|f| f[0]).collect()
}

fn energy(samples: impl Iterator<Item = f32>) -> f32 {
    samples.map(|s| s * s).sum()
}

#[test]
fn a_source_plays_its_sound_and_writes_back_where_it_is() {
    let mut bed = Bed::new("plays", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    let e = bed
        .world
        .spawn((AudioSource::new("tone.wav").on_bus("music"),));
    bed.until(|b| b.voice(e).is_some());
    // The voice started at the last update; this frame's mix is its first 10 ms.
    let out = bed.frame(10);
    for (i, &s) in left(&out).iter().enumerate() {
        assert!(
            (s - value(i as u64) * CENTRE).abs() < 1e-6,
            "frame {i}: {s}"
        );
    }
    // Each update reads where the mixer is: 10 ms in, then 20.
    bed.frame(10);
    assert!(
        (bed.source(e).position - 0.01).abs() < 1e-9,
        "{}",
        bed.source(e).position
    );
    bed.frame(10);
    assert!((bed.source(e).position - 0.02).abs() < 1e-9);
}

#[test]
fn a_still_world_keeps_its_sources_silent_where_they_are() {
    let mut bed = Bed::new("still", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    write_wav(&bed.folder.join("blip.wav"), 960);
    let e = bed.world.spawn((AudioSource::new("tone.wav"),));
    let gone = bed
        .world
        .spawn((AudioSource::new("blip.wav").despawned_at_end(),));
    bed.until(|b| b.voice(e).is_some() && b.voice(gone).is_some());
    bed.frame(10);
    bed.frame(10);
    let at = bed.source(e).position;
    assert!(at > 0.0);
    // The editor while editing, a pause: silent after the voices' fade (`FADE`, 20 ms),
    // nothing moves on or despawns.
    bed.simulating = false;
    bed.frame(30);
    for _ in 0..5 {
        let out = bed.frame(10);
        assert_eq!(energy(out.into_iter()), 0.0);
    }
    assert!(bed.voice(e).is_none());
    assert_eq!(bed.source(e).position, at);
    assert!(bed.source(e).playing && bed.world.contains(gone));
    // Running again: on from where it was.
    bed.simulating = true;
    bed.until(|b| b.voice(e).is_some());
    bed.frame(10);
    bed.frame(10);
    assert!(bed.source(e).position > at, "{}", bed.source(e).position);
}

#[test]
fn a_one_shot_stops_or_despawns_at_its_end() {
    let mut bed = Bed::new("ends", AudioSettings::default());
    write_wav(&bed.folder.join("blip.wav"), 960);
    let once = bed.world.spawn((AudioSource::new("blip.wav"),));
    let gone = bed
        .world
        .spawn((AudioSource::new("blip.wav").despawned_at_end(),));
    bed.until(|b| b.voice(once).is_some() && b.voice(gone).is_some());
    bed.frame(30);
    bed.frame(10);
    let source = bed.source(once);
    assert!(!source.playing);
    assert_eq!(source.position, 0.0);
    assert!(!bed.world.contains(gone));
    // Played again when asked.
    bed.source_mut(once).playing = true;
    bed.frame(5);
    assert!(bed.voice(once).is_some());
}

#[test]
fn a_paused_sound_goes_on_from_where_it_stopped_and_a_seek_moves_it() {
    let mut bed = Bed::new("pause", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    let e = bed
        .world
        .spawn((AudioSource::new("tone.wav").on_bus("music"),));
    bed.until(|b| b.voice(e).is_some());
    bed.frame(100);
    bed.source_mut(e).playing = false;
    bed.frame(40);
    let paused_at = bed.source(e).position;
    assert!((paused_at - 0.1).abs() < 1e-9, "{paused_at}");
    assert!(bed.voice(e).is_none());
    // The stopped voice has faded out by now: what plays is the resumed one alone.
    bed.source_mut(e).playing = true;
    let out = bed.frame(10);
    let from = (paused_at * f64::from(RATE)).round() as u64;
    for (i, &s) in left(&out).iter().enumerate() {
        assert!(
            (s - value(from + i as u64) * CENTRE).abs() < 1e-6,
            "frame {i}: {s}"
        );
    }
    // A seek: the game writes the position.
    bed.source_mut(e).position = 0.5;
    bed.frame(10);
    bed.frame(10);
    assert!(
        (bed.source(e).position - 0.51).abs() < 1e-9,
        "{}",
        bed.source(e).position
    );
}

#[test]
fn a_load_resumes_every_sound_where_the_save_had_it() {
    let mut bed = Bed::new("load", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    let music = bed.world.spawn((
        engine_save::SceneId::named("music"),
        AudioSource::new("tone.wav").on_bus("music").looped(),
    ));
    let jingle = bed.world.spawn((
        engine_save::SceneId::named("jingle"),
        AudioSource {
            restart_on_load: true,
            ..AudioSource::new("tone.wav")
        },
    ));
    bed.until(|b| b.voice(music).is_some() && b.voice(jingle).is_some());
    bed.frame(200);
    bed.frame(1);
    let saved_at = bed.source(music).position;
    assert!((saved_at - 0.2).abs() < 1e-9, "{saved_at}");
    let image = engine_save::capture(&bed.world, &engine_save::CaptureOptions::default());
    let path = bed.folder.join("slot.sav");
    engine_save::write_save(
        &path,
        &image,
        engine_save::Encoding::Text,
        &engine_save::SaveHeader::default(),
    )
    .unwrap();
    // Play on, then load.
    bed.frame(300);
    let (_, save) = engine_save::read_save(&path).unwrap();
    engine_save::apply(&mut bed.world, &save).unwrap();
    bed.sound.after_load(&mut bed.world);
    assert!(bed.sound.plain.list.is_empty());
    assert_eq!(bed.source(music).position, saved_at);
    assert_eq!(bed.source(jingle).position, 0.0);
    // The next frame starts them again: the music where it was, the jingle from the start.
    bed.frame(10);
    let music_voice = bed.voice(music).unwrap();
    let jingle_voice = bed.voice(jingle).unwrap();
    bed.frame(10);
    // Two frames of 10 ms mixed since.
    let at = |v| bed.audio().position(v).unwrap();
    assert_eq!(
        at(music_voice),
        (saved_at * f64::from(RATE)).round() as u64 + 960
    );
    assert_eq!(at(jingle_voice), 960);
}

#[test]
fn a_despawned_source_stops() {
    let mut bed = Bed::new("despawn", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    let e = bed.world.spawn((AudioSource::new("tone.wav").looped(),));
    bed.until(|b| b.voice(e).is_some());
    bed.frame(20);
    assert_eq!(bed.audio().voice_counts().0, 1);
    bed.world.despawn(e);
    bed.frame(40);
    bed.frame(5);
    assert!(bed.sound.plain.list.is_empty());
    assert_eq!(bed.audio().voice_counts(), (0, 0));
}

#[test]
fn a_long_sound_streams_from_its_offset() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine_audio_decode/tests/data");
    let mut bed = Bed::new(
        "stream",
        AudioSettings {
            folder: fixtures,
            stream_from: 0.0,
            ..AudioSettings::default()
        },
    );
    let e = bed.world.spawn((AudioSource {
        position: 0.05,
        ..AudioSource::new("stereo_44100.ogg").on_bus("music")
    },));
    bed.until(|b| b.voice(e).is_some());
    assert!(matches!(
        bed.sound.sounds.entries[bed.track(e).unwrap().sound.unwrap() as usize].state,
        State::Long { rate: 44_100 }
    ));
    for _ in 0..10 {
        bed.frame(10);
    }
    let at = bed.source(e).position;
    // 50 ms in, then 90 ms of mix read back (the last frame's after this update).
    assert!((at - 0.14).abs() < 0.001, "{at}");
    assert_eq!(bed.audio().stream_underruns(), 0);
}

#[test]
fn a_placed_sound_is_heard_from_its_side() {
    let mut bed = Bed::new("spatial", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    let e = bed.world.spawn((
        GlobalTransform(Affine3A::from_translation(Vec3::new(3.0, 0.0, 0.0))),
        AudioSource3D::new("tone.wav").within(1.0, 50.0),
    ));
    bed.until(|b| b.voice(e).is_some());
    let out = bed.frame(20);
    let (l, r) = (
        energy(out.chunks_exact(2).map(|f| f[0])),
        energy(out.chunks_exact(2).map(|f| f[1])),
    );
    assert!(r > 4.0 * l, "left {l}, right {r}");
}

#[test]
fn the_mixer_sets_a_buses_volume() {
    let mut bed = Bed::new("mixer", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    let e = bed
        .world
        .spawn((AudioSource::new("tone.wav").on_bus("music").looped(),));
    bed.until(|b| b.voice(e).is_some());
    assert!(energy(bed.frame(10).into_iter()) > 0.0);
    let mixer = bed.world.resource_mut::<Mixer>().unwrap();
    assert_eq!(mixer.volume("music"), Some(1.0));
    assert!(mixer.set_volume("music", 0.0));
    assert!(!mixer.set_volume("no such bus", 0.5));
    bed.frame(100);
    assert_eq!(energy(bed.frame(10).into_iter()), 0.0);
}

/// A mono 32-bit float WAV file of `frames` frames, every one `v`: its energy tells gains
/// apart wherever it plays.
fn write_level_wav(path: &Path, frames: u64, v: f32) {
    let data = (frames * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&RATE.to_le_bytes());
    bytes.extend_from_slice(&(RATE * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for _ in 0..frames {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn a_new_layout_moves_fades_and_drops_buses_while_its_sounds_play() {
    let mut bed = Bed::new("layout", AudioSettings::default());
    write_level_wav(&bed.folder.join("level.wav"), u64::from(RATE), 0.25);
    let e = bed
        .world
        .spawn((AudioSource::new("level.wav").on_bus("music").looped(),));
    bed.until(|b| b.voice(e).is_some());
    // The energy of 20 ms once the gains' ramps are over.
    let level = |bed: &mut Bed| {
        bed.frame(120);
        energy(bed.frame(20).into_iter())
    };
    let full = level(&mut bed);
    assert!(full > 0.0);
    // Music under a new world bus at -6 dB, a new steps bus under effects, voice and ui
    // gone.
    let mut layout = MixerFile::default();
    layout.buses.insert(String::from("world"), BusLayout::new());
    layout.buses.insert(
        String::from("music"),
        BusLayout::new().under("world").at(-6.0),
    );
    layout
        .buses
        .insert(String::from("effects"), BusLayout::new().reverberant());
    layout
        .buses
        .insert(String::from("steps"), BusLayout::new().under("effects"));
    bed.sound.set_layout(&layout);
    let names: Vec<&str> = bed.sound.buses.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["master", "effects", "steps", "world", "music"]);
    // Its sound goes on, at a quarter of the power.
    let fader = level(&mut bed);
    assert!(bed.voice(e).is_some(), "still playing");
    assert!((fader / full - 0.251).abs() < 0.01, "{fader} of {full}");
    let world = bed.sound.buses[3].id;
    let music = &bed.sound.buses[4];
    assert_eq!(music.parent.as_deref(), Some("world"));
    assert_ne!(music.id, world);
    // The world's Mixer follows the buses; the game's volume multiplies the fader.
    let mixer = bed.world.resource_mut::<Mixer>().unwrap();
    assert_eq!(
        mixer.buses().collect::<Vec<_>>(),
        ["master", "effects", "steps", "world", "music"]
    );
    assert_eq!(mixer.volume("music"), Some(1.0));
    assert!(mixer.set_volume("music", 0.5));
    assert!(mixer.volume("voice").is_none(), "gone");
    let both = level(&mut bed);
    assert!((both / full - 0.0627).abs() < 0.005, "{both} of {full}");
    // Effects keeps its world child; a bus made reverberant gets one, one made dry loses
    // it; the game's volume stays across a new layout.
    assert!(bed.sound.buses[1].world.is_some());
    layout.buses.get_mut("music").unwrap().reverb = true;
    layout.buses.get_mut("effects").unwrap().reverb = false;
    bed.sound.set_layout(&layout);
    assert!(bed.sound.buses[4].world.is_some());
    assert!(bed.sound.buses[1].world.is_none());
    let again = level(&mut bed);
    assert!((again / both - 1.0).abs() < 0.02, "{again} after {both}");
    assert_eq!(
        bed.world.resource::<Mixer>().unwrap().volume("music"),
        Some(0.5)
    );
}

/// Bus `name` of the bed's mixer as made.
fn live_bus<'a>(bed: &'a Bed, name: &str) -> &'a super::Bus {
    bed.sound.buses.iter().find(|b| b.name == name).unwrap()
}

/// An effect of `kind` with parameters `params` (the rest at their defaults).
fn effect(kind: &str, params: &[(&str, f32)]) -> EffectLayout {
    let mut e = EffectLayout::new(kind);
    for (name, value) in params {
        e.params.insert((*name).to_owned(), *value);
    }
    e
}

#[test]
fn effects_follow_the_layout_and_carry_on_where_it_keeps_them() {
    let mut bed = Bed::new("effects", AudioSettings::default());
    write_level_wav(&bed.folder.join("level.wav"), u64::from(RATE), 0.25);
    let e = bed
        .world
        .spawn((AudioSource::new("level.wav").on_bus("music").looped(),));
    bed.until(|b| b.voice(e).is_some());
    // The energy of 20 ms once the ramps are over and a compressor carried on has let go
    // of what it took before (its release is 120 ms).
    let level = |bed: &mut Bed| {
        bed.frame(600);
        energy(bed.frame(20).into_iter())
    };
    let full = level(&mut bed);
    // A low shelf 6 dB down: the level (a constant, all low) at a quarter of the power.
    let shelf = effect("low_shelf", &[("gain_db", -6.0)]);
    let mut layout = MixerFile::builtin();
    layout.buses.get_mut("music").unwrap().effects = vec![shelf.clone()];
    bed.sound.set_layout(&layout);
    let quarter = level(&mut bed);
    assert!((quarter / full - 0.251).abs() < 0.02, "{quarter} of {full}");
    let shelf_id = live_bus(&bed, "music").effects[0].id.unwrap();
    // Bypassed: as it came.
    layout.buses.get_mut("music").unwrap().effects[0].bypass = true;
    bed.sound.set_layout(&layout);
    assert!((level(&mut bed) / full - 1.0).abs() < 0.02);
    // Back, and a compressor before it (−15 dBFS in, −30 at 4:1): the shelf carries on.
    let comp = effect("compressor", &[("threshold_db", -30.0)]);
    layout.buses.get_mut("music").unwrap().effects = vec![comp.clone(), shelf.clone()];
    bed.sound.set_layout(&layout);
    let first = level(&mut bed);
    assert_eq!(live_bus(&bed, "music").effects[1].id, Some(shelf_id));
    // 11.2 dB taken by the compressor, then 6 by the shelf.
    assert!(
        (first / full - 0.0190).abs() < 0.004,
        "compressor first: {}",
        first / full
    );
    let reduction = bed.sound.take_reduction("music", 0).unwrap();
    assert!((reduction - 11.2).abs() < 0.5, "{reduction} dB");
    assert_eq!(bed.sound.take_reduction("music", 1), Some(0.0), "a shelf");
    // The other way round: both carry on, moved; the compressor sees 6 dB less.
    layout.buses.get_mut("music").unwrap().effects = vec![shelf.clone(), comp.clone()];
    bed.sound.set_layout(&layout);
    let second = level(&mut bed);
    assert!(
        (second / full - 0.0536).abs() < 0.008,
        "shelf first: {}",
        second / full
    );
    assert_eq!(live_bus(&bed, "music").effects[0].id, Some(shelf_id));
    // A parameter changed in place; then the compressor gone and the shelf still there.
    layout.buses.get_mut("music").unwrap().effects[1]
        .params
        .insert(String::from("threshold_db"), 0.0);
    bed.sound.set_layout(&layout);
    assert!(
        (level(&mut bed) / full - 0.251).abs() < 0.02,
        "nothing compressed"
    );
    layout.buses.get_mut("music").unwrap().effects = vec![shelf];
    bed.sound.set_layout(&layout);
    assert!((level(&mut bed) / full - 0.251).abs() < 0.02);
    assert_eq!(live_bus(&bed, "music").effects.len(), 1);
    assert_eq!(live_bus(&bed, "music").effects[0].id, Some(shelf_id));
    // A kind the engine lacks holds its place and plays nothing; the master has effects.
    layout
        .buses
        .get_mut("music")
        .unwrap()
        .effects
        .insert(0, effect("chorus", &[]));
    layout
        .master
        .effects
        .push(effect("limiter", &[("ceiling_db", -24.0)]));
    bed.sound.set_layout(&layout);
    let limited = level(&mut bed);
    assert!(live_bus(&bed, "music").effects[0].id.is_none());
    assert_eq!(live_bus(&bed, "music").effects[1].id, Some(shelf_id));
    // The shelf leaves −21 dBFS, and the limiter −24: 9 dB under the level alone.
    assert!(
        (limited / full - 0.127).abs() < 0.015,
        "limited: {}",
        limited / full
    );
    assert!(bed.sound.take_reduction("master", 0).unwrap() > 2.0);
}

#[test]
fn sends_and_sidechains_follow_the_layout() {
    let mut bed = Bed::new("sends", AudioSettings::default());
    write_level_wav(&bed.folder.join("level.wav"), u64::from(RATE), 0.25);
    let mut layout = MixerFile::builtin();
    layout
        .buses
        .insert(String::from("canyon"), BusLayout::new());
    bed.sound.set_layout(&layout);
    let music = bed
        .world
        .spawn((AudioSource::new("level.wav").on_bus("music").looped(),));
    bed.until(|b| b.voice(music).is_some());
    // What a bus mixed over 20 ms, once the ramps are over.
    let peak = |bed: &mut Bed, bus: &str| {
        bed.frame(120);
        let _ = bed.sound.take_peak(bus);
        bed.frame(20);
        bed.sound.take_peak(bus).unwrap()[0]
    };
    let side = 0.25 * core::f32::consts::FRAC_1_SQRT_2;
    assert_eq!(peak(&mut bed, "canyon"), 0.0);
    // Music sends into canyon after its fader: canyon has it; then 6 dB less.
    let send = |gain: f32, pre_fader: bool| SendLayout { gain, pre_fader };
    fn sends(layout: &mut MixerFile) -> &mut std::collections::BTreeMap<String, SendLayout> {
        &mut layout.buses.get_mut("music").unwrap().sends
    }
    sends(&mut layout).insert(String::from("canyon"), send(0.0, false));
    bed.sound.set_layout(&layout);
    assert!((peak(&mut bed, "canyon") - side).abs() < 1e-3);
    sends(&mut layout).insert(String::from("canyon"), send(-6.0, false));
    bed.sound.set_layout(&layout);
    assert!((peak(&mut bed, "canyon") - side * 0.501).abs() < 1e-3);
    // Music's fader 20 dB down: an after-fader send follows it, a before-fader one does not.
    layout.buses.get_mut("music").unwrap().gain = -20.0;
    bed.sound.set_layout(&layout);
    assert!((peak(&mut bed, "canyon") - side * 0.0501).abs() < 1e-3);
    sends(&mut layout).insert(String::from("canyon"), send(-6.0, true));
    bed.sound.set_layout(&layout);
    assert!((peak(&mut bed, "canyon") - side * 0.501).abs() < 1e-3);
    assert_eq!(live_bus(&bed, "music").sends.len(), 1);
    sends(&mut layout).clear();
    bed.sound.set_layout(&layout);
    assert_eq!(peak(&mut bed, "canyon"), 0.0, "gone");
    assert!(live_bus(&bed, "music").sends.is_empty());
    // A compressor on music (−10 at 4:1) keyed by the voice (+12 dB): music alone
    // (−15 dBFS) is not compressed; the voice's −3 dBFS is.
    layout.buses.get_mut("music").unwrap().gain = 0.0;
    layout.buses.get_mut("music").unwrap().effects =
        vec![effect("compressor", &[("threshold_db", -10.0)])];
    layout.buses.get_mut("voice").unwrap().gain = 12.0;
    bed.sound.set_layout(&layout);
    let voice = bed
        .world
        .spawn((AudioSource::new("level.wav").on_bus("voice").looped(),));
    bed.until(|b| b.voice(voice).is_some());
    let reduction = |bed: &mut Bed, ms: u32| {
        bed.frame(ms);
        let _ = bed.sound.take_reduction("music", 0);
        bed.frame(20);
        bed.sound.take_reduction("music", 0).unwrap()
    };
    assert!(reduction(&mut bed, 120) < 0.01, "music alone");
    layout.buses.get_mut("music").unwrap().effects[0].sidechain = String::from("voice");
    bed.sound.set_layout(&layout);
    let ducked = reduction(&mut bed, 120);
    assert!(
        (ducked - 5.2).abs() < 0.6,
        "keyed by the voice: {ducked} dB"
    );
    assert_eq!(
        live_bus(&bed, "music").effects[0].keyed_by.as_deref(),
        Some("voice")
    );
    // Music sending into the voice as well would loop the sound back: the send stays, the
    // sidechain goes.
    sends(&mut layout).insert(String::from("voice"), send(-60.0, false));
    bed.sound.set_layout(&layout);
    assert_eq!(live_bus(&bed, "music").sends.len(), 1);
    assert!(live_bus(&bed, "music").effects[0].keyed_by.is_none());
    assert!(reduction(&mut bed, 600) < 0.2, "released");
}

#[test]
fn sources_heard_in_place_play_while_the_world_stands_and_nothing_is_written_back() {
    let mut bed = Bed::new("audition", AudioSettings::default());
    write_level_wav(&bed.folder.join("level.wav"), u64::from(RATE), 0.25);
    // A one-shot, not playing, that would despawn its entity at its end; the world kept
    // still, as the editor keeps it: silence.
    let mut once = AudioSource::new("level.wav").on_bus("music");
    once.mode = PlayMode::Despawn;
    once.playing = false;
    let e = bed.world.spawn((once,));
    bed.simulating = false;
    bed.frame(20);
    assert_eq!(energy(bed.frame(20).into_iter()), 0.0);
    // Heard in place: it plays, and loops past its end (1 s) without despawning anything.
    bed.sound.set_audition(&[e]);
    bed.until(|b| b.voice(e).is_some());
    bed.frame(100);
    assert!(energy(bed.frame(20).into_iter()) > 0.0, "heard");
    for _ in 0..12 {
        bed.frame(100);
    }
    assert!(bed.world.contains(e), "not despawned");
    assert!(energy(bed.frame(20).into_iter()) > 0.0, "looping");
    let s = bed.source(e);
    assert!(!s.playing && s.position == 0.0, "the component as it was");
    // A 3D sound 2 m in front of the camera, a listener far away: the camera hears it.
    let mut near = AudioSource3D::new("level.wav");
    near.max_distance = 10.0;
    let placed = bed.world.spawn((
        near,
        GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, 0.0, -2.0))),
    ));
    bed.world.spawn((
        AudioListener,
        GlobalTransform(Affine3A::from_translation(Vec3::new(1000.0, 0.0, 0.0))),
    ));
    bed.sound.set_audition(&[placed]);
    bed.until(|b| b.voice(placed).is_some());
    bed.frame(100);
    assert!(energy(bed.frame(20).into_iter()) > 0.0, "the camera hears");
    assert!(bed.voice(e).is_none(), "the first is no longer heard");
    // No more: silence again.
    bed.sound.set_audition(&[]);
    bed.frame(100);
    assert_eq!(energy(bed.frame(20).into_iter()), 0.0);
    assert!(bed.world.contains(e) && bed.world.contains(placed));
}

#[test]
fn the_editors_solo_and_mute_listen_on_top_of_the_games_mutes() {
    let mut bed = Bed::new("listening", AudioSettings::default());
    write_level_wav(&bed.folder.join("level.wav"), u64::from(RATE), 0.25);
    // Steps under effects; a sound on steps, one on music.
    let mut layout = MixerFile::builtin();
    layout
        .buses
        .insert(String::from("steps"), BusLayout::new().under("effects"));
    bed.sound.set_layout(&layout);
    let steps = bed
        .world
        .spawn((AudioSource::new("level.wav").on_bus("steps").looped(),));
    let music = bed
        .world
        .spawn((AudioSource::new("level.wav").on_bus("music").looped(),));
    bed.until(|b| b.voice(steps).is_some() && b.voice(music).is_some());
    let level = |bed: &mut Bed| {
        bed.frame(120);
        energy(bed.frame(20).into_iter())
    };
    // The two play the same level in step: together four times the power of one.
    let both = level(&mut bed);
    // The meters: what each bus mixed since the last look.
    let peak = |bed: &Bed, bus: &str| bed.sound.take_peak(bus).unwrap()[0];
    let _ = peak(&bed, "music");
    bed.frame(20);
    // A mono sound in the middle: each side at -3 dB (equal power).
    let p = peak(&bed, "music");
    assert!(
        (p - 0.25 * core::f32::consts::FRAC_1_SQRT_2).abs() < 0.01,
        "music's peak {p}"
    );
    assert!(peak(&bed, "voice") == 0.0, "nothing on voice");
    assert!(bed.sound.take_peak("no such bus").is_none());
    // Effects soloed: steps feeds it (heard), music does not (silent).
    let s = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    bed.sound.set_listening(&[], &s(&["effects"]));
    let solo = level(&mut bed);
    assert!((solo / both - 0.25).abs() < 0.03, "{solo} of {both}");
    // Steps soloed: effects, which it feeds, stays; music is silent.
    bed.sound.set_listening(&[], &s(&["steps"]));
    assert!((level(&mut bed) / both - 0.25).abs() < 0.03);
    // Music muted, no solo: steps alone; then nothing, the game unmuted underneath.
    bed.sound.set_listening(&s(&["music"]), &[]);
    assert!((level(&mut bed) / both - 0.25).abs() < 0.03);
    bed.sound.set_listening(&s(&["music", "effects"]), &[]);
    assert_eq!(level(&mut bed), 0.0);
    bed.sound.set_listening(&[], &[]);
    assert!((level(&mut bed) / both - 1.0).abs() < 0.05);
    // The game's own mute stays when the aids go.
    let mixer = bed.world.resource_mut::<Mixer>().unwrap();
    assert!(mixer.set_muted("music", true));
    bed.sound.set_listening(&s(&["effects"]), &[]);
    bed.sound.set_listening(&[], &[]);
    assert!(
        (level(&mut bed) / both - 0.25).abs() < 0.03,
        "music still muted by the game"
    );
}

#[test]
fn the_players_layout_comes_from_its_file_or_is_the_built_in_one() {
    let dir = std::env::temp_dir().join(format!("ofelia-layout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.mixer");
    // No file: the built-in four.
    assert_eq!(crate::player::load_layout(&path), MixerFile::builtin());
    // A file: its buses.
    let mut layout = MixerFile::default();
    layout
        .buses
        .insert(String::from("sfx"), BusLayout::new().at(-3.0));
    std::fs::write(&path, layout.to_text(engine_scene::AssetId::new())).unwrap();
    assert_eq!(crate::player::load_layout(&path), layout);
    // A file that cannot be read: the built-in four again (and a warning).
    std::fs::write(&path, "[mixer version=1]\nbuses[\"sfx\"].gain = loud\n").unwrap();
    assert_eq!(crate::player::load_layout(&path), MixerFile::builtin());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_sound_or_bus_does_no_harm() {
    let mut bed = Bed::new("missing", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    let missing = bed.world.spawn((AudioSource::new("nothing.ogg"),));
    let stray = bed
        .world
        .spawn((AudioSource::new("tone.wav").on_bus("no such bus"),));
    bed.until(|b| {
        let id = b.track(missing).and_then(|t| t.sound);
        id.is_some_and(|id| matches!(b.sound.sounds.entries[id as usize].state, State::Failed))
            && b.voice(stray).is_some()
    });
    bed.frame(10);
    assert!(bed.voice(missing).is_none());
    assert!(bed.source(missing).playing);
}

#[test]
fn sound_off_opens_nothing() {
    let (mut sound, renderer) = Sound::manual(&AudioSettings {
        enabled: false,
        ..AudioSettings::default()
    });
    assert!(renderer.is_none());
    assert!(!sound.available());
    assert!(sound.error().is_some());
    let mut world = World::new();
    world.spawn((AudioSource::new("tone.wav"),));
    sound.update(&mut world, None, None, 0.016, true);
    assert!(sound.plain.list.is_empty());
}

/// Static boxes: walls of `half` half-extents at `at`.
fn walls(physics: &mut JoltBackend, boxes: &[(Vec3, Vec3)]) {
    for &(at, half) in boxes {
        let shape = physics
            .create_shape(&ShapeDesc::Box { half_extents: half })
            .unwrap();
        physics.create_body(&BodyDesc::fixed(shape, at)).unwrap();
    }
}

#[test]
fn a_wall_muffles_what_is_behind_it() {
    let mut bed = Bed::new("occlusion", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    let mut physics = JoltBackend::new(&PhysicsSettings::default()).unwrap();
    // A wall 5 m ahead, between the listener and the sound 10 m away.
    walls(
        &mut physics,
        &[(Vec3::new(0.0, 0.0, -5.0), Vec3::new(4.0, 4.0, 0.2))],
    );
    bed.physics = Some(physics);
    let behind = bed.world.spawn((
        GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, 0.0, -10.0))),
        AudioSource3D::new("tone.wav").looped().within(1.0, 50.0),
    ));
    let beside = bed.world.spawn((
        GlobalTransform(Affine3A::from_translation(Vec3::new(10.0, 0.0, 0.0))),
        AudioSource3D::new("tone.wav").looped().within(1.0, 50.0),
    ));
    bed.until(|b| b.voice(behind).is_some() && b.voice(beside).is_some());
    for _ in 0..30 {
        bed.frame(16);
    }
    let behind = bed.track(behind).unwrap();
    assert!(
        (behind.occlusion_target - 0.6).abs() < 1e-6,
        "{}",
        behind.occlusion_target
    );
    assert!(behind.occlusion > 0.55, "{}", behind.occlusion);
    assert_eq!(bed.track(beside).unwrap().occlusion_target, 0.0);
}

#[test]
fn a_closed_room_gives_reverb_and_open_ground_none() {
    let mut bed = Bed::new("room", AudioSettings::default());
    let mut physics = JoltBackend::new(&PhysicsSettings::default()).unwrap();
    // A 16 m stone box around the listener.
    let (h, t) = (8.0, 0.2);
    walls(
        &mut physics,
        &[
            (Vec3::new(h, 0.0, 0.0), Vec3::new(t, h, h)),
            (Vec3::new(-h, 0.0, 0.0), Vec3::new(t, h, h)),
            (Vec3::new(0.0, h, 0.0), Vec3::new(h, t, h)),
            (Vec3::new(0.0, -h, 0.0), Vec3::new(h, t, h)),
            (Vec3::new(0.0, 0.0, h), Vec3::new(h, h, t)),
            (Vec3::new(0.0, 0.0, -h), Vec3::new(h, h, t)),
        ],
    );
    bed.physics = Some(physics);
    for _ in 0..120 {
        bed.frame(16);
    }
    let room = bed.sound.acoustics.room().unwrap();
    assert!((room.level - 0.35).abs() < 1e-6, "{room:?}");
    assert!(room.decay > 1.0, "{room:?}");
    assert!(bed.sound.reverb.is_some());
    assert!(bed.sound.room.level > 0.3, "{:?}", bed.sound.room);

    // Only a floor: open ground.
    let mut bed = Bed::new("field", AudioSettings::default());
    let mut physics = JoltBackend::new(&PhysicsSettings::default()).unwrap();
    walls(
        &mut physics,
        &[(Vec3::new(0.0, -2.0, 0.0), Vec3::new(100.0, 0.2, 100.0))],
    );
    bed.physics = Some(physics);
    for _ in 0..60 {
        bed.frame(16);
    }
    assert!(bed.sound.acoustics.room().unwrap().level < 0.01);
}

#[test]
fn snapshots_blend_their_values_over_the_mix_by_weight() {
    let mut bed = Bed::new("snapshots", AudioSettings::default());
    let mut layout = MixerFile::builtin();
    layout
        .buses
        .get_mut("music")
        .unwrap()
        .effects
        .push(EffectLayout::new("low_pass"));
    layout.snapshots.push(
        SnapshotLayout::new("underwater")
            .with("music/gain", -20.0)
            .with("music/effects/0/frequency_hz", 600.0)
            .with("room/gain", -6.0),
    );
    layout
        .snapshots
        .push(SnapshotLayout::new("paused").with("music/gain", -40.0));
    bed.sound.set_layout(&layout);
    bed.frame(16);
    fn music(bed: &Bed) -> &Bus {
        bed.sound.buses.iter().find(|b| b.name == "music").unwrap()
    }
    let db = |bed: &Bed| 20.0 * music(bed).fader.log10();
    let cutoff = |bed: &Bed| music(bed).effects[0].layout.param("frequency_hz").unwrap();
    assert!(db(&bed).abs() < 0.01, "the mix as it is: {}", db(&bed));
    // The game's mixer knows them; a name it lacks is refused.
    {
        let mixer = bed.world.resource_mut::<Mixer>().unwrap();
        assert_eq!(
            mixer.snapshots().collect::<Vec<_>>(),
            ["underwater", "paused"]
        );
        assert!(!mixer.blend("nope", 1.0, None));
        assert!(mixer.blend("underwater", 1.0, Some(0.0)));
    }
    bed.frame(16);
    assert!((db(&bed) + 20.0).abs() < 0.01, "{}", db(&bed));
    assert!((cutoff(&bed) - 600.0).abs() < 0.01, "{}", cutoff(&bed));
    assert!(
        (bed.sound.room_trim - 0.501).abs() < 0.01,
        "{}",
        bed.sound.room_trim
    );
    assert_eq!(
        bed.world.resource::<Mixer>().unwrap().weight("underwater"),
        Some(1.0)
    );
    // Halfway: halfway in dB and in hertz.
    bed.world
        .resource_mut::<Mixer>()
        .unwrap()
        .blend("underwater", 0.5, Some(0.0));
    bed.frame(16);
    assert!((db(&bed) + 10.0).abs() < 0.01, "{}", db(&bed));
    // The low-pass from its default, 2 kHz.
    let half = 2_000.0 + (600.0 - 2_000.0) * 0.5;
    assert!((cutoff(&bed) - half).abs() < 1.0, "{}", cutoff(&bed));
    // The later layer wins where they meet: paused at full weight over the half.
    bed.world
        .resource_mut::<Mixer>()
        .unwrap()
        .blend("paused", 1.0, Some(0.0));
    bed.frame(16);
    assert!((db(&bed) + 40.0).abs() < 0.01, "{}", db(&bed));
    // A fade: half of 0.4 s gone, half of the way.
    bed.world
        .resource_mut::<Mixer>()
        .unwrap()
        .blend("paused", 0.0, Some(0.4));
    for _ in 0..10 {
        bed.frame(20);
    }
    let weight = bed
        .world
        .resource::<Mixer>()
        .unwrap()
        .weight("paused")
        .unwrap();
    assert!((weight - 0.5).abs() < 0.03, "{weight}");
    for _ in 0..20 {
        bed.frame(20);
    }
    assert_eq!(
        bed.world.resource::<Mixer>().unwrap().weight("paused"),
        Some(0.0)
    );
    assert!(
        (db(&bed) + 10.0).abs() < 0.01,
        "underwater's half again: {}",
        db(&bed)
    );
    // A new layout keeps the weights by name.
    let mut again = layout.clone();
    again.buses.get_mut("music").unwrap().gain = -2.0;
    bed.sound.set_layout(&again);
    bed.frame(16);
    assert_eq!(
        bed.world.resource::<Mixer>().unwrap().weight("underwater"),
        Some(0.5)
    );
    assert!(
        (db(&bed) + 11.0).abs() < 0.01,
        "halfway from −2 to −20: {}",
        db(&bed)
    );
    // The editor's chosen snapshot alone at full weight, whatever the game blended; then
    // the game's weights again.
    bed.sound.hear_snapshot(Some("paused"));
    bed.frame(16);
    assert!((db(&bed) + 40.0).abs() < 0.01, "{}", db(&bed));
    assert_eq!(
        bed.world.resource::<Mixer>().unwrap().weight("underwater"),
        Some(0.0)
    );
    bed.sound.hear_snapshot(None);
    bed.frame(16);
    assert!((db(&bed) + 11.0).abs() < 0.01, "{}", db(&bed));
    // Back to 0: the mix as it is.
    bed.world
        .resource_mut::<Mixer>()
        .unwrap()
        .blend("underwater", 0.0, Some(0.0));
    bed.frame(16);
    assert!((db(&bed) + 2.0).abs() < 0.01, "{}", db(&bed));
    assert!((cutoff(&bed) - 2_000.0).abs() < 0.01, "{}", cutoff(&bed));
}

#[test]
fn a_reverb_zone_takes_over_where_the_listener_is() {
    let mut bed = Bed::new("zone", AudioSettings::default());
    let zone = bed.world.spawn((
        Transform::default(),
        GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, 0.0, -3.0))),
        ReverbZone::new(Vec3::splat(5.0), Room::CAVE),
    ));
    for _ in 0..120 {
        bed.frame(16);
    }
    assert!(bed.sound.reverb.is_some());
    assert!(
        (bed.sound.room.level - Room::CAVE.level).abs() < 0.01,
        "{:?}",
        bed.sound.room
    );
    assert!((bed.sound.room.decay - Room::CAVE.decay).abs() < 0.05);
    // What the editor's room strip shows (E11-2c): the zone, all of it, no walls; the
    // reverb's bus metered.
    let heard = bed.sound.room_heard();
    assert!(heard.made);
    match heard.from {
        RoomFrom::Zone {
            entity,
            weight,
            walls,
        } => {
            assert_eq!(entity, zone);
            assert!((weight - 1.0).abs() < 1e-3, "{weight}");
            assert!(!walls);
        }
        other => panic!("{other:?}"),
    }
    assert!(bed.sound.take_peak("reverb").is_some());
    // The layout's room fader trims it, now and for a reverb made later.
    let mut layout = MixerFile::builtin();
    layout.room.gain = -6.0;
    bed.sound.set_layout(&layout);
    assert!(
        (bed.sound.room_trim - 0.501).abs() < 0.01,
        "{}",
        bed.sound.room_trim
    );
    // Away from it, the reverb dies down.
    bed.world.get_mut::<GlobalTransform>(zone).unwrap().0 =
        Affine3A::from_translation(Vec3::new(0.0, 0.0, -100.0));
    for _ in 0..120 {
        bed.frame(16);
    }
    assert!(bed.sound.room.level < 0.01, "{:?}", bed.sound.room);
    assert_eq!(bed.sound.room_heard().from, RoomFrom::Nothing);
}

/// What a frame's sound costs the game thread: `n` looping placed sources circling the
/// listener, inside a room of walls when `acoustics` (rays at the default budget).
/// `cargo test --release -p engine_app --lib measure_sound -- --ignored --nocapture`
#[test]
#[ignore = "a measurement"]
fn measure_sound() {
    for (n, acoustics) in [(100, false), (1_000, false), (1_000, true)] {
        let mut bed = Bed::new(
            &format!("measure-{n}-{acoustics}"),
            AudioSettings::default(),
        );
        write_wav(&bed.folder.join("tone.wav"), RATE as u64);
        if acoustics {
            let mut physics = JoltBackend::new(&PhysicsSettings::default()).unwrap();
            let (h, t) = (40.0, 0.5);
            let mut boxes = vec![
                (Vec3::new(h, 0.0, 0.0), Vec3::new(t, h, h)),
                (Vec3::new(-h, 0.0, 0.0), Vec3::new(t, h, h)),
                (Vec3::new(0.0, h, 0.0), Vec3::new(h, t, h)),
                (Vec3::new(0.0, -h, 0.0), Vec3::new(h, t, h)),
                (Vec3::new(0.0, 0.0, h), Vec3::new(h, h, t)),
                (Vec3::new(0.0, 0.0, -h), Vec3::new(h, h, t)),
            ];
            // Pillars in between.
            for i in 0..64 {
                let (x, z) = ((i % 8) as f32 * 9.0 - 31.5, (i / 8) as f32 * 9.0 - 31.5);
                boxes.push((Vec3::new(x, 0.0, z), Vec3::new(0.6, 6.0, 0.6)));
            }
            walls(&mut physics, &boxes);
            bed.physics = Some(physics);
        }
        let mut seed = 0x2545_f491u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32
        };
        let placed: Vec<(Entity, f32, f32)> = (0..n)
            .map(|_| {
                let (radius, phase) = (2.0 + next() * 30.0, next() * core::f32::consts::TAU);
                let mut source = AudioSource3D::new("tone.wav").looped().within(1.0, 60.0);
                source.position = f64::from(next());
                let e = bed.world.spawn((GlobalTransform::IDENTITY, source));
                (e, radius, phase)
            })
            .collect();
        bed.until(|b| placed.iter().all(|&(e, ..)| b.voice(e).is_some()));
        let mut update = Vec::new();
        let mut mix = Vec::new();
        for frame in 0..660 {
            let t = frame as f32 / 60.0;
            for &(e, radius, phase) in &placed {
                let a = phase + t * 0.5;
                bed.world.get_mut::<GlobalTransform>(e).unwrap().0 =
                    Affine3A::from_translation(Vec3::new(a.cos() * radius, 1.0, a.sin() * radius));
            }
            let physics = bed.physics.as_ref().map(|p| p as &dyn PhysicsBackend);
            let start = std::time::Instant::now();
            bed.sound
                .update(&mut bed.world, Some(&bed.camera), physics, 1.0 / 60.0, true);
            let updated = start.elapsed();
            let mut out = vec![0.0; 800 * 2];
            let start = std::time::Instant::now();
            bed.renderer.render(&mut out, 2);
            if frame >= 60 {
                update.push(updated.as_secs_f64() * 1e6);
                mix.push(start.elapsed().as_secs_f64() * 1e6);
            }
        }
        let summary = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            (v[v.len() / 2], v[v.len() * 99 / 100])
        };
        let (u50, u99) = summary(&mut update);
        let (m50, m99) = summary(&mut mix);
        println!(
            "{n} sources, acoustics {acoustics}: update {u50:.0} µs (p99 {u99:.0}), \
             mix of 16.7 ms {m50:.0} µs (p99 {m99:.0}), voices {:?}",
            bed.audio().voice_counts()
        );
    }
}

#[test]
fn plain_sounds_stay_dry_and_3d_ones_reverberate() {
    let mut bed = Bed::new("routing", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    let plain = bed
        .world
        .spawn((AudioSource::new("tone.wav").on_bus("effects").looped(),));
    let placed = bed.world.spawn((
        GlobalTransform::IDENTITY,
        AudioSource3D::new("tone.wav").looped(),
    ));
    let mut dry = AudioSource3D::new("tone.wav").looped();
    dry.reverb = false;
    let dry = bed.world.spawn((GlobalTransform::IDENTITY, dry));
    bed.until(|b| [plain, placed, dry].iter().all(|&e| b.voice(e).is_some()));
    let effects = bed
        .sound
        .buses
        .iter()
        .find(|b| b.name == "effects")
        .unwrap();
    let (bus, world) = (effects.id, effects.world.unwrap());
    // The reverb is fed by the world child alone: the plain sound and the dry 3D one stay
    // out of it, all three under the effects bus's volume.
    assert_eq!(bed.track(plain).unwrap().bus, bus);
    assert_eq!(bed.track(placed).unwrap().bus, world);
    assert_eq!(bed.track(dry).unwrap().bus, bus);
    // Music has no world child.
    assert!(
        bed.sound
            .buses
            .iter()
            .find(|b| b.name == "music")
            .unwrap()
            .world
            .is_none()
    );
}

#[test]
fn sounds_play_from_a_pack_where_the_folder_is_missing() {
    let mut bed = Bed::new("pack", AudioSettings::default());
    let folder = bed.folder.join("source");
    std::fs::create_dir_all(folder.join("sfx")).unwrap();
    write_wav(&folder.join("sfx/tone.wav"), RATE as u64);
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine_audio_decode/tests/data");
    std::fs::copy(fixtures.join("stereo_44100.ogg"), folder.join("theme.ogg")).unwrap();
    let pack = bed.folder.join("sounds.pack");
    let report = engine_assets::sounds::write_sound_pack(&folder, &pack, 19).unwrap();
    assert_eq!(report.sounds.len(), 2);
    std::fs::remove_dir_all(&folder).unwrap();
    // By build: the folder is missing, so the pack.
    let settings = AudioSettings {
        folder: bed.folder.join("nothing here"),
        pack,
        stream_from: 0.5,
        ..AudioSettings::default()
    };
    let (sound, renderer) = Sound::manual(&settings);
    bed.sound = sound;
    bed.renderer = renderer.unwrap();
    assert!(
        bed.sound.note().contains("sounds.pack"),
        "{}",
        bed.sound.note()
    );
    let clip = bed.world.spawn((AudioSource::new("sfx/tone.wav"),));
    let music = bed.world.spawn((AudioSource {
        position: 0.05,
        ..AudioSource::new("theme.ogg").looped()
    },));
    bed.until(|b| b.voice(clip).is_some() && b.voice(music).is_some());
    let theme = bed.track(music).unwrap().sound.unwrap() as usize;
    assert!(matches!(
        bed.sound.sounds.entries[theme].state,
        State::Long { rate: 44_100 }
    ));
    // The clip from the pack, sample for sample; the music streamed from the pack's file.
    let out = bed.frame(10);
    let (l, r) = (
        energy(out.chunks_exact(2).map(|f| f[0])),
        energy(out.chunks_exact(2).map(|f| f[1])),
    );
    assert!(l > 0.0 && r > 0.0);
    for _ in 0..9 {
        bed.frame(10);
    }
    assert!(
        (bed.source(music).position - 0.14).abs() < 0.001,
        "{}",
        bed.source(music).position
    );
    assert_eq!(bed.audio().stream_underruns(), 0);
}

#[test]
fn a_changed_file_is_heard_at_once_from_where_it_was() {
    let mut bed = Bed::new("live", AudioSettings::default());
    write_wav(&bed.folder.join("tone.wav"), RATE as u64);
    // Made again: the folder exists now, and is watched.
    let (sound, renderer) = Sound::manual(&AudioSettings {
        folder: bed.folder.clone(),
        files: SoundFiles::Folder,
        ..AudioSettings::default()
    });
    bed.sound = sound;
    bed.renderer = renderer.unwrap();
    assert!(bed.sound.live_folder.is_some());
    let e = bed.world.spawn((AudioSource::new("tone.wav").looped(),));
    bed.until(|b| b.voice(e).is_some());
    let first = bed.voice(e).unwrap();
    bed.frame(100);
    // The file is saved again with another signal.
    write_wav_with(&bed.folder.join("tone.wav"), RATE as u64, 0.3);
    bed.until(|b| b.voice(e).is_some_and(|v| v != first));
    // The old voice fades out over 20 ms; then the new sound alone, from where it was.
    bed.frame(30);
    let out = bed.frame(10);
    let at = (bed.source(e).position * f64::from(RATE)).round() as u64;
    assert!(at > 4_800, "{at}");
    for (i, &s) in left(&out).iter().enumerate() {
        let expected = (value(at + i as u64) + 0.3) * CENTRE;
        assert!((s - expected).abs() < 1e-6, "frame {i}: {s} vs {expected}");
    }
}

#[test]
fn a_preview_plays_a_file_as_it_is_while_the_world_stands_still() {
    let mut bed = Bed::new("preview", AudioSettings::default());
    // The editor's world, kept still: its sources silent, the preview heard.
    bed.simulating = false;
    let file = bed.folder.join("beep.wav");
    write_wav(&file, u64::from(RATE) / 2);
    bed.sound.preview(&file, 0.0, false);
    bed.until(|b| b.sound.preview_state().is_some_and(|p| p.playing));
    let out = bed.frame(50);
    assert!(energy(left(&out).into_iter()) > 0.0, "heard");
    let state = bed.sound.preview_state().unwrap();
    assert!(state.position > 0.0, "{state:?}");
    assert_eq!(state.length, Some(0.5));
    assert!(state.failed.is_none());
    // Stopped: silent once its fade is out.
    bed.sound.stop_preview();
    assert!(bed.sound.preview_state().is_none());
    bed.frame(30);
    let out = bed.frame(20);
    assert_eq!(energy(left(&out).into_iter()), 0.0);
}

#[test]
fn a_long_preview_streams_from_where_it_is_asked() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine_audio_decode/tests/data/stereo_44100.ogg");
    let mut bed = Bed::new(
        "preview-stream",
        AudioSettings {
            stream_from: 0.0,
            ..AudioSettings::default()
        },
    );
    bed.simulating = false;
    bed.sound.preview(&file, 0.05, false);
    bed.until(|b| b.sound.preview_state().is_some_and(|p| p.playing));
    for _ in 0..10 {
        bed.frame(10);
    }
    let state = bed.sound.preview_state().unwrap();
    // 50 ms in, then 100 ms of mix: the voice's position, read after the last mix. Its
    // length from the Ogg file's last page.
    assert!((state.position - 0.15).abs() < 0.002, "{state:?}");
    assert!(state.length.is_some_and(|l| l > 0.15), "{state:?}");
    assert_eq!(bed.audio().stream_underruns(), 0);
    // A file that is not there says why.
    bed.sound
        .preview(&bed.folder.join("missing.ogg"), 0.0, false);
    bed.until(|b| b.sound.preview_state().is_some_and(|p| p.failed.is_some()));
}
