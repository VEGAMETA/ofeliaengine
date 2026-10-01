//! Source animation as authored (glTF channels): keyframes per joint and component, with
//! glTF's interpolation. [`RawClip::sample_joint`] is the reference the compressor and
//! the tests measure against.

use crate::pose::{Pose, Transform};
use crate::skeleton::Skeleton;
use engine_core::math::{Quat, Vec3, Vec4};

/// How values between keys are found (glTF's `interpolation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Interpolation {
    /// The previous key's value.
    Step,
    /// Linear (spherical for rotations).
    #[default]
    Linear,
    /// Cubic Hermite spline; each key has an in-tangent, a value and an out-tangent.
    CubicSpline,
}

/// Keys of one component of one joint.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawTrack<T> {
    /// Interpolation between keys.
    pub interpolation: Interpolation,
    /// Key times in seconds, increasing.
    pub times: Vec<f32>,
    /// One value per key, or for [`Interpolation::CubicSpline`] three: in-tangent, value,
    /// out-tangent (glTF's layout).
    pub values: Vec<T>,
}

/// The keyed components of one joint (`None`: the rest pose's).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawJointTracks {
    /// Translation keys.
    pub translation: Option<RawTrack<Vec3>>,
    /// Rotation keys (x, y, z, w).
    pub rotation: Option<RawTrack<Quat>>,
    /// Scale keys.
    pub scale: Option<RawTrack<Vec3>>,
}

/// An animation of one skeleton, as authored.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawClip {
    /// Name.
    pub name: String,
    /// Length in seconds (the last key's time).
    pub duration: f32,
    /// Tracks by skeleton joint (as many as the skeleton has joints).
    pub joints: Vec<RawJointTracks>,
}

/// Values that can be keyed: [`Vec3`] (translations, scales) and [`Quat`] (rotations).
pub trait Keyed: Copy {
    /// As four floats (w = 0 for vectors).
    fn to_vec4(self) -> Vec4;
    /// From four floats.
    fn from_vec4(v: Vec4) -> Self;
    /// Linear interpolation (spherical for rotations).
    fn interpolate(a: Self, b: Self, t: f32) -> Self;
    /// After a cubic spline (rotations are normalized).
    fn finish(self) -> Self {
        self
    }
}

impl Keyed for Vec3 {
    fn to_vec4(self) -> Vec4 {
        self.extend(0.0)
    }
    fn from_vec4(v: Vec4) -> Self {
        v.truncate()
    }
    fn interpolate(a: Self, b: Self, t: f32) -> Self {
        a.lerp(b, t)
    }
}

impl Keyed for Quat {
    fn to_vec4(self) -> Vec4 {
        Vec4::from(self)
    }
    fn from_vec4(v: Vec4) -> Self {
        Quat::from_vec4(v)
    }
    fn interpolate(a: Self, b: Self, t: f32) -> Self {
        a.slerp(b, t)
    }
    fn finish(self) -> Self {
        self.normalize()
    }
}

impl<T: Keyed> RawTrack<T> {
    /// The value at `time` (clamped to the first and last keys), or `None` without keys.
    pub(crate) fn sample(&self, time: f32) -> Option<T> {
        let n = self.times.len();
        let stride = if self.interpolation == Interpolation::CubicSpline {
            3
        } else {
            1
        };
        if n == 0 || self.values.len() < n * stride {
            return None;
        }
        let value = |k: usize| self.values[k * stride + stride / 2];
        if time <= self.times[0] || n == 1 {
            return Some(value(0));
        }
        if time >= self.times[n - 1] {
            return Some(value(n - 1));
        }
        // The last key at or before `time`.
        let k = self.times.partition_point(|&t| t <= time).saturating_sub(1);
        let (t0, t1) = (self.times[k], self.times[k + 1]);
        let dt = t1 - t0;
        let s = if dt > 0.0 { (time - t0) / dt } else { 0.0 };
        Some(match self.interpolation {
            Interpolation::Step => value(k),
            Interpolation::Linear => T::interpolate(value(k), value(k + 1), s),
            Interpolation::CubicSpline => {
                let p0 = value(k).to_vec4();
                let m0 = self.values[k * 3 + 2].to_vec4() * dt;
                let p1 = value(k + 1).to_vec4();
                let m1 = self.values[(k + 1) * 3].to_vec4() * dt;
                let (s2, s3) = (s * s, s * s * s);
                let v = p0 * (2.0 * s3 - 3.0 * s2 + 1.0)
                    + m0 * (s3 - 2.0 * s2 + s)
                    + p1 * (-2.0 * s3 + 3.0 * s2)
                    + m1 * (s3 - s2);
                T::from_vec4(v).finish()
            }
        })
    }

    /// The smallest time between two keys (`None` with fewer than two).
    pub(crate) fn min_interval(&self) -> Option<f32> {
        self.times
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|&d| d > 1e-6)
            .min_by(f32::total_cmp)
    }
}

impl RawClip {
    /// Joint `j`'s local transform at `time`; components without keys come from `rest`.
    pub fn sample_joint(&self, j: usize, time: f32, rest: Transform) -> Transform {
        let Some(tracks) = self.joints.get(j) else {
            return rest;
        };
        Transform {
            translation: tracks
                .translation
                .as_ref()
                .and_then(|t| t.sample(time))
                .map_or(rest.translation, Into::into),
            rotation: tracks
                .rotation
                .as_ref()
                .and_then(|t| t.sample(time))
                .map_or(rest.rotation, Quat::normalize),
            scale: tracks
                .scale
                .as_ref()
                .and_then(|t| t.sample(time))
                .map_or(rest.scale, Into::into),
        }
    }

    /// The whole pose at `time` (reference sampling: slow, exact).
    pub fn sample(&self, skeleton: &Skeleton, time: f32, out: &mut Pose) {
        for j in 0..skeleton.joint_count().min(out.len()) {
            out.set(j, self.sample_joint(j, time, skeleton.rest().get(j)));
        }
    }

    /// The smallest time between two keys of any track (`None` if no track has two keys).
    pub fn min_key_interval(&self) -> Option<f32> {
        let mut best: Option<f32> = None;
        let mut take = |d: Option<f32>| {
            if let Some(d) = d {
                best = Some(best.map_or(d, |b| b.min(d)));
            }
        };
        for j in &self.joints {
            take(j.translation.as_ref().and_then(RawTrack::min_interval));
            take(j.rotation.as_ref().and_then(RawTrack::min_interval));
            take(j.scale.as_ref().and_then(RawTrack::min_interval));
        }
        best
    }

    /// Keys stored in the source (all components, cubic tangents counted as values).
    pub fn source_bytes(&self) -> usize {
        fn bytes<T>(t: &Option<RawTrack<T>>, size: usize) -> usize {
            t.as_ref()
                .map_or(0, |t| t.times.len() * 4 + t.values.len() * size)
        }
        self.joints
            .iter()
            .map(|j| bytes(&j.translation, 12) + bytes(&j.rotation, 16) + bytes(&j.scale, 12))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::rotation_angle;

    #[test]
    fn interpolation_follows_gltf() {
        let linear = RawTrack {
            interpolation: Interpolation::Linear,
            times: vec![0.0, 1.0, 3.0],
            values: vec![Vec3::ZERO, Vec3::X, Vec3::new(3.0, 0.0, 0.0)],
        };
        assert_eq!(linear.sample(-1.0), Some(Vec3::ZERO));
        assert_eq!(linear.sample(0.5), Some(Vec3::new(0.5, 0.0, 0.0)));
        assert_eq!(linear.sample(2.0), Some(Vec3::new(2.0, 0.0, 0.0)));
        assert_eq!(linear.sample(9.0), Some(Vec3::new(3.0, 0.0, 0.0)));
        let step = RawTrack {
            interpolation: Interpolation::Step,
            ..linear.clone()
        };
        assert_eq!(step.sample(0.99), Some(Vec3::ZERO));
        assert_eq!(step.sample(1.0), Some(Vec3::X));
        // A cubic spline with zero tangents eases: smoothstep between the values.
        let cubic = RawTrack {
            interpolation: Interpolation::CubicSpline,
            times: vec![0.0, 2.0],
            values: vec![
                Vec3::ZERO,
                Vec3::ZERO,
                Vec3::ZERO,
                Vec3::ZERO,
                Vec3::Y,
                Vec3::ZERO,
            ],
        };
        let v = cubic.sample(0.5).unwrap();
        assert!((v.y - (3.0 * 0.0625 - 2.0 * 0.015625)).abs() < 1e-6, "{v}");
        // Tangents scale with the key interval: a linear ramp v(t) = t/2 has tangent 0.5.
        let ramp = RawTrack {
            interpolation: Interpolation::CubicSpline,
            times: vec![0.0, 2.0],
            values: vec![
                Vec3::splat(0.5),
                Vec3::ZERO,
                Vec3::splat(0.5),
                Vec3::splat(0.5),
                Vec3::ONE,
                Vec3::splat(0.5),
            ],
        };
        assert!((ramp.sample(0.7).unwrap().x - 0.35).abs() < 1e-6);
    }

    #[test]
    fn rotations_slerp() {
        let track = RawTrack {
            interpolation: Interpolation::Linear,
            times: vec![0.0, 1.0],
            values: vec![Quat::IDENTITY, Quat::from_rotation_y(2.0)],
        };
        let q = track.sample(0.25).unwrap();
        assert!(rotation_angle(q, Quat::from_rotation_y(0.5)) < 1e-5);
    }
}
