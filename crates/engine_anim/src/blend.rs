//! Blending poses: weighted layers with per-joint masks, and additive layers.
//!
//! Normal layers are averaged per joint by their weights (times the mask's weight of the
//! joint): translations and scales linearly, rotations by normalized weighted sums with
//! each rotation flipped into the first one's hemisphere (the weighted nlerp of ozz and
//! Unreal). Where the layers' total weight of a joint is below the threshold, the rest
//! pose fills the remainder, so a joint no layer drives stays at rest. Additive layers are
//! then applied on top in order: `translation += w·Δt`, `rotation = rotation ·
//! nlerp(1, Δr, w)`, `scale *= lerp(1, Δs, w)`.

use crate::pose::{Pose, same_hemisphere};
use engine_core::math::{Quat, Vec3A, Vec4};

/// How a layer is combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayerMode {
    /// Averaged with the other normal layers by weight.
    #[default]
    Normal,
    /// Applied on top of the normal layers' result (the pose holds deltas, as sampled
    /// from an additive clip or made by [`Pose::delta_from`]).
    Additive,
}

/// One input of [`Blender::blend`].
#[derive(Debug, Clone, Copy)]
pub struct Layer<'a> {
    /// The pose (local transforms of every joint).
    pub pose: &'a Pose,
    /// Weight of the whole layer (0 skips it).
    pub weight: f32,
    /// Per-joint weights multiplied with `weight` (an upper-body mask...), or `None` for
    /// all joints at 1.
    pub mask: Option<&'a [f32]>,
    /// How it combines.
    pub mode: LayerMode,
}

/// Blends layers into a pose. Holds the per-joint weight accumulators, so blending
/// allocates nothing once it has been made for the skeleton's joint count.
#[derive(Debug, Clone, Default)]
pub struct Blender {
    weights: Vec<f32>,
    rotations: Vec<Vec4>,
    /// Below this total weight, a joint is completed with the rest pose.
    pub threshold: f32,
}

impl Blender {
    /// A blender for poses of `joints` joints.
    pub fn new(joints: usize) -> Self {
        Self {
            weights: vec![0.0; joints],
            rotations: vec![Vec4::ZERO; joints],
            threshold: 0.1,
        }
    }

    /// Blends `layers` into `out` (see the module documentation); `rest` completes joints
    /// the normal layers drive too little. Every pose must have the joint count of this
    /// blender (joints past a shorter pose are left alone).
    pub fn blend(&mut self, rest: &Pose, layers: &[Layer<'_>], out: &mut Pose) {
        engine_profiler::zone!("anim::blend");
        let n = self.weights.len().min(out.len()).min(rest.len());
        let weights = &mut self.weights[..n];
        let rotations = &mut self.rotations[..n];
        weights.fill(0.0);
        rotations.fill(Vec4::ZERO);
        out.translations[..n].fill(Vec3A::ZERO);
        out.scales[..n].fill(Vec3A::ZERO);
        for layer in layers
            .iter()
            .filter(|l| l.mode == LayerMode::Normal && l.weight > 0.0)
        {
            let pose = layer.pose;
            let m = n.min(pose.len());
            for j in 0..m {
                let w = layer.weight
                    * layer
                        .mask
                        .and_then(|mask| mask.get(j))
                        .copied()
                        .unwrap_or(1.0);
                if w <= 0.0 {
                    continue;
                }
                weights[j] += w;
                out.translations[j] += pose.translations[j] * w;
                out.scales[j] += pose.scales[j] * w;
                // Into the accumulated sum's hemisphere (the first layer's).
                let q = Vec4::from(pose.rotations[j]);
                let q = if q.dot(rotations[j]) < 0.0 { -q } else { q };
                rotations[j] += q * w;
            }
        }
        let threshold = self.threshold.max(0.0);
        for j in 0..n {
            let missing = threshold - weights[j];
            if missing > 0.0 {
                weights[j] += missing;
                out.translations[j] += rest.translations[j] * missing;
                out.scales[j] += rest.scales[j] * missing;
                let q = Vec4::from(rest.rotations[j]);
                let q = if q.dot(rotations[j]) < 0.0 { -q } else { q };
                rotations[j] += q * missing;
            }
            let inv = 1.0 / weights[j];
            out.translations[j] *= inv;
            out.scales[j] *= inv;
            out.rotations[j] = Quat::from_vec4(rotations[j].normalize_or(Vec4::W));
        }
        for layer in layers
            .iter()
            .filter(|l| l.mode == LayerMode::Additive && l.weight > 0.0)
        {
            let pose = layer.pose;
            let m = n.min(pose.len());
            for j in 0..m {
                let w = layer.weight
                    * layer
                        .mask
                        .and_then(|mask| mask.get(j))
                        .copied()
                        .unwrap_or(1.0);
                if w <= 0.0 {
                    continue;
                }
                out.translations[j] += pose.translations[j] * w;
                out.scales[j] *= Vec3A::ONE.lerp(pose.scales[j], w);
                let delta = same_hemisphere(pose.rotations[j], Quat::IDENTITY);
                let partial = Vec4::W.lerp(Vec4::from(delta), w).normalize_or(Vec4::W);
                out.rotations[j] = (out.rotations[j] * Quat::from_vec4(partial)).normalize();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::{Transform, rotation_angle};
    use engine_core::math::Vec3;

    fn pose(t: Vec3, r: Quat, s: Vec3) -> Pose {
        Pose::from_transforms(&[Transform::new(t, r, s), Transform::new(t * 2.0, r, s)])
    }

    #[test]
    fn identities() {
        let rest = Pose::identity(2);
        let a = pose(Vec3::X, Quat::from_rotation_y(0.5), Vec3::splat(2.0));
        let b = pose(Vec3::Y, Quat::from_rotation_y(1.5), Vec3::ONE);
        let mut blender = Blender::new(2);
        let mut out = Pose::identity(2);
        // One layer at any weight above the threshold is itself.
        for w in [0.3, 1.0, 7.0] {
            let layer = Layer {
                pose: &a,
                weight: w,
                mask: None,
                mode: LayerMode::Normal,
            };
            blender.blend(&rest, &[layer], &mut out);
            for j in 0..2 {
                assert!((out.translations[j] - a.translations[j]).length() < 1e-6);
                assert!(rotation_angle(out.rotations[j], a.rotations[j]) < 1e-5);
                assert!((out.scales[j] - a.scales[j]).length() < 1e-6);
            }
        }
        // Equal weights: the midpoint (rotations about one axis: the mid angle).
        let layers = [
            Layer {
                pose: &a,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Normal,
            },
            Layer {
                pose: &b,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Normal,
            },
        ];
        blender.blend(&rest, &layers, &mut out);
        assert!((out.translations[0] - Vec3A::new(0.5, 0.5, 0.0)).length() < 1e-6);
        assert!(rotation_angle(out.rotations[0], Quat::from_rotation_y(1.0)) < 1e-5);
        // Weights 0 and 1: the second layer.
        let layers = [
            Layer {
                pose: &a,
                weight: 0.0,
                mask: None,
                mode: LayerMode::Normal,
            },
            Layer {
                pose: &b,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Normal,
            },
        ];
        blender.blend(&rest, &layers, &mut out);
        assert!(rotation_angle(out.rotations[1], b.rotations[1]) < 1e-5);
        // A mask of 0 on joint 1: joint 1 comes from the other layer only.
        let mask = [1.0, 0.0];
        let layers = [
            Layer {
                pose: &a,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Normal,
            },
            Layer {
                pose: &b,
                weight: 1.0,
                mask: Some(&mask),
                mode: LayerMode::Normal,
            },
        ];
        blender.blend(&rest, &layers, &mut out);
        assert!((out.translations[1] - a.translations[1]).length() < 1e-6);
        assert!((out.translations[0] - Vec3A::new(0.5, 0.5, 0.0)).length() < 1e-6);
        // No weight at all: the rest pose.
        blender.blend(&rest, &[], &mut out);
        for j in 0..2 {
            assert!(out.translations[j].length() < 1e-6);
            assert!(rotation_angle(out.rotations[j], Quat::IDENTITY) < 1e-5);
            assert!((out.scales[j] - Vec3A::ONE).length() < 1e-6);
        }
        // Opposite-sign quaternions of one rotation blend to that rotation.
        let c = pose(Vec3::ZERO, -Quat::from_rotation_y(0.5), Vec3::ONE);
        let d = pose(Vec3::ZERO, Quat::from_rotation_y(0.5), Vec3::ONE);
        let layers = [
            Layer {
                pose: &c,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Normal,
            },
            Layer {
                pose: &d,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Normal,
            },
        ];
        blender.blend(&rest, &layers, &mut out);
        assert!(rotation_angle(out.rotations[0], Quat::from_rotation_y(0.5)) < 1e-5);
    }

    #[test]
    fn additive_layers_restore_their_source() {
        let rest = Pose::identity(2);
        let base = pose(Vec3::X, Quat::from_rotation_x(0.3), Vec3::splat(1.5));
        let target = pose(Vec3::Z, Quat::from_rotation_z(1.2), Vec3::splat(0.5));
        let mut delta = Pose::identity(2);
        target.delta_from(&base, &mut delta);
        let mut blender = Blender::new(2);
        let mut out = Pose::identity(2);
        let layers = [
            Layer {
                pose: &base,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Normal,
            },
            Layer {
                pose: &delta,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Additive,
            },
        ];
        blender.blend(&rest, &layers, &mut out);
        for j in 0..2 {
            assert!((out.translations[j] - target.translations[j]).length() < 1e-5);
            assert!(rotation_angle(out.rotations[j], target.rotations[j]) < 1e-4);
            assert!((out.scales[j] - target.scales[j]).length() < 1e-5);
        }
        // Weight 0 of the additive layer: the base.
        let layers = [
            Layer {
                pose: &base,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Normal,
            },
            Layer {
                pose: &delta,
                weight: 0.0,
                mask: None,
                mode: LayerMode::Additive,
            },
        ];
        blender.blend(&rest, &layers, &mut out);
        assert!(rotation_angle(out.rotations[0], base.rotations[0]) < 1e-5);
    }
}
