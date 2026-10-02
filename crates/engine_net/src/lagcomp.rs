//! Lag compensation (ADR 0019, ADR 0074): the server keeps a ring of per-tick hit shapes
//! and answers "what did this ray hit at time t" against the shapes interpolated to t, as
//! the shooter saw them. The live world (physics, animation) is never moved back.
//!
//! The server records the shapes after every tick ([`crate::Server::send`] does it for
//! entities with a [`HitCapsule`] and a `Transform`); a hit test passes the shooter's
//! view time from its input ([`crate::InputHeader::view_time`]).

use engine_core::math::{Quat, Vec3};
use engine_ecs::{Entity, bytemuck, pod_component};

/// A hit volume in the entity's local space (a capsule from `a` to `b`; a sphere when
/// they are equal), recorded for lag compensation.
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct HitCapsule {
    /// One end of the segment (local).
    pub a: Vec3,
    /// The other end (local).
    pub b: Vec3,
    /// Radius.
    pub radius: f32,
}
pod_component!(HitCapsule);

impl HitCapsule {
    /// An upright capsule `height` tall with `radius`, standing on the local origin (a
    /// character whose position is its feet).
    pub fn upright(height: f32, radius: f32) -> Self {
        Self {
            a: Vec3::new(0.0, radius, 0.0),
            b: Vec3::new(0.0, (height - radius).max(radius), 0.0),
            radius,
        }
    }
}

/// A lag-compensated hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LagHit {
    /// The entity hit.
    pub entity: Entity,
    /// Distance along the ray.
    pub distance: f32,
    /// Where the ray entered the shape (at the rewound time).
    pub point: Vec3,
}

#[derive(Debug, Clone, Copy)]
struct Shape {
    entity: Entity,
    a: Vec3,
    b: Vec3,
    radius: f32,
}

#[derive(Debug, Default)]
struct TickShapes {
    tick: u32,
    valid: bool,
    /// Sorted by entity.
    shapes: Vec<Shape>,
}

/// The ring of recorded hit shapes.
#[derive(Debug)]
pub struct LagCompensation {
    ring: Vec<TickShapes>,
    newest: Option<u32>,
}

impl LagCompensation {
    /// A ring holding `ticks` ticks (one second at the tick rate is the usual limit).
    pub fn new(ticks: u32) -> Self {
        Self {
            ring: (0..ticks.max(2)).map(|_| TickShapes::default()).collect(),
            newest: None,
        }
    }

    /// Ticks the ring holds.
    pub fn capacity(&self) -> u32 {
        self.ring.len() as u32
    }

    /// Starts recording `tick` (replacing the oldest); add its shapes with
    /// [`Self::add`].
    pub fn begin(&mut self, tick: u32) {
        let n = self.ring.len() as u32;
        let slot = &mut self.ring[(tick % n) as usize];
        slot.tick = tick;
        slot.valid = true;
        slot.shapes.clear();
        self.newest = Some(tick);
    }

    /// Adds `entity`'s capsule at `position`, `rotation` to the tick being recorded.
    /// Entities must be added in increasing order (as an ECS query visits them sorted, or
    /// sort first) for the lookups between ticks.
    pub fn add(&mut self, entity: Entity, capsule: &HitCapsule, position: Vec3, rotation: Quat) {
        let Some(tick) = self.newest else { return };
        let n = self.ring.len() as u32;
        let slot = &mut self.ring[(tick % n) as usize];
        slot.shapes.push(Shape {
            entity,
            a: position + rotation * capsule.a,
            b: position + rotation * capsule.b,
            radius: capsule.radius,
        });
    }

    /// Sorts the tick's shapes by entity (call once after adding in arbitrary order).
    pub fn finish(&mut self) {
        let Some(tick) = self.newest else { return };
        let n = self.ring.len() as u32;
        self.ring[(tick % n) as usize]
            .shapes
            .sort_unstable_by_key(|s| s.entity);
    }

    fn at(&self, tick: u32) -> Option<&TickShapes> {
        let n = self.ring.len() as u32;
        let s = &self.ring[(tick % n) as usize];
        (s.valid && s.tick == tick).then_some(s)
    }

    /// The closest shape hit by the ray from `origin` along unit `dir` within `max`, with
    /// every shape interpolated to `time` (in ticks, fractional; clamped to what the
    /// ring holds), ignoring `ignore`.
    pub fn raycast(
        &self,
        origin: Vec3,
        dir: Vec3,
        max: f32,
        time: f64,
        ignore: Option<Entity>,
    ) -> Option<LagHit> {
        let newest = self.newest?;
        let oldest = newest.saturating_sub(self.ring.len() as u32 - 1);
        let time = time.clamp(f64::from(oldest), f64::from(newest));
        let t0 = time.floor() as u32;
        let t1 = (t0 + 1).min(newest);
        let alpha = (time - f64::from(t0)) as f32;
        let a = self.at(t0).or_else(|| self.at(t1))?;
        let b = self.at(t1).unwrap_or(a);
        let mut best: Option<LagHit> = None;
        for s in &a.shapes {
            if Some(s.entity) == ignore {
                continue;
            }
            // The same entity one tick later, if it existed then.
            let (p, q) = match b.shapes.binary_search_by_key(&s.entity, |x| x.entity) {
                Ok(i) => {
                    let n = &b.shapes[i];
                    (s.a.lerp(n.a, alpha), s.b.lerp(n.b, alpha))
                }
                Err(_) => (s.a, s.b),
            };
            if let Some(d) = ray_capsule(origin, dir, p, q, s.radius)
                && d <= max
                && best.is_none_or(|h| d < h.distance)
            {
                best = Some(LagHit {
                    entity: s.entity,
                    distance: d,
                    point: origin + dir * d,
                });
            }
        }
        best
    }

    /// The recorded segment of `entity`'s capsule at `time` (for tests and debug drawing).
    pub fn shape_at(&self, entity: Entity, time: f64) -> Option<(Vec3, Vec3, f32)> {
        let t0 = time.floor() as u32;
        let alpha = (time - f64::from(t0)) as f32;
        let find = |t: u32| {
            self.at(t).and_then(|s| {
                s.shapes
                    .binary_search_by_key(&entity, |x| x.entity)
                    .ok()
                    .map(|i| s.shapes[i])
            })
        };
        let a = find(t0)?;
        let b = find(t0 + 1).unwrap_or(a);
        Some((a.a.lerp(b.a, alpha), a.b.lerp(b.b, alpha), a.radius))
    }
}

/// Distance along a unit ray to a capsule, if it hits (0 when starting inside).
pub(crate) fn ray_capsule(o: Vec3, d: Vec3, a: Vec3, b: Vec3, r: f32) -> Option<f32> {
    // Ericson, Real-Time Collision Detection §5.3.7 (segment vs cylinder), with the caps
    // as spheres; adequate for hit volumes.
    let ab = b - a;
    let ao = o - a;
    let len2 = ab.length_squared();
    let mut best = f32::INFINITY;
    if len2 > 1e-12 {
        // Infinite cylinder about AB.
        let abd = ab.dot(d);
        let abo = ab.dot(ao);
        let qa = len2 - abd * abd;
        let qb = len2 * ao.dot(d) - abo * abd;
        let qc = len2 * ao.length_squared() - abo * abo - r * r * len2;
        if qc <= 0.0 && (0.0..=len2).contains(&abo) {
            return Some(0.0); // starts inside the cylinder part
        }
        if qa.abs() > 1e-12 {
            let disc = qb * qb - qa * qc;
            if disc >= 0.0 {
                let t = (-qb - disc.sqrt()) / qa;
                let along = abo + t * abd;
                if t >= 0.0 && (0.0..=len2).contains(&along) {
                    best = t;
                }
            }
        }
    }
    for c in [a, b] {
        if let Some(t) = ray_sphere(o, d, c, r) {
            best = best.min(t);
        }
    }
    best.is_finite().then_some(best)
}

fn ray_sphere(o: Vec3, d: Vec3, c: Vec3, r: f32) -> Option<f32> {
    let m = o - c;
    let b = m.dot(d);
    let c2 = m.length_squared() - r * r;
    if c2 > 0.0 && b > 0.0 {
        return None;
    }
    let disc = b * b - c2;
    if disc < 0.0 {
        return None;
    }
    Some((-b - disc.sqrt()).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rays_hit_capsules_like_the_analytic_answer() {
        let (a, b) = (Vec3::new(0.0, 0.5, 0.0), Vec3::new(0.0, 1.5, 0.0));
        // From the side at mid height: the cylinder.
        let d = ray_capsule(Vec3::new(-5.0, 1.0, 0.0), Vec3::X, a, b, 0.5).unwrap_or(-1.0);
        assert!((d - 4.5).abs() < 1e-5, "{d}");
        // From above: the top cap.
        let d = ray_capsule(Vec3::new(0.0, 5.0, 0.0), -Vec3::Y, a, b, 0.5).unwrap_or(-1.0);
        assert!((d - 3.0).abs() < 1e-5, "{d}");
        // A miss, and a ray pointing away.
        assert!(ray_capsule(Vec3::new(-5.0, 1.0, 0.6), Vec3::X, a, b, 0.5).is_none());
        assert!(ray_capsule(Vec3::new(-5.0, 1.0, 0.0), -Vec3::X, a, b, 0.5).is_none());
        // Inside.
        assert_eq!(
            ray_capsule(Vec3::new(0.0, 1.0, 0.0), Vec3::X, a, b, 0.5),
            Some(0.0)
        );
    }

    #[test]
    fn rewinds_to_the_shapes_of_a_fractional_tick() {
        let mut lag = LagCompensation::new(64);
        let e = Entity::from_bits(5);
        let capsule = HitCapsule::upright(1.8, 0.3);
        for tick in 0..100u32 {
            lag.begin(tick);
            // Moves 0.1 m per tick along x.
            lag.add(
                e,
                &capsule,
                Vec3::new(tick as f32 * 0.1, 0.0, 0.0),
                Quat::IDENTITY,
            );
            lag.finish();
        }
        // At tick 50.5 the capsule is at x = 5.05: a ray along z at x = 5.05 hits, one at
        // x = 9.9 (where it is now) does not at that time.
        let hit = lag.raycast(Vec3::new(5.05, 1.0, -5.0), Vec3::Z, 100.0, 50.5, None);
        assert!(hit.is_some_and(|h| h.entity == e && (h.distance - 4.7).abs() < 1e-4));
        assert!(
            lag.raycast(Vec3::new(9.9, 1.0, -5.0), Vec3::Z, 100.0, 50.5, None)
                .is_none()
        );
        assert!(
            lag.raycast(Vec3::new(5.05, 1.0, -5.0), Vec3::Z, 100.0, 50.5, Some(e))
                .is_none()
        );
        // Older than the ring: clamped to the oldest tick held (36).
        let hit = lag.raycast(Vec3::new(3.6, 1.0, -5.0), Vec3::Z, 100.0, 10.0, None);
        assert!(hit.is_some());
    }
}
