//! Root motion: the movement of a clip's root joint taken out of the pose and handed to
//! the game (to move the character's entity or capsule), so a walk cycle in place moves
//! the character by exactly what the animator authored.

use crate::clip::CompressedClip;
use crate::pose::{Pose, Transform};
use crate::skeleton::Skeleton;
use engine_core::math::{Quat, Vec3, Vec3A};

/// Which parts of the root's motion are extracted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RootMotionMode {
    /// Horizontal translation (across `up`).
    pub horizontal: bool,
    /// Vertical translation (along `up`).
    pub vertical: bool,
    /// Rotation about `up` (yaw).
    pub yaw: bool,
    /// The character's up in the space of the root joint's parent, of length 1: +Y unless
    /// the root hangs under a node turned otherwise (an armature exported standing Z up).
    pub up: Vec3,
}

impl Default for RootMotionMode {
    /// Ground motion: horizontal translation and yaw (jumps stay in the pose), +Y up.
    fn default() -> Self {
        Self {
            horizontal: true,
            vertical: false,
            yaw: true,
            up: Vec3::Y,
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

    /// The motion that undoes this one, in the frame where this one ends: a clip played
    /// backwards moves by its forward motion undone.
    pub fn inverse(self) -> RootMotion {
        let rotation = self.rotation.conjugate();
        RootMotion {
            translation: -(rotation * self.translation),
            rotation,
        }
    }

    /// Two motions blended, `b` by `w` (ozz's way, for clips fading into each other):
    /// the direction and the length of the translations, and the rotations, each between
    /// the two.
    pub fn blend(a: RootMotion, b: RootMotion, w: f32) -> RootMotion {
        let w = w.clamp(0.0, 1.0);
        let (la, lb) = (a.translation.length(), b.translation.length());
        let unit = |t: Vec3, l: f32| if l > 1e-12 { t / l } else { Vec3::ZERO };
        let direction =
            (unit(a.translation, la) * (1.0 - w) + unit(b.translation, lb) * w).normalize_or_zero();
        RootMotion {
            translation: direction * (la + (lb - la) * w),
            rotation: a.rotation.slerp(b.rotation, w).normalize(),
        }
    }

    /// Its turn about `up` (of length 1) in radians, counterclockwise seen from above:
    /// within ±π.
    pub fn angle_about(self, up: Vec3) -> f32 {
        let q = twist_of(self.rotation, up);
        let a = 2.0 * Vec3::new(q.x, q.y, q.z).dot(up).atan2(q.w);
        if a > core::f32::consts::PI {
            a - core::f32::consts::TAU
        } else if a < -core::f32::consts::PI {
            a + core::f32::consts::TAU
        } else {
            a
        }
    }
}

/// The rotation of `q` about +Y (the twist of a swing-twist decomposition).
pub fn yaw_of(q: Quat) -> Quat {
    twist_of(q, Vec3::Y)
}

/// The rotation of `q` about `axis` (of length 1): the twist of a swing-twist
/// decomposition.
pub fn twist_of(q: Quat, axis: Vec3) -> Quat {
    let along = axis * Vec3::new(q.x, q.y, q.z).dot(axis);
    let twist = Quat::from_xyzw(along.x, along.y, along.z, q.w);
    if twist.length_squared() < 1e-12 {
        Quat::IDENTITY
    } else {
        twist.normalize()
    }
}

/// The extracted parts of a root transform: its reference for [`RootMotionMode`].
fn extracted(t: &Transform, mode: RootMotionMode) -> (Vec3, Quat) {
    let p = Vec3::from(t.translation);
    let vertical = mode.up * p.dot(mode.up);
    let mut translation = Vec3::ZERO;
    if mode.horizontal {
        translation += p - vertical;
    }
    if mode.vertical {
        translation += vertical;
    }
    let rotation = if mode.yaw {
        twist_of(t.rotation, mode.up)
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

/// The root motion of `clip` over `from..to` seconds of play for joint `root`, the times
/// not wrapped: a span past the clip's end goes on from its start (as often as it wraps),
/// and `to < from` plays it backwards. In the character's frame at `from`, as
/// [`root_motion`]'s.
pub fn root_motion_over(
    clip: &CompressedClip,
    skeleton: &Skeleton,
    root: usize,
    from: f32,
    to: f32,
    mode: RootMotionMode,
) -> RootMotion {
    let length = clip.duration();
    if length <= 0.0 || from == to || !(from.is_finite() && to.is_finite()) {
        return RootMotion::NONE;
    }
    if to < from {
        return root_motion_over(clip, skeleton, root, to, from, mode).inverse();
    }
    // A span more than a few loops long (a tick of a very short clip played fast) moves by
    // whole loops, then the rest.
    let mut motion = RootMotion::NONE;
    let mut start = from;
    for _ in 0..64 {
        if start >= to {
            break;
        }
        let base = (start / length).floor() * length;
        let end = to.min(base + length);
        motion = motion.then(root_motion(
            clip,
            skeleton,
            root,
            start - base,
            end - base,
            false,
            mode,
        ));
        start = end;
    }
    motion
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
