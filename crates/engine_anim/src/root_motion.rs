//! Root motion: the movement of a clip's root joint taken out of the pose and handed to
//! the game (to move the character's entity or capsule), so a walk cycle in place moves
//! the character by exactly what the animator authored.

use crate::clip::CompressedClip;
use crate::pose::{Pose, Transform};
use crate::skeleton::Skeleton;
use engine_core::math::{Quat, Vec3, Vec3A};

/// Which parts of the root's motion are extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootMotionMode {
    /// Horizontal translation (x and z: +Y is up).
    pub horizontal: bool,
    /// Vertical translation (y).
    pub vertical: bool,
    /// Rotation about +Y (yaw).
    pub yaw: bool,
}

impl Default for RootMotionMode {
    /// Ground motion: horizontal translation and yaw (jumps stay in the pose).
    fn default() -> Self {
        Self {
            horizontal: true,
            vertical: false,
            yaw: true,
        }
    }
}

/// A root's motion over an interval, in the space of the root joint's parent (the
/// character's space for a root without one).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RootMotion {
    /// Displacement.
    pub translation: Vec3,
    /// Rotation about +Y.
    pub rotation: Quat,
}

impl RootMotion {
    /// No motion.
    pub const NONE: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
    };

    /// Motion `a` then `b` (`b` expressed relative to where `a` ended).
    pub fn then(self, b: RootMotion) -> RootMotion {
        RootMotion {
            translation: self.translation + self.rotation * b.translation,
            rotation: (self.rotation * b.rotation).normalize(),
        }
    }
}

/// The rotation of `q` about +Y (the twist of a swing-twist decomposition).
pub fn yaw_of(q: Quat) -> Quat {
    let twist = Quat::from_xyzw(0.0, q.y, 0.0, q.w);
    if twist.length_squared() < 1e-12 {
        Quat::IDENTITY
    } else {
        twist.normalize()
    }
}

/// The extracted parts of a root transform: its reference for [`RootMotionMode`].
fn extracted(t: &Transform, mode: RootMotionMode) -> (Vec3, Quat) {
    let p = Vec3::from(t.translation);
    let translation = Vec3::new(
        if mode.horizontal { p.x } else { 0.0 },
        if mode.vertical { p.y } else { 0.0 },
        if mode.horizontal { p.z } else { 0.0 },
    );
    let rotation = if mode.yaw {
        yaw_of(t.rotation)
    } else {
        Quat::IDENTITY
    };
    (translation, rotation)
}

/// Motion from root transform `a` to `b`, in the frame of the character at `a`.
fn motion_between(a: &Transform, b: &Transform, mode: RootMotionMode) -> RootMotion {
    let (ta, ra) = extracted(a, mode);
    let (tb, rb) = extracted(b, mode);
    RootMotion {
        translation: ra.conjugate() * (tb - ta),
        rotation: (ra.conjugate() * rb).normalize(),
    }
}

/// The root motion of `clip` from `from` to `to` seconds for joint `root`. With `looping`,
/// `to < from` means the clip wrapped once: the motion to the end, then from the start.
/// The translation is in the character's frame at `from` (rotate it by the character's
/// orientation to move it in the world).
pub fn root_motion(
    clip: &CompressedClip,
    skeleton: &Skeleton,
    root: usize,
    from: f32,
    to: f32,
    looping: bool,
    mode: RootMotionMode,
) -> RootMotion {
    let rest = skeleton.rest().get(root);
    let at = |t: f32| clip.sample_joint(root, t, rest);
    if looping && to < from {
        let end = clip.duration();
        motion_between(&at(from), &at(end), mode).then(motion_between(&at(0.0), &at(to), mode))
    } else {
        motion_between(&at(from), &at(to), mode)
    }
}

/// Removes the extracted motion from `pose`'s root joint: its extracted parts are reset
/// to the clip's at time 0, so the pose stays where the character's transform puts it.
pub fn remove_root_motion(
    pose: &mut Pose,
    clip: &CompressedClip,
    skeleton: &Skeleton,
    root: usize,
    mode: RootMotionMode,
) {
    if root >= pose.len() {
        return;
    }
    let start = clip.sample_joint(root, 0.0, skeleton.rest().get(root));
    let mut t = pose.get(root);
    let (now_t, now_r) = extracted(&t, mode);
    let (start_t, start_r) = extracted(&start, mode);
    // Undo the yaw since the start, then the translation since the start, in the space
    // the start's yaw defines.
    let undo = (start_r * now_r.conjugate()).normalize();
    t.rotation = (undo * t.rotation).normalize();
    let p = Vec3::from(t.translation) - now_t;
    t.translation = Vec3A::from(undo * p + start_t);
    pose.set(root, t);
}
