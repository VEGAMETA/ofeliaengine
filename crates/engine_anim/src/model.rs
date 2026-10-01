//! From local transforms to model space, and skinning matrices.

use crate::pose::Pose;
use crate::skeleton::Skeleton;
use engine_core::math::Affine3A;

/// Model-space transforms of every joint: `model[j] = model[parent] · local[j]`, roots
/// placed by `root` (identity for the skeleton's own space). One forward pass: parents
/// come first in a [`Skeleton`].
pub fn local_to_model(skeleton: &Skeleton, local: &Pose, root: &Affine3A, model: &mut [Affine3A]) {
    engine_profiler::zone!("anim::local_to_model");
    update_model_range(skeleton, local, root, model, 0, skeleton.joint_count());
}

/// Recomputes the model transforms of joints `from..to` only (their parents must be up to
/// date): after changing the local transforms of a subtree, `from..to` is
/// `j..skeleton.subtree_end(j)`.
pub fn update_model_range(
    skeleton: &Skeleton,
    local: &Pose,
    root: &Affine3A,
    model: &mut [Affine3A],
    from: usize,
    to: usize,
) {
    let to = to
        .min(model.len())
        .min(local.len())
        .min(skeleton.joint_count());
    let parents = skeleton.parents();
    for j in from..to {
        let m = Affine3A::from_scale_rotation_translation(
            local.scales[j].into(),
            local.rotations[j],
            local.translations[j].into(),
        );
        let p = parents[j];
        model[j] = if (p as usize) < j {
            model[p as usize] * m
        } else {
            *root * m
        };
    }
}

/// Skinning matrices for the GPU: for each joint `k` of a skin, `model[joints[k]] ·
/// inverse_bind[k]`, as the three rows of a 3 × 4 matrix (48 bytes: what the skinning
/// shader reads).
pub fn skinning_matrices(
    model: &[Affine3A],
    joints: &[u16],
    inverse_bind: &[Affine3A],
    out: &mut [[f32; 12]],
) {
    engine_profiler::zone!("anim::skinning_matrices");
    for ((o, &j), ib) in out.iter_mut().zip(joints).zip(inverse_bind) {
        let m = model.get(j as usize).copied().unwrap_or(Affine3A::IDENTITY) * *ib;
        *o = rows(&m);
    }
}

/// The three rows of an affine transform (3 × 4, row-major).
#[inline]
pub fn rows(m: &Affine3A) -> [f32; 12] {
    let (x, y, z, t) = (
        m.matrix3.x_axis,
        m.matrix3.y_axis,
        m.matrix3.z_axis,
        m.translation,
    );
    [
        x.x, y.x, z.x, t.x, //
        x.y, y.y, z.y, t.y, //
        x.z, y.z, z.z, t.z,
    ]
}
