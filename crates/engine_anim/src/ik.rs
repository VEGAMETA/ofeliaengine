//! Inverse kinematics on a sampled pose: two-bone (limbs), FABRIK (chains), look-at.
//!
//! Every solver works on the model-space transforms of a pose (from
//! [`crate::local_to_model`]) and writes its result back twice: into the local rotations
//! of the joints it moves (so the pose stays the source of truth) and into the model
//! transforms of their subtrees (so the next solver and the skinning see it). Targets and
//! poles are in model space. Joints are rotated about their own origin; bone lengths
//! never change. Parents with non-uniform scale are not supported (their rotation is
//! taken from the polar part of the matrix).

use crate::model::update_model_range;
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use engine_core::math::{Affine3A, Quat, Vec3, Vec3A};

/// Below this, lengths and distances count as zero.
const EPSILON: f32 = 1e-6;

/// What a solver achieved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IkOutcome {
    /// The target was within reach (the end now lies on it, within the tolerance).
    pub reached: bool,
    /// Distance left between the end joint and the target.
    pub distance: f32,
}

/// The rotation of an affine transform (its polar part).
fn rotation_of(m: &Affine3A) -> Quat {
    m.to_scale_rotation_translation().1
}

/// Rotates joint `j` by `delta` (in model space, about its origin) and updates its
/// subtree's model transforms.
pub fn rotate_joint(
    skeleton: &Skeleton,
    local: &mut Pose,
    model: &mut [Affine3A],
    root: &Affine3A,
    j: usize,
    delta: Quat,
) {
    if j >= local.len() || j >= model.len() {
        return;
    }
    let parent = skeleton.parent(j).map_or(*root, |p| model[p]);
    let p = rotation_of(&parent);
    local.rotations[j] = (p.conjugate() * delta * p * local.rotations[j]).normalize();
    update_model_range(skeleton, local, root, model, j, skeleton.subtree_end(j));
}

/// The shortest rotation from direction `from` to `to` (identity when either is zero).
fn arc(from: Vec3, to: Vec3) -> Quat {
    match (from.try_normalize(), to.try_normalize()) {
        (Some(a), Some(b)) => Quat::from_rotation_arc(a, b),
        _ => Quat::IDENTITY,
    }
}

/// `q` scaled by `weight` (from identity).
fn weighted(q: Quat, weight: f32) -> Quat {
    if weight >= 1.0 {
        q
    } else {
        Quat::IDENTITY.slerp(q, weight.max(0.0))
    }
}

fn position(model: &[Affine3A], j: usize) -> Vec3 {
    model
        .get(j)
        .map_or(Vec3::ZERO, |m| Vec3::from(m.translation))
}

/// A limb: `start` (hip, shoulder), `mid` (knee, elbow) below it, `end` (ankle, wrist)
/// below that.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwoBoneIk {
    /// Upper joint.
    pub start: usize,
    /// Middle joint (a descendant of `start`).
    pub mid: usize,
    /// End joint (a descendant of `mid`).
    pub end: usize,
    /// Where the end joint should be (model space).
    pub target: Vec3,
    /// A point the middle joint bends towards (a knee's front, an elbow's back), model
    /// space.
    pub pole: Vec3,
    /// 0 leaves the pose, 1 solves fully.
    pub weight: f32,
}

impl TwoBoneIk {
    /// Solves the limb (analytically: the law of cosines places the middle joint in the
    /// plane of the start, the target and the pole). A target out of reach (too far, or
    /// closer than the bones' difference) gets the end as close as the bones allow, on
    /// the line to it.
    pub fn solve(
        &self,
        skeleton: &Skeleton,
        local: &mut Pose,
        model: &mut [Affine3A],
        root: &Affine3A,
    ) -> IkOutcome {
        engine_profiler::zone!("anim::two_bone_ik");
        let (a, b, c) = (
            position(model, self.start),
            position(model, self.mid),
            position(model, self.end),
        );
        let (l1, l2) = ((b - a).length(), (c - b).length());
        let to_target = self.target - a;
        let distance = to_target.length();
        if l1 < EPSILON || l2 < EPSILON || distance < EPSILON {
            return IkOutcome {
                reached: false,
                distance: (c - self.target).length(),
            };
        }
        let dir = to_target / distance;
        let (lo, hi) = ((l1 - l2).abs(), l1 + l2);
        let d = distance.clamp(lo, hi);
        // The bend plane: through the start, the target and the pole (or the current
        // middle joint, or any plane).
        let perpendicular = |v: Vec3| (v - dir * v.dot(dir)).try_normalize();
        let side = perpendicular(self.pole - a)
            .or_else(|| perpendicular(b - a))
            .unwrap_or_else(|| dir.any_orthonormal_vector());
        let cos = ((l1 * l1 + d * d - l2 * l2) / (2.0 * l1 * d)).clamp(-1.0, 1.0);
        let sin = (1.0 - cos * cos).max(0.0).sqrt();
        let knee = a + l1 * (cos * dir + sin * side);
        let foot = a + d * dir;
        let upper = weighted(arc(b - a, knee - a), self.weight);
        rotate_joint(skeleton, local, model, root, self.start, upper);
        let (b, c) = (position(model, self.mid), position(model, self.end));
        let lower = weighted(arc(c - b, foot - b), self.weight);
        rotate_joint(skeleton, local, model, root, self.mid, lower);
        let left = (position(model, self.end) - self.target).length();
        IkOutcome {
            reached: distance <= hi + EPSILON && distance >= lo - EPSILON,
            distance: left,
        }
    }
}

/// FABRIK (Aristidou and Lasenby, 2011) for chains of any length, with scratch space
/// reused between calls (no allocation once it has seen the longest chain).
#[derive(Debug, Clone, Default)]
pub struct Fabrik {
    positions: Vec<Vec3>,
    lengths: Vec<f32>,
}

impl Fabrik {
    /// An empty solver.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves `chain` (joints from its base to its tip, each an ancestor of the next) so
    /// its tip reaches `target` within `tolerance`, in at most `iterations` forward and
    /// backward passes. Out of reach, the chain points straight at the target.
    #[allow(clippy::too_many_arguments)]
    pub fn solve(
        &mut self,
        skeleton: &Skeleton,
        local: &mut Pose,
        model: &mut [Affine3A],
        root: &Affine3A,
        chain: &[usize],
        target: Vec3,
        tolerance: f32,
        iterations: u32,
    ) -> IkOutcome {
        engine_profiler::zone!("anim::fabrik");
        let n = chain.len();
        if n < 2 {
            let tip = chain.first().map_or(Vec3::ZERO, |&j| position(model, j));
            return IkOutcome {
                reached: false,
                distance: (tip - target).length(),
            };
        }
        self.positions.clear();
        self.positions
            .extend(chain.iter().map(|&j| position(model, j)));
        self.lengths.clear();
        self.lengths
            .extend(self.positions.windows(2).map(|w| (w[1] - w[0]).length()));
        let p = &mut self.positions;
        let lengths = &self.lengths;
        let total: f32 = lengths.iter().sum();
        let base = p[0];
        let reachable = (target - base).length() <= total;
        if !reachable {
            let dir = (target - base).normalize_or_zero();
            for i in 0..n - 1 {
                p[i + 1] = p[i] + dir * lengths[i];
            }
        } else {
            let mut k = 0;
            while (p[n - 1] - target).length() > tolerance && k < iterations {
                // Backward: the tip on the target, each joint pulled towards its child.
                p[n - 1] = target;
                for i in (0..n - 1).rev() {
                    let dir = (p[i] - p[i + 1]).normalize_or_zero();
                    p[i] = p[i + 1] + dir * lengths[i];
                }
                // Forward: the base back in place, each joint pulled towards its parent.
                p[0] = base;
                for i in 0..n - 1 {
                    let dir = (p[i + 1] - p[i]).normalize_or_zero();
                    p[i + 1] = p[i] + dir * lengths[i];
                }
                k += 1;
            }
        }
        // Rotations: each bone turned from where it points to where it should.
        for i in 0..n - 1 {
            let (from, to) = (chain[i], chain[i + 1]);
            let now = position(model, to) - position(model, from);
            let wanted = self.positions[i + 1] - self.positions[i];
            rotate_joint(skeleton, local, model, root, from, arc(now, wanted));
        }
        let distance = (position(model, chain[n - 1]) - target).length();
        IkOutcome {
            reached: reachable && distance <= tolerance * 1.01 + EPSILON,
            distance,
        }
    }
}

/// Turns a joint (a head, an eye, a gun barrel) so an axis of it points at a target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LookAt {
    /// The joint.
    pub joint: usize,
    /// The axis to aim, in the joint's local space (for example +Z).
    pub forward: Vec3,
    /// What to look at (model space).
    pub target: Vec3,
    /// 0 leaves the pose, 1 aims fully (within `max_angle`).
    pub weight: f32,
    /// Largest turn, radians.
    pub max_angle: f32,
}

impl LookAt {
    /// Aims the joint: the shortest turn of its axis towards the target, limited to
    /// `max_angle` and scaled by `weight`. Returns the angle left between the axis and the
    /// target.
    pub fn solve(
        &self,
        skeleton: &Skeleton,
        local: &mut Pose,
        model: &mut [Affine3A],
        root: &Affine3A,
    ) -> f32 {
        engine_profiler::zone!("anim::look_at");
        let Some(m) = model.get(self.joint).copied() else {
            return 0.0;
        };
        let forward = m.transform_vector3a(Vec3A::from(self.forward));
        let wanted = self.target - Vec3::from(m.translation);
        let turn = arc(forward.into(), wanted);
        let (axis, angle) = turn.to_axis_angle();
        let angle = angle.min(self.max_angle.max(0.0)) * self.weight.clamp(0.0, 1.0);
        if angle > 0.0 {
            rotate_joint(
                skeleton,
                local,
                model,
                root,
                self.joint,
                Quat::from_axis_angle(axis, angle),
            );
        }
        let now = model[self.joint].transform_vector3a(Vec3A::from(self.forward));
        Vec3::from(now).angle_between(wanted)
    }
}
