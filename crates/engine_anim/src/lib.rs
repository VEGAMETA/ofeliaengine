//! Skeletal animation (CLAUDE.md §13, ADR 0068): data-oriented and allocation-free per
//! frame, after ozz-animation's runtime and the Animation Compression Library's clip
//! format.
//!
//! - [`Skeleton`]: joints in depth-first order with parent indices and a rest pose.
//! - [`RawClip`]: keyframes as authored (glTF's step, linear and cubic spline channels),
//!   the reference.
//! - [`compress`] → [`CompressedClip`]: uniformly sampled, constant and default tracks
//!   removed, range-reduced and quantized with a bit width per track chosen against an
//!   object-space error bound.
//! - [`CompressedClip::sample`] → a [`Pose`] (local transforms, structure of arrays).
//! - [`Blender`]: weighted layers with per-joint masks, additive layers.
//! - [`root_motion`]: the root's motion taken out of the pose for the game.
//! - [`local_to_model`], [`skinning_matrices`]: model space and the GPU's skinning matrices.
//! - IK on model-space poses: [`TwoBoneIk`], [`Fabrik`], [`LookAt`].
//! - [`for_each_parallel`]: characters in parallel on `engine_core`'s job system.
//!
//! A frame of one character:
//!
//! ```
//! use engine_anim::*;
//! use engine_core::math::{Affine3A, Quat, Vec3};
//!
//! // A two-joint skeleton and a clip that turns the second joint.
//! let (skeleton, _) = Skeleton::new(&[
//!     JointDesc { name: "root".into(), parent: None, rest: Transform::IDENTITY },
//!     JointDesc {
//!         name: "arm".into(),
//!         parent: Some(0),
//!         rest: Transform::new(Vec3::Y, Quat::IDENTITY, Vec3::ONE),
//!     },
//! ]).unwrap();
//! let mut raw = RawClip { name: "wave".into(), duration: 1.0, joints: vec![Default::default(); 2] };
//! raw.joints[1].rotation = Some(RawTrack {
//!     interpolation: Interpolation::Linear,
//!     times: vec![0.0, 1.0],
//!     values: vec![Quat::IDENTITY, Quat::from_rotation_z(1.0)],
//! });
//! let (clip, stats) = compress(&raw, &skeleton, &CompressSettings::default());
//! assert!(stats.max_error <= 1e-4);
//!
//! // Per frame: no allocation.
//! let mut pose = Pose::identity(skeleton.joint_count());
//! let mut model = vec![Affine3A::IDENTITY; skeleton.joint_count()];
//! clip.sample(0.5, skeleton.rest(), &mut pose);
//! local_to_model(&skeleton, &pose, &Affine3A::IDENTITY, &mut model);
//! assert!(rotation_angle(pose.rotations[1], Quat::from_rotation_z(0.5)) < 1e-3);
//! ```

mod blend;
mod bytes;
mod clip;
mod compress;
mod ik;
mod jobs;
mod model;
mod pose;
mod raw;
mod root_motion;
mod skeleton;

pub use blend::{Blender, Layer, LayerMode};
pub use clip::{Channel, CompressedClip, SEGMENT_SAMPLES};
pub use compress::{AdditiveBase, CompressSettings, CompressionStats, compress};
pub use ik::{Fabrik, IkOutcome, LookAt, TwoBoneIk, rotate_joint};
pub use jobs::for_each_parallel;
pub use model::{local_to_model, rows, skinning_matrices, update_model_range};
pub use pose::{Pose, Transform, nlerp, rotation_angle};
pub use raw::{Interpolation, Keyed, RawClip, RawJointTracks, RawTrack};
pub use root_motion::{RootMotion, RootMotionMode, remove_root_motion, root_motion, yaw_of};
pub use skeleton::{JointDesc, MAX_JOINTS, NO_PARENT, Skeleton, SkeletonError};

/// A cooked skeleton or clip that cannot be read: not one, another version, damaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatError(pub &'static str);

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "invalid animation data: {}", self.0)
    }
}

impl std::error::Error for FormatError {}
