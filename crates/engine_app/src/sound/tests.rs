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
        bytes.extend_from_slice(&value(i).to_le_bytes());
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
            self.sound
                .update(&mut self.world, Some(&self.camera), physics, 0.005);
            if done(self) {
                return;
            }
            wait_ms(5);
        }
        panic!("timed out");
    }

    fn track(&self, entity: Entity) -> Option<&Track> {
        self.sound.tracks.list.iter().find(|t| t.entity == entity)
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
    assert!(bed.sound.tracks.list.is_empty());
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
    assert!(bed.sound.tracks.list.is_empty());
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
        AudioSource::new("tone.wav").placed(Spatial::within(1.0, 50.0)),
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
    sound.update(&mut world, None, None, 0.016);
    assert!(sound.tracks.list.is_empty());
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
        AudioSource::new("tone.wav")
            .looped()
            .placed(Spatial::within(1.0, 50.0)),
    ));
    let beside = bed.world.spawn((
        GlobalTransform(Affine3A::from_translation(Vec3::new(10.0, 0.0, 0.0))),
        AudioSource::new("tone.wav")
            .looped()
            .placed(Spatial::within(1.0, 50.0)),
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
    // Away from it, the reverb dies down.
    bed.world.get_mut::<GlobalTransform>(zone).unwrap().0 =
        Affine3A::from_translation(Vec3::new(0.0, 0.0, -100.0));
    for _ in 0..120 {
        bed.frame(16);
    }
    assert!(bed.sound.room.level < 0.01, "{:?}", bed.sound.room);
}

/// What a frame's sound costs the game thread: `n` looping placed sources circling the
/// listener, inside a room of walls when `acoustics` (rays at the default budget).
/// `cargo test --release -p engine_app --lib measure_sound -- --ignored --nocapture`
#[test]
#[ignore = "a measurement"]
fn measure_sound() {
    for (n, acoustics) in [(100, false), (1_000, false), (1_000, true)] {
        let mut bed = Bed::new(&format!("measure-{n}-{acoustics}"), AudioSettings::default());
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
                let e = bed.world.spawn((
                    GlobalTransform::IDENTITY,
                    AudioSource {
                        position: f64::from(next()),
                        ..AudioSource::new("tone.wav")
                            .looped()
                            .placed(Spatial::within(1.0, 60.0))
                    },
                ));
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
                .update(&mut bed.world, Some(&bed.camera), physics, 1.0 / 60.0);
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
