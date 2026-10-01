//! A glTF scene's skins and animations as `engine_anim` data (ADR 0068).
//!
//! - A skin becomes a [`SkinSkeleton`]: an `engine_anim` skeleton of the skin's joints
//!   **and all their ancestors** up to the scene's roots, so the skeleton's model space is
//!   the scene's world space (glTF places a skinned mesh by its joints' world transforms
//!   and ignores the transform of the node holding the mesh); the rest pose is the nodes'
//!   transforms.
//! - An animation becomes one [`RawClip`] per skeleton whose joints it moves.
//! - [`shell_distances`]: per joint, the farthest vertex it influences: where its error
//!   shows, for the clip compressor's error metric.

use crate::gltf_import::{ChannelKeys, ImportError, ImportedScene};
use engine_anim::{JointDesc, RawClip, RawJointTracks, Skeleton, Transform};
use engine_core::math::{Affine3A, Mat4, Quat, Vec3};

/// A skin's skeleton (see the module documentation).
#[derive(Debug, Clone, PartialEq)]
pub struct SkinSkeleton {
    /// The skeleton.
    pub skeleton: Skeleton,
    /// The node of each skeleton joint (index into [`ImportedScene::nodes`]).
    pub nodes: Vec<usize>,
    /// The skeleton joint of each joint of the skin (what `JOINTS_n` index).
    pub skin_joints: Vec<u16>,
    /// The skin's inverse bind matrices, per skin joint.
    pub inverse_bind: Vec<Affine3A>,
}

/// Each node's parent (the first that lists it as a child).
fn parents(scene: &ImportedScene) -> Vec<Option<usize>> {
    let mut parent = vec![None; scene.nodes.len()];
    for (p, node) in scene.nodes.iter().enumerate() {
        for &c in &node.children {
            if let Some(slot) = parent.get_mut(c)
                && slot.is_none()
                && c != p
            {
                *slot = Some(p);
            }
        }
    }
    parent
}

/// The skeleton of skin `skin` (see the module documentation).
pub fn skin_skeleton(scene: &ImportedScene, skin: usize) -> Result<SkinSkeleton, ImportError> {
    let data = scene
        .skins
        .get(skin)
        .ok_or_else(|| ImportError::Invalid(format!("no skin {skin}")))?;
    let parent = parents(scene);
    // The joints and their ancestors, each once.
    let mut index_of = vec![None; scene.nodes.len()];
    let mut nodes = Vec::new();
    for &joint in &data.joints {
        let mut n = Some(joint);
        let mut steps = 0;
        while let Some(node) = n {
            if node >= scene.nodes.len() {
                return Err(ImportError::Invalid(format!(
                    "skin {skin}: joint node {node} does not exist"
                )));
            }
            if index_of[node].is_some() {
                break;
            }
            index_of[node] = Some(nodes.len());
            nodes.push(node);
            n = parent[node];
            steps += 1;
            if steps > scene.nodes.len() {
                return Err(ImportError::Invalid(format!("skin {skin}: a node cycle")));
            }
        }
    }
    let joints: Vec<JointDesc> = nodes
        .iter()
        .map(|&node| {
            let n = &scene.nodes[node];
            JointDesc {
                name: n.name.clone(),
                parent: parent[node].and_then(|p| index_of[p]),
                rest: Transform::new(
                    Vec3::from(n.translation),
                    Quat::from_array(n.rotation).normalize(),
                    Vec3::from(n.scale),
                ),
            }
        })
        .collect();
    let (skeleton, remap) =
        Skeleton::new(&joints).map_err(|e| ImportError::Invalid(format!("skin {skin}: {e}")))?;
    let mut sorted_nodes = vec![0; nodes.len()];
    for (old, &new) in remap.iter().enumerate() {
        sorted_nodes[new] = nodes[old];
    }
    let skin_joints = data
        .joints
        .iter()
        .map(|&node| index_of[node].map_or(0, |old| remap[old] as u16))
        .collect();
    let inverse_bind = data
        .inverse_bind_matrices
        .iter()
        .take(data.joints.len())
        .map(|m| Affine3A::from_mat4(Mat4::from_cols_array(m)))
        .collect();
    Ok(SkinSkeleton {
        skeleton,
        nodes: sorted_nodes,
        skin_joints,
        inverse_bind,
    })
}

/// Animation `animation`'s channels on the joints of `skeleton`, or `None` if it moves
/// none of them. Its duration is its last key on those joints.
pub fn raw_clip(
    scene: &ImportedScene,
    animation: usize,
    skeleton: &SkinSkeleton,
) -> Option<RawClip> {
    let data = scene.animations.get(animation)?;
    let mut joint_of = vec![None; scene.nodes.len()];
    for (j, &node) in skeleton.nodes.iter().enumerate() {
        joint_of[node] = Some(j);
    }
    let mut clip = RawClip {
        name: data.name.clone(),
        duration: 0.0,
        joints: vec![RawJointTracks::default(); skeleton.skeleton.joint_count()],
    };
    let mut any = false;
    for channel in &data.channels {
        let Some(j) = joint_of.get(channel.node).copied().flatten() else {
            continue;
        };
        let last = match &channel.keys {
            ChannelKeys::Translation(t) | ChannelKeys::Scale(t) => t.times.last(),
            ChannelKeys::Rotation(r) => r.times.last(),
        };
        clip.duration = clip.duration.max(last.copied().unwrap_or(0.0));
        let tracks = &mut clip.joints[j];
        match &channel.keys {
            ChannelKeys::Translation(t) => tracks.translation = Some(t.clone()),
            ChannelKeys::Rotation(r) => tracks.rotation = Some(r.clone()),
            ChannelKeys::Scale(s) => tracks.scale = Some(s.clone()),
        }
        any = true;
    }
    any.then_some(clip)
}

/// Per joint of `skeleton`, the distance from its bind position to the farthest vertex it
/// influences in the meshes of the nodes that use skin `skin` (0 for joints that
/// influence none).
pub fn shell_distances(scene: &ImportedScene, skin: usize, skeleton: &SkinSkeleton) -> Vec<f32> {
    let mut shell = vec![0.0f32; skeleton.skeleton.joint_count()];
    let bind_positions: Vec<Vec3> = skeleton
        .inverse_bind
        .iter()
        .map(|m| Vec3::from(m.inverse().translation))
        .collect();
    let meshes = scene
        .nodes
        .iter()
        .filter(|n| n.skin == Some(skin))
        .filter_map(|n| n.mesh);
    for mesh in meshes {
        for p in &scene.meshes[mesh].primitives {
            let sets = p.influence_sets();
            for (v, &position) in p.positions.iter().enumerate() {
                for k in 0..sets {
                    let (joints, weights) = (p.joints[v * sets + k], p.weights[v * sets + k]);
                    for i in 0..4 {
                        let skin_joint = joints[i] as usize;
                        if weights[i] <= 0.0 || skin_joint >= bind_positions.len() {
                            continue;
                        }
                        let d = (Vec3::from(position) - bind_positions[skin_joint]).length();
                        let j = skeleton.skin_joints[skin_joint] as usize;
                        shell[j] = shell[j].max(d);
                    }
                }
            }
        }
    }
    shell
}
