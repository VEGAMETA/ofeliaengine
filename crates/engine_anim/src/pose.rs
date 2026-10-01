//! Joint transforms and poses.

use engine_core::math::{Affine3A, Quat, Vec3, Vec3A};

/// A joint's transform relative to its parent: scale, then rotation, then translation
/// (glTF's TRS).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// Translation.
    pub translation: Vec3A,
    /// Rotation (unit quaternion).
    pub rotation: Quat,
    /// Scale per axis.
    pub scale: Vec3A,
}

impl Transform {
    /// No translation, rotation or scale.
    pub const IDENTITY: Self = Self {
        translation: Vec3A::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3A::ONE,
    };

    /// A transform from its parts.
    pub fn new(translation: Vec3, rotation: Quat, scale: Vec3) -> Self {
        Self {
            translation: translation.into(),
            rotation,
            scale: scale.into(),
        }
    }

    /// The affine matrix: `T · R · S`.
    #[inline]
    pub fn to_affine(&self) -> Affine3A {
        Affine3A::from_scale_rotation_translation(
            self.scale.into(),
            self.rotation,
            self.translation.into(),
        )
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The local transforms of every joint of a skeleton, one array per component
/// (structure of arrays: blending and sampling stream through each array).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pose {
    /// Translations, by joint.
    pub translations: Vec<Vec3A>,
    /// Rotations (unit quaternions), by joint.
    pub rotations: Vec<Quat>,
    /// Scales, by joint.
    pub scales: Vec<Vec3A>,
}

impl Pose {
    /// `joints` identity transforms.
    pub fn identity(joints: usize) -> Self {
        Self {
            translations: vec![Vec3A::ZERO; joints],
            rotations: vec![Quat::IDENTITY; joints],
            scales: vec![Vec3A::ONE; joints],
        }
    }

    /// A pose from transforms.
    pub fn from_transforms(transforms: &[Transform]) -> Self {
        Self {
            translations: transforms.iter().map(|t| t.translation).collect(),
            rotations: transforms.iter().map(|t| t.rotation).collect(),
            scales: transforms.iter().map(|t| t.scale).collect(),
        }
    }

    /// Joints in the pose.
    #[inline]
    pub fn len(&self) -> usize {
        self.rotations.len()
    }

    /// Whether the pose has no joints.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rotations.is_empty()
    }

    /// Joint `j`'s transform (identity past the end).
    #[inline]
    pub fn get(&self, j: usize) -> Transform {
        match (
            self.translations.get(j),
            self.rotations.get(j),
            self.scales.get(j),
        ) {
            (Some(&translation), Some(&rotation), Some(&scale)) => Transform {
                translation,
                rotation,
                scale,
            },
            _ => Transform::IDENTITY,
        }
    }

    /// Sets joint `j`'s transform (ignored past the end).
    #[inline]
    pub fn set(&mut self, j: usize, t: Transform) {
        if let (Some(tr), Some(r), Some(s)) = (
            self.translations.get_mut(j),
            self.rotations.get_mut(j),
            self.scales.get_mut(j),
        ) {
            *tr = t.translation;
            *r = t.rotation;
            *s = t.scale;
        }
    }

    /// Copies `other` into this pose without allocating when the lengths match.
    pub fn copy_from(&mut self, other: &Pose) {
        self.translations.clone_from(&other.translations);
        self.rotations.clone_from(&other.rotations);
        self.scales.clone_from(&other.scales);
    }

    /// The additive delta of this pose over `reference` (both of one skeleton):
    /// translation difference, rotation `reference⁻¹ · pose`, scale ratio. Applied by an
    /// additive layer of [`crate::Blender`] to bring `reference` back to this pose.
    pub fn delta_from(&self, reference: &Pose, out: &mut Pose) {
        let n = self.len().min(reference.len()).min(out.len());
        for j in 0..n {
            out.translations[j] = self.translations[j] - reference.translations[j];
            out.rotations[j] = (reference.rotations[j].conjugate() * self.rotations[j]).normalize();
            out.scales[j] = self.scales[j] / safe_scale(reference.scales[j]);
        }
    }
}

/// `s` with components too close to zero replaced by a tiny signed value (no division by
/// zero in ratios).
#[inline]
pub(crate) fn safe_scale(s: Vec3A) -> Vec3A {
    let tiny = Vec3A::splat(1e-12);
    Vec3A::select(s.abs().cmplt(tiny), tiny.copysign(s), s)
}

/// `q` or `-q`, whichever lies in the same hemisphere as `reference` (the two are the
/// same rotation; interpolating between neighbours takes the short way round).
#[inline]
pub(crate) fn same_hemisphere(q: Quat, reference: Quat) -> Quat {
    if q.dot(reference) < 0.0 { -q } else { q }
}

/// The angle between two rotations in radians, accurate for small angles too
/// (`2 · atan2(|v|, |w|)` of the relative rotation; glam's `angle_between` goes through an
/// approximate arc cosine).
pub fn rotation_angle(a: Quat, b: Quat) -> f32 {
    let d = a.conjugate() * b;
    2.0 * Vec3::new(d.x, d.y, d.z).length().atan2(d.w.abs())
}

/// Normalized linear interpolation of rotations along the shorter arc.
#[inline]
pub fn nlerp(a: Quat, b: Quat, t: f32) -> Quat {
    let b = same_hemisphere(b, a);
    let v = engine_core::math::Vec4::from(a).lerp(engine_core::math::Vec4::from(b), t);
    Quat::from_vec4(v).normalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_round_trips() {
        let a = Pose::from_transforms(&[Transform::new(
            Vec3::new(1.0, 2.0, 3.0),
            Quat::from_rotation_y(0.4),
            Vec3::new(1.0, 2.0, 0.5),
        )]);
        let b = Pose::from_transforms(&[Transform::new(
            Vec3::new(-1.0, 0.5, 3.0),
            Quat::from_rotation_x(1.1),
            Vec3::new(2.0, 2.0, 1.0),
        )]);
        let mut d = Pose::identity(1);
        b.delta_from(&a, &mut d);
        let back = a.rotations[0] * d.rotations[0];
        assert!(rotation_angle(back, b.rotations[0]) < 1e-5);
        assert!((a.translations[0] + d.translations[0] - b.translations[0]).length() < 1e-6);
        assert!((a.scales[0] * d.scales[0] - b.scales[0]).length() < 1e-6);
    }

    #[test]
    fn nlerp_takes_the_short_way() {
        let a = Quat::from_rotation_z(0.1);
        let b = -Quat::from_rotation_z(0.3);
        let m = nlerp(a, b, 0.5);
        assert!(rotation_angle(m, Quat::from_rotation_z(0.2)) < 1e-5);
    }
}
