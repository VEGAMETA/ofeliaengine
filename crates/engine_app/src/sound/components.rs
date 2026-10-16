//! The components a game puts sound on its entities with (ADR 0105). All are reflected:
//! saves keep them, and with them where every sound was.

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

/// How a placed sound gets quieter with distance.
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

/// A sound placed at its entity: heard from where the entity is, panned to its side,
/// fainter with distance. Read when the sound starts.
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub struct Spatial {
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
    /// Walls between it and the listener muffle it (with the game's acoustics,
    /// [`crate::Game::acoustics`]).
    pub occlusion: bool,
}

impl Default for Spatial {
    fn default() -> Self {
        Self {
            min_distance: 1.0,
            max_distance: 100.0,
            falloff: Falloff::Inverse,
            doppler: 1.0,
            occlusion: true,
        }
    }
}

impl Spatial {
    /// Full volume within `min_distance` metres, falling off to `max_distance`.
    pub fn within(min_distance: f32, max_distance: f32) -> Self {
        Self {
            min_distance,
            max_distance,
            ..Self::default()
        }
    }
}

/// A sound its entity plays (ADR 0105): music, a voice line, a footstep where it was made.
///
/// ```ignore
/// world.spawn((AudioSource::new("music/theme.ogg").looped().on_bus("music"),));
/// world.spawn((Transform::from_translation(at), AudioSource::new("sfx/shot.ogg")
///     .placed(Spatial::within(2.0, 80.0)).despawned_at_end()));
/// ```
///
/// The player starts its voice once the sound is loaded, keeps it in step every frame
/// (volume, pitch, the entity's position) and writes back where it is, so saves hold it:
/// after a load it goes on from there.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct AudioSource {
    /// The sound: a file under the sounds folder ([`crate::AudioSettings::folder`]) by its
    /// path, with `/` (`sfx/door.ogg`): WAV, Ogg Vorbis, Opus or FLAC.
    pub sound: String,
    /// The bus it plays on, by name ([`crate::AudioSettings::buses`]): `effects` by
    /// default; a name no bus has plays on the master. Read when it starts.
    pub bus: String,
    /// Linear gain.
    pub volume: f32,
    /// Playback rate: 2 is an octave up and twice as fast.
    pub pitch: f32,
    /// What it does at its end.
    pub mode: PlayMode,
    /// Playing. `false` stops it where it is; `true` again goes on from there.
    pub playing: bool,
    /// Placed at its entity's `GlobalTransform`; `None` is heard as it is (music, UI).
    pub spatial: Option<Spatial>,
    /// Seconds into the sound. The player keeps it while the sound plays; setting it moves
    /// the sound there (a seek).
    pub position: f64,
    /// After a load, starts from the beginning instead of where the save had it.
    pub restart_on_load: bool,
    /// Higher wins when sounds compete for the mixer's voices.
    pub priority: u8,
    /// Seconds to fade in when it starts.
    pub fade_in: f32,
}

reflect_component!(AudioSource);

impl Default for AudioSource {
    fn default() -> Self {
        Self {
            sound: String::new(),
            bus: String::from("effects"),
            volume: 1.0,
            pitch: 1.0,
            mode: PlayMode::Once,
            playing: true,
            spatial: None,
            position: 0.0,
            restart_on_load: false,
            priority: 128,
            fade_in: 0.0,
        }
    }
}

impl AudioSource {
    /// `sound` played once on the `effects` bus, as it is, starting at once.
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

    /// Placed at its entity.
    pub fn placed(mut self, spatial: Spatial) -> Self {
        self.spatial = Some(spatial);
        self
    }

    /// Not playing until `playing` is set.
    pub fn paused(mut self) -> Self {
        self.playing = false;
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
/// Where zones overlap, the highest `priority` wins.
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
