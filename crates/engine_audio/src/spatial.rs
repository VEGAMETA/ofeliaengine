//! Spatial audio: sounds placed in the world and heard from a listener (ADR 0050,
//! docs/research/spatial-audio.md).
//!
//! A voice played with [`crate::PlayParams::spatial`] follows an [`Emitter`]: the mixer
//! derives its gain from the distance ([`Rolloff`]), its pan from the direction in the
//! listener's frame, its pitch from the Doppler effect, and a low-pass from air absorption
//! and occlusion. The listener is set once per frame with [`crate::Audio::set_listener`].
//! Everything is recomputed on the audio thread once per render call (every device period,
//! about 5 ms) and ramped over it, so moving sounds never click and one listener update
//! moves every sound at once.
//!
//! World conventions are the engine's (ADR 0012): right-handed, +Y up, metres, seconds.

use engine_core::math::Vec3;

/// Speed of sound in air at 20 °C, m/s.
pub const SPEED_OF_SOUND: f32 = 343.0;

/// Where sounds are heard from: usually the camera, or the player's head.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Listener {
    /// Position in the world.
    pub position: Vec3,
    /// Where the listener faces (need not be normalized).
    pub forward: Vec3,
    /// The listener's up (need not be normalized or exactly orthogonal to `forward`).
    pub up: Vec3,
    /// Velocity in m/s, for the Doppler effect.
    pub velocity: Vec3,
}

impl Default for Listener {
    /// At the origin, facing −Z with +Y up (the engine's camera convention), at rest.
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            forward: Vec3::NEG_Z,
            up: Vec3::Y,
            velocity: Vec3::ZERO,
        }
    }
}

impl Listener {
    /// The listener's right: `forward × up` in the engine's right-handed world.
    pub fn right(&self) -> Vec3 {
        self.forward.cross(self.up).normalize_or_zero()
    }
}

/// How gain falls with distance between [`Emitter::min_distance`] and
/// [`Emitter::max_distance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rolloff {
    /// Physical: `min / distance`, −6 dB per doubling of the distance. Beyond the maximum
    /// the gain stays where it got to (so a 1 m sound is at −40 dB from 100 m on).
    #[default]
    Inverse,
    /// Straight from full gain at the minimum to silence at the maximum: the sound is
    /// gone (and its voice virtual) beyond the maximum.
    Linear,
}

/// A sound's place in the world and how distance affects it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Emitter {
    /// Position in the world.
    pub position: Vec3,
    /// Velocity in m/s, for the Doppler effect.
    pub velocity: Vec3,
    /// Full gain up to this distance (m). It is also the sound's size: closer than this,
    /// the pan narrows towards the centre, so a sound passing through the listener never
    /// jumps from one side to the other.
    pub min_distance: f32,
    /// Where the rolloff ends (m).
    pub max_distance: f32,
    /// The distance curve.
    pub rolloff: Rolloff,
    /// Doppler strength: 1 is physical, 0 disables it. Games often use 0.3–0.5, since a
    /// physical Doppler on fast cameras sounds exaggerated.
    pub doppler: f32,
    /// Air absorption strength: 1 halves the low-pass cutoff every 50 m (20 kHz at the
    /// source, 5 kHz at 100 m), 0 disables it.
    pub air_absorption: f32,
}

impl Default for Emitter {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            velocity: Vec3::ZERO,
            min_distance: 1.0,
            max_distance: 100.0,
            rolloff: Rolloff::Inverse,
            doppler: 1.0,
            air_absorption: 1.0,
        }
    }
}

impl Emitter {
    /// An emitter at `position` with the default distances (1 m full gain, 100 m end).
    pub fn at(position: Vec3) -> Self {
        Self {
            position,
            ..Self::default()
        }
    }
}

/// Gain at `distance` for a rolloff between `min` and `max` (what the mixer applies, for
/// tools and tests).
pub fn attenuation(distance: f32, min: f32, max: f32, rolloff: Rolloff) -> f32 {
    let min = min.max(1e-3);
    let max = max.max(min);
    let d = distance.clamp(min, max);
    match rolloff {
        Rolloff::Inverse => min / d,
        Rolloff::Linear if max > min => 1.0 - (d - min) / (max - min),
        Rolloff::Linear => {
            if distance <= min {
                1.0
            } else {
                0.0
            }
        }
    }
}

/// Full occlusion: this much quieter (dB) ...
const OCCLUSION_DB: f32 = 15.0;
/// ... and this many octaves of low-pass below 20 kHz (full occlusion: 625 Hz).
const OCCLUSION_OCTAVES: f32 = 5.0;
/// Air absorption halves the cutoff every this many metres (at strength 1).
const AIR_HALVING_METRES: f32 = 50.0;
/// Cutoffs at or above this are not filtered.
pub(crate) const NO_FILTER_HZ: f32 = 20_000.0;

/// What an emitter comes to, as heard by a listener.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Heard {
    /// Distance and occlusion gain.
    pub(crate) gain: f32,
    /// −1 (left) .. 1 (right).
    pub(crate) pan: f32,
    /// Doppler pitch ratio.
    pub(crate) pitch: f32,
    /// Low-pass cutoff in Hz ([`NO_FILTER_HZ`] and above: none).
    pub(crate) cutoff: f32,
}

impl Heard {
    /// A sound that is not spatial.
    pub(crate) const PLAIN: Self = Self {
        gain: 1.0,
        pan: 0.0,
        pitch: 1.0,
        cutoff: NO_FILTER_HZ,
    };
}

impl Default for Heard {
    fn default() -> Self {
        Self::PLAIN
    }
}

/// How `listener` hears `emitter` with `occlusion` (0 clear .. 1 fully occluded).
pub(crate) fn hear(listener: &Listener, emitter: &Emitter, occlusion: f32) -> Heard {
    let offset = emitter.position - listener.position;
    let distance = offset.length();
    if !distance.is_finite() {
        return Heard {
            gain: 0.0,
            ..Heard::PLAIN
        };
    }
    let occlusion = occlusion.clamp(0.0, 1.0);
    let gain = attenuation(
        distance,
        emitter.min_distance,
        emitter.max_distance,
        emitter.rolloff,
    ) * 10f32.powf(-OCCLUSION_DB * occlusion / 20.0);

    let (pan, pitch) = if distance > 1e-4 {
        let direction = offset / distance;
        // Closer than the sound's size, the sound surrounds the listener: narrow the pan.
        let width = (distance / emitter.min_distance.max(1e-3)).min(1.0);
        let pan = direction.dot(listener.right()) * width;
        // Doppler: the source moving away and the listener moving away both lower the
        // pitch. Speeds are limited to half the speed of sound (no sonic booms).
        let limit = 0.5 * SPEED_OF_SOUND;
        let factor = emitter.doppler.max(0.0);
        let source_away = (emitter.velocity.dot(direction) * factor).clamp(-limit, limit);
        let listener_towards = (listener.velocity.dot(direction) * factor).clamp(-limit, limit);
        let pitch = (SPEED_OF_SOUND + listener_towards) / (SPEED_OF_SOUND + source_away);
        (pan, pitch)
    } else {
        (0.0, 1.0)
    };

    let air = distance * emitter.air_absorption.max(0.0) / AIR_HALVING_METRES;
    let octaves = air + OCCLUSION_OCTAVES * occlusion;
    Heard {
        gain,
        pan: if pan.is_finite() { pan } else { 0.0 },
        pitch: if pitch.is_finite() { pitch } else { 1.0 },
        cutoff: NO_FILTER_HZ * (-octaves).exp2(),
    }
}

/// A region with its own reverb: an axis-aligned box that fades out over `fade` metres
/// around it. Blend reverb sends by where the listener is:
///
/// ```
/// # use engine_audio::spatial::ReverbZone;
/// # use engine_core::math::Vec3;
/// let cave = ReverbZone { center: Vec3::new(0.0, 0.0, -40.0), half_extents: Vec3::splat(10.0), fade: 5.0 };
/// let w = cave.weight(Vec3::new(0.0, 0.0, -27.5)); // halfway into the fade
/// assert!((w - 0.5).abs() < 1e-6);
/// // audio.set_send_gain(world_to_cave, w, 0.1); // each frame, ramped
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReverbZone {
    /// Centre of the box.
    pub center: Vec3,
    /// Half its size along each axis.
    pub half_extents: Vec3,
    /// Distance outside the box over which the weight falls from 1 to 0.
    pub fade: f32,
}

impl ReverbZone {
    /// 1 inside the box, falling linearly to 0 at `fade` outside it.
    pub fn weight(&self, point: Vec3) -> f32 {
        let outside = ((point - self.center).abs() - self.half_extents).max(Vec3::ZERO);
        let distance = outside.length();
        if self.fade <= 0.0 {
            return if distance == 0.0 { 1.0 } else { 0.0 };
        }
        (1.0 - distance / self.fade).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    #[test]
    fn inverse_rolloff_halves_per_doubling_and_holds_beyond_the_maximum() {
        let a = |d| attenuation(d, 2.0, 50.0, Rolloff::Inverse);
        assert_eq!(a(0.5), 1.0);
        assert_eq!(a(2.0), 1.0);
        assert!((a(4.0) - 0.5).abs() < 1e-6);
        assert!((a(8.0) - 0.25).abs() < 1e-6);
        assert!((a(50.0) - 0.04).abs() < 1e-6);
        assert_eq!(a(500.0), a(50.0));
    }

    #[test]
    fn linear_rolloff_reaches_silence_at_the_maximum() {
        let a = |d| attenuation(d, 10.0, 30.0, Rolloff::Linear);
        assert_eq!(a(5.0), 1.0);
        assert!((a(20.0) - 0.5).abs() < 1e-6);
        assert_eq!(a(30.0), 0.0);
        assert_eq!(a(1_000.0), 0.0);
        assert_eq!(attenuation(3.0, 5.0, 5.0, Rolloff::Linear), 1.0);
        assert_eq!(attenuation(6.0, 5.0, 5.0, Rolloff::Linear), 0.0);
    }

    #[test]
    fn pan_follows_the_listeners_right_and_narrows_inside_the_sound() {
        let listener = Listener::default();
        let right = hear(&listener, &Emitter::at(Vec3::new(5.0, 0.0, 0.0)), 0.0);
        assert!((right.pan - 1.0).abs() < 1e-6);
        let front = hear(&listener, &Emitter::at(Vec3::new(0.0, 0.0, -5.0)), 0.0);
        assert!(front.pan.abs() < 1e-6);
        // Turned to face +X, the same source is in front.
        let turned = Listener {
            forward: Vec3::X,
            ..listener
        };
        let ahead = hear(&turned, &Emitter::at(Vec3::new(5.0, 0.0, 0.0)), 0.0);
        assert!(ahead.pan.abs() < 1e-6);
        // Inside a 4 m sound, 1 m to the left: a quarter of the way to the left.
        let big = Emitter {
            min_distance: 4.0,
            ..Emitter::at(Vec3::new(-1.0, 0.0, 0.0))
        };
        assert!((hear(&listener, &big, 0.0).pan + 0.25).abs() < 1e-6);
        // At the listener: centred, no NaN.
        let here = hear(&listener, &Emitter::default(), 0.0);
        assert_eq!((here.pan, here.pitch, here.gain), (0.0, 1.0, 1.0));
    }

    #[test]
    fn doppler_raises_approaching_and_lowers_receding_sounds() {
        let listener = Listener::default();
        let at = Vec3::new(0.0, 0.0, -50.0);
        let approaching = Emitter {
            velocity: Vec3::new(0.0, 0.0, 34.3),
            ..Emitter::at(at)
        };
        // Source moving towards the listener at 0.1 c: 1 / 0.9.
        let p = hear(&listener, &approaching, 0.0).pitch;
        assert!((p - 1.0 / 0.9).abs() < 1e-5, "{p}");
        let receding = Emitter {
            velocity: -approaching.velocity,
            ..approaching
        };
        assert!((hear(&listener, &receding, 0.0).pitch - 1.0 / 1.1).abs() < 1e-5);
        // Listener moving towards the source at 0.1 c: 1.1.
        let moving = Listener {
            velocity: Vec3::new(0.0, 0.0, -34.3),
            ..listener
        };
        assert!((hear(&moving, &Emitter::at(at), 0.0).pitch - 1.1).abs() < 1e-5);
        // Sideways motion does not shift the pitch; strength 0 disables it.
        let passing = Emitter {
            velocity: Vec3::new(100.0, 0.0, 0.0),
            ..Emitter::at(at)
        };
        assert!((hear(&listener, &passing, 0.0).pitch - 1.0).abs() < 1e-6);
        let off = Emitter {
            doppler: 0.0,
            ..approaching
        };
        assert_eq!(hear(&listener, &off, 0.0).pitch, 1.0);
    }

    #[test]
    fn air_and_occlusion_lower_the_cutoff_and_occlusion_the_gain() {
        let listener = Listener::default();
        let near = hear(&listener, &Emitter::at(Vec3::new(0.0, 0.0, -1.0)), 0.0);
        assert!(near.cutoff > 19_000.0);
        let far = Emitter {
            rolloff: Rolloff::Linear,
            max_distance: 1_000.0,
            ..Emitter::at(Vec3::new(0.0, 0.0, -100.0))
        };
        assert!((hear(&listener, &far, 0.0).cutoff - 5_000.0).abs() < 1.0);
        let occluded = hear(&listener, &Emitter::at(Vec3::new(0.0, 0.0, -1.0)), 1.0);
        assert!((occluded.gain - 10f32.powf(-0.75)).abs() < 1e-6);
        assert!(occluded.cutoff < 700.0);
        let dry = Emitter {
            air_absorption: 0.0,
            ..far
        };
        assert_eq!(hear(&listener, &dry, 0.0).cutoff, NO_FILTER_HZ);
    }

    #[test]
    fn reverb_zone_weight_is_one_inside_and_fades_outside() {
        let zone = ReverbZone {
            center: Vec3::new(10.0, 0.0, 0.0),
            half_extents: Vec3::new(5.0, 3.0, 5.0),
            fade: 4.0,
        };
        assert_eq!(zone.weight(Vec3::new(12.0, 1.0, -4.0)), 1.0);
        assert!((zone.weight(Vec3::new(17.0, 0.0, 0.0)) - 0.5).abs() < 1e-6);
        assert_eq!(zone.weight(Vec3::new(30.0, 0.0, 0.0)), 0.0);
        let hard = ReverbZone { fade: 0.0, ..zone };
        assert_eq!(hard.weight(Vec3::new(15.0, 0.0, 0.0)), 1.0);
        assert_eq!(hard.weight(Vec3::new(15.1, 0.0, 0.0)), 0.0);
    }
}
