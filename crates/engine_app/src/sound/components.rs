//! The components a game puts sound on its entities with (ADR 0105). All are reflected:
//! saves keep them, and with them where every sound was.
//!
//! - [`AudioSource`]: a sound heard as it is (music, the UI, a narrator), nothing more.
//! - [`AudioSource3D`]: a sound in the world, at its entity: fainter with distance, from
//!   its side, muffled by walls, answered by the room.

use engine_core::math::Vec3;
use engine_ecs::reflect_component;
use engine_reflect::Reflect;

/// What a source does when its sound ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum PlayMode {
    /// Stops: `playing` turns `false` and `position` goes back to 0 (set `playing` to play
    /// it again).
    #[default]
    Once = 0,
    /// Starts over until stopped.
    Loop = 1,
    /// Despawns its entity: a one-shot sound (a shot, a footstep) needs no clean-up.
    Despawn = 2,
}

/// How a sound in the world gets quieter with distance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum Falloff {
    /// Physical: half as loud each time the distance doubles past `min_distance`, held
    /// from `max_distance` on.
    #[default]
    Inverse = 0,
    /// Straight down from full at `min_distance` to silence at `max_distance`, where the
    /// sound stops costing anything.
    Linear = 1,
}

/// A sound heard as it is (ADR 0105): music, the UI, a narrator. No distance, no walls, no
/// reverb, whatever bus it is on.
///
/// ```ignore
/// world.spawn((AudioSource::new("music/theme.ogg").on_bus("music").looped(),));
/// ```
///
/// The player starts it once its sound is loaded and writes back where it is, so saves hold
/// it: after a load it goes on from there. A fade is a `Tween` of `AudioSource.volume`.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct AudioSource {
    /// The sound: a file under the sounds folder ([`crate::AudioSettings::folder`]) by its
    /// path, with `/` (`music/theme.ogg`): WAV, Ogg Vorbis, Opus or FLAC.
    pub sound: String,
    /// The bus it plays on, by name ([`crate::AudioSettings::buses`]); a name no bus has
    /// plays on the master. Read when it starts.
    pub bus: String,
    /// Linear gain.
    pub volume: f32,
    /// What it does at its end.
    pub mode: PlayMode,
    /// Playing. `false` stops it where it is; `true` again goes on from there.
    pub playing: bool,
    /// Seconds into the sound. The player keeps it while the sound plays; setting it moves
    /// the sound there (a seek).
    pub position: f64,
    /// After a load, starts from the beginning instead of where the save had it.
    pub restart_on_load: bool,
}

reflect_component!(AudioSource);

impl Default for AudioSource {
    fn default() -> Self {
        Self {
            sound: String::new(),
            bus: String::from("music"),
            volume: 1.0,
            mode: PlayMode::Once,
            playing: true,
            position: 0.0,
            restart_on_load: false,
        }
    }
}

impl AudioSource {
    /// `sound` played once on the `music` bus, starting at once.
    pub fn new(sound: &str) -> Self {
        Self {
            sound: sound.to_owned(),
            ..Self::default()
        }
    }

    /// On the bus named `bus`.
    pub fn on_bus(mut self, bus: &str) -> Self {
        bus.clone_into(&mut self.bus);
        self
    }

    /// At linear gain `volume`.
    pub fn at_volume(mut self, volume: f32) -> Self {
        self.volume = volume;
        self
    }

    /// Starting over until stopped.
    pub fn looped(mut self) -> Self {
        self.mode = PlayMode::Loop;
        self
    }

    /// Despawning its entity at its end.
    pub fn despawned_at_end(mut self) -> Self {
        self.mode = PlayMode::Despawn;
        self
    }

    /// Not playing until `playing` is set.
    pub fn paused(mut self) -> Self {
        self.playing = false;
        self
    }
}

/// A sound in the world, at its entity's `GlobalTransform` (ADR 0105): fainter with
/// distance, panned to its side, its pitch moved by the Doppler effect. With the game's
/// physics ([`crate::Game::acoustics`]), walls between it and the listener muffle it and
/// the room around the listener answers it.
///
/// ```ignore
/// world.spawn((Transform::from_translation(muzzle),
///     AudioSource3D::new("sfx/shot.wav").within(2.0, 80.0).despawned_at_end()));
/// ```
///
/// What it plays and how is an [`AudioSource`]'s (`source`, also reached through this
/// one: `shot.playing`); the rest says how it sits in the world, read when it starts.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct AudioSource3D {
    /// What it plays and how: the sound, the bus (`effects` by default), volume, the end,
    /// playing, the position saves keep.
    pub source: AudioSource,
    /// Playback rate: 2 is an octave up and twice as fast (an engine's revs).
    pub pitch: f32,
    /// Full volume up to here, in metres. It is the sound's size too: closer, it pans less,
    /// so a sound passing through the listener never jumps from one ear to the other.
    pub min_distance: f32,
    /// Where the falloff ends, in metres.
    pub max_distance: f32,
    /// The curve between them.
    pub falloff: Falloff,
    /// The Doppler effect's strength: 1 physical, 0 none (a fast camera often sounds
    /// better at 0.3–0.5).
    pub doppler: f32,
    /// Walls between it and the listener muffle it.
    pub occlusion: bool,
    /// The room around the listener answers it (on a bus with `reverb`).
    pub reverb: bool,
    /// Higher wins when sounds compete for the mixer's voices.
    pub priority: u8,
}

reflect_component!(AudioSource3D);

impl Default for AudioSource3D {
    fn default() -> Self {
        Self {
            source: AudioSource::default().on_bus("effects"),
            pitch: 1.0,
            min_distance: 1.0,
            max_distance: 100.0,
            falloff: Falloff::Inverse,
            doppler: 1.0,
            occlusion: true,
            reverb: true,
            priority: 128,
        }
    }
}

impl core::ops::Deref for AudioSource3D {
    type Target = AudioSource;

    fn deref(&self) -> &AudioSource {
        &self.source
    }
}

impl core::ops::DerefMut for AudioSource3D {
    fn deref_mut(&mut self) -> &mut AudioSource {
        &mut self.source
    }
}

impl AudioSource3D {
    /// `sound` played once on the `effects` bus at its entity, starting at once.
    pub fn new(sound: &str) -> Self {
        let mut s = Self::default();
        sound.clone_into(&mut s.source.sound);
        s
    }

    /// Full volume within `min_distance` metres, falling off to `max_distance`.
    pub fn within(mut self, min_distance: f32, max_distance: f32) -> Self {
        self.min_distance = min_distance;
        self.max_distance = max_distance;
        self
    }

    /// On the bus named `bus`.
    pub fn on_bus(mut self, bus: &str) -> Self {
        bus.clone_into(&mut self.source.bus);
        self
    }

    /// At linear gain `volume`.
    pub fn at_volume(mut self, volume: f32) -> Self {
        self.source.volume = volume;
        self
    }

    /// Starting over until stopped.
    pub fn looped(mut self) -> Self {
        self.source.mode = PlayMode::Loop;
        self
    }

    /// Despawning its entity at its end.
    pub fn despawned_at_end(mut self) -> Self {
        self.source.mode = PlayMode::Despawn;
        self
    }

    /// Not playing until `playing` is set.
    pub fn paused(mut self) -> Self {
        self.source.playing = false;
        self
    }
}

/// The entity that hears: sounds are heard from its `GlobalTransform` (facing its −Z,
/// with +Y up). Without one, the main camera hears; with several, the first found does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub struct AudioListener;

reflect_component!(AudioListener);

/// A reverb's character: how a room answers.
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub struct Room {
    /// Seconds the tail takes to fall 60 dB.
    pub decay: f32,
    /// 0 (a small room) .. 1 (a hall): how far apart the echoes are.
    pub size: f32,
    /// Hz above which the walls swallow the sound: lower for softer walls.
    pub damping: f32,
    /// Seconds before the first echo.
    pub pre_delay: f32,
    /// How much of what is heard goes into the reverb, 0..1.
    pub level: f32,
}

impl Room {
    /// Nearly dry: open air.
    pub const OUTDOORS: Self = Self {
        decay: 0.8,
        size: 1.0,
        damping: 5_000.0,
        pre_delay: 0.05,
        level: 0.04,
    };
    /// A furnished room.
    pub const ROOM: Self = Self {
        decay: 0.6,
        size: 0.25,
        damping: 7_000.0,
        pre_delay: 0.008,
        level: 0.25,
    };
    /// A corridor of hard walls.
    pub const CORRIDOR: Self = Self {
        decay: 1.2,
        size: 0.4,
        damping: 7_000.0,
        pre_delay: 0.01,
        level: 0.3,
    };
    /// A stone hall.
    pub const HALL: Self = Self {
        decay: 2.4,
        size: 0.8,
        damping: 6_000.0,
        pre_delay: 0.025,
        level: 0.35,
    };
    /// A cave.
    pub const CAVE: Self = Self {
        decay: 3.5,
        size: 0.9,
        damping: 4_000.0,
        pre_delay: 0.03,
        level: 0.45,
    };

    /// Between `self` (at 0) and `other` (at 1).
    pub fn lerp(&self, other: &Self, t: f32) -> Self {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Self {
            decay: mix(self.decay, other.decay),
            size: mix(self.size, other.size),
            damping: mix(self.damping, other.damping),
            pre_delay: mix(self.pre_delay, other.pre_delay),
            level: mix(self.level, other.level),
        }
    }
}

impl Default for Room {
    fn default() -> Self {
        Self::ROOM
    }
}

/// A place with a reverb of its own, over what the geometry around the listener gives
/// (ADR 0105): a box at its entity (the `GlobalTransform` places, turns and scales it) of
/// `half_extents`, its room taking over as the listener comes within `fade` metres of it.
/// Where zones overlap, the highest `priority` wins. Only [`AudioSource3D`]s reverberate.
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub struct ReverbZone {
    /// Half the box's size along its entity's axes, in its entity's units.
    pub half_extents: Vec3,
    /// Metres outside the box over which its room fades out.
    pub fade: f32,
    /// The room it sounds like.
    pub room: Room,
    /// Which zone wins where zones overlap.
    pub priority: i32,
}

reflect_component!(ReverbZone);

impl Default for ReverbZone {
    fn default() -> Self {
        Self {
            half_extents: Vec3::splat(5.0),
            fade: 2.0,
            room: Room::ROOM,
            priority: 0,
        }
    }
}

impl ReverbZone {
    /// A box of `half_extents` sounding like `room`.
    pub fn new(half_extents: Vec3, room: Room) -> Self {
        Self {
            half_extents,
            room,
            ..Self::default()
        }
    }
}
