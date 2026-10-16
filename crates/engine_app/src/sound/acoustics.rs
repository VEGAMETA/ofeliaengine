//! Sound through the world (ADR 0105): walls between a sound and the listener muffle it,
//! and the room around the listener gives the reverb. Both cast rays through the game's
//! physics world ([`crate::Game::acoustics`]), a budget of them each frame.
//!
//! - **Occlusion:** a ray from the listener to the sound; each body it crosses on the way
//!   (not the listener's, not the sound's own) takes 60 % more of what is left: one wall
//!   0.6, two 0.84 (the mixer's occlusion: 15 dB quieter and muffled to about 600 Hz at 1).
//! - **The room:** [`PROBE_RAYS`] rays from the listener in every direction, a few each
//!   frame. From what they hit:
//!   - the mean free path (1.2 × the mean distance to the walls) and the walls'
//!     absorption (open sky absorbing all) give the decay by Sabine's formula,
//!     RT60 ≈ 0.04 · path / absorption;
//!   - the path gives the size, the nearest wall the pre-delay, the walls' absorption the
//!     damping;
//!   - the share of upward rays that reach no wall (open sky) takes the reverb's level
//!     down: a hall answers, a field does not.

use super::components::Room;
use engine_core::math::Vec3;
use engine_physics::{PhysicsBackend, QueryFilter, Ray, RayHit};

/// Directions the room probe looks in.
pub(crate) const PROBE_RAYS: usize = 32;
/// Probe rays a frame at most: the room is looked at whole every 4 frames.
pub(crate) const PROBE_RAYS_PER_FRAME: usize = 8;

/// The room probe's directions and what each saw, and scratch for occlusion rays.
pub(crate) struct Acoustics {
    directions: [Vec3; PROBE_RAYS],
    /// Per direction: the distance to the wall and its absorption, or `None` for open sky
    /// (nothing within the probe's reach). Not looked at yet: `None` and not `seen`.
    found: [Option<(f32, f32)>; PROBE_RAYS],
    seen: [bool; PROBE_RAYS],
    next: usize,
    hits: Vec<RayHit>,
}

impl Acoustics {
    pub(crate) fn new() -> Self {
        // A Fibonacci sphere: even directions, none repeated.
        let golden = core::f32::consts::PI * (3.0 - 5f32.sqrt());
        let directions = core::array::from_fn(|i| {
            let y = 1.0 - 2.0 * (i as f32 + 0.5) / PROBE_RAYS as f32;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let phi = golden * i as f32;
            Vec3::new(phi.cos() * r, y, phi.sin() * r)
        });
        Self {
            directions,
            found: [None; PROBE_RAYS],
            seen: [false; PROBE_RAYS],
            next: 0,
            hits: Vec::with_capacity(16),
        }
    }

    /// Forgets what the probe saw (a new level, a load).
    pub(crate) fn reset(&mut self) {
        self.found = [None; PROBE_RAYS];
        self.seen = [false; PROBE_RAYS];
    }

    /// Casts up to `rays` of the probe's rays from `at`, `reach` metres long; returns how
    /// many it cast. `absorption` says what a hit's surface absorbs.
    pub(crate) fn probe(
        &mut self,
        physics: &dyn PhysicsBackend,
        at: Vec3,
        reach: f32,
        rays: usize,
        filter: QueryFilter,
        absorption: &dyn Fn(&RayHit) -> f32,
    ) -> usize {
        let n = rays.min(PROBE_RAYS_PER_FRAME);
        if n == 0 {
            return 0;
        }
        let mut batch = [Ray {
            origin: at,
            direction: Vec3::ZERO,
        }; PROBE_RAYS_PER_FRAME];
        let mut index = [0usize; PROBE_RAYS_PER_FRAME];
        for k in 0..n {
            let i = (self.next + k) % PROBE_RAYS;
            index[k] = i;
            batch[k].direction = self.directions[i] * reach;
        }
        let mut out = [None; PROBE_RAYS_PER_FRAME];
        physics.raycast_batch(&batch[..n], filter, &mut out[..n]);
        for k in 0..n {
            let i = index[k];
            self.seen[i] = true;
            self.found[i] = out[k].map(|hit| ((hit.fraction * reach).max(0.05), absorption(&hit)));
        }
        self.next = (self.next + n) % PROBE_RAYS;
        n
    }

    /// The room the probe has seen; `None` before it has looked anywhere.
    pub(crate) fn room(&self) -> Option<Room> {
        let mut looked = 0u32;
        let (mut hits, mut distance, mut absorbed, mut nearest) = (0u32, 0.0f32, 0.0f32, f32::MAX);
        let (mut up, mut up_open) = (0u32, 0u32);
        for (i, found) in self.found.iter().enumerate() {
            if !self.seen[i] {
                continue;
            }
            looked += 1;
            let upward = self.directions[i].y > -0.2;
            up += u32::from(upward);
            match *found {
                Some((d, a)) => {
                    hits += 1;
                    distance += d;
                    absorbed += a;
                    nearest = nearest.min(d);
                }
                None => up_open += u32::from(upward),
            }
        }
        if looked == 0 {
            return None;
        }
        if hits == 0 {
            return Some(Room {
                level: 0.0,
                ..Room::OUTDOORS
            });
        }
        let open = (looked - hits) as f32;
        let path = 1.2 * distance / hits as f32;
        let walls = absorbed / hits as f32;
        // Open sky absorbs everything that reaches it.
        let absorption = ((absorbed + open) / looked as f32).max(0.01);
        let enclosed = 1.0 - up_open as f32 / up.max(1) as f32;
        Some(Room {
            decay: (0.04 * path / absorption).clamp(0.2, 8.0),
            size: ((path - 2.0) / 20.0).clamp(0.0, 1.0),
            damping: (9_000.0 * (1.0 - walls).powi(2)).clamp(1_500.0, 12_000.0),
            pre_delay: (2.0 * nearest / 343.0).clamp(0.002, 0.1),
            level: 0.35 * enclosed * enclosed,
        })
    }

    /// How much the bodies between `from` (the listener) and `to` (a sound) muffle it,
    /// 0..1: each body crossed on the way takes 60 % more of what is left (at most four
    /// counted).
    pub(crate) fn occlusion(
        &mut self,
        physics: &dyn PhysicsBackend,
        from: Vec3,
        to: Vec3,
        filter: QueryFilter,
    ) -> f32 {
        let way = to - from;
        let length = way.length();
        if length < 0.5 {
            return 0.0;
        }
        self.hits.clear();
        physics.raycast_all(
            &Ray {
                origin: from,
                direction: way,
            },
            filter,
            &mut self.hits,
        );
        // Not the listener's own body (a hit where the ray starts) nor the sound's (a hit
        // where it ends).
        let walls = self
            .hits
            .iter()
            .filter(|hit| {
                let at = hit.fraction * length;
                at > 0.05 && at < length - 0.25
            })
            .count()
            .min(4);
        1.0 - 0.4f32.powi(walls as i32)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    fn probe_with(f: impl Fn(Vec3) -> Option<(f32, f32)>) -> Acoustics {
        let mut a = Acoustics::new();
        for i in 0..PROBE_RAYS {
            a.seen[i] = true;
            a.found[i] = f(a.directions[i]);
        }
        a
    }

    #[test]
    fn directions_cover_the_sphere_evenly() {
        let a = Acoustics::new();
        // Balanced: the sum of 32 unit vectors stays near zero.
        let sum: Vec3 = a.directions.iter().copied().sum();
        assert!(sum.length() < 0.1, "{sum:?}");
        for d in a.directions {
            assert!((d.length() - 1.0).abs() < 1e-5);
        }
        let up = a.directions.iter().filter(|d| d.y > 0.0).count();
        assert_eq!(up, PROBE_RAYS / 2);
    }

    #[test]
    fn a_closed_stone_hall_rings_and_open_ground_is_dry() {
        // 12 m to every wall, stone (2 % absorbed): long and loud.
        let hall = probe_with(|_| Some((12.0, 0.02))).room().unwrap();
        assert!(hall.decay > 4.0, "{hall:?}");
        assert!((hall.level - 0.35).abs() < 1e-6);
        assert_eq!(hall.size, (1.2f32 * 12.0 - 2.0) / 20.0);
        // A small furnished room: short.
        let room = probe_with(|_| Some((2.0, 0.3))).room().unwrap();
        assert!(room.decay < 0.5 && room.decay < hall.decay, "{room:?}");
        assert!(room.damping < hall.damping);
        // Ground below, sky above: almost no reverb.
        let field = probe_with(|d| (d.y < -0.2).then_some((4.0, 0.1)))
            .room()
            .unwrap();
        assert!(field.level < 0.01, "{field:?}");
        // Nothing anywhere: none.
        let sky = probe_with(|_| None).room().unwrap();
        assert_eq!(sky.level, 0.0);
        // Not looked yet: no estimate.
        assert!(Acoustics::new().room().is_none());
    }
}
