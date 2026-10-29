//! Skinned models (ADR 0124's E12-1a): a [`Model`](crate::Model) whose glTF mesh a skinned
//! node draws is drawn skinned. The loader builds the mesh's parts with their joints and
//! weights and the skin's [`Rig`] (its skeleton, its rest pose's skinning matrices and the
//! file's clips that move it, compressed); the player gives each such model an instance of
//! the skin per part (`engine_render::skinning`) and poses it: in its rest pose, or at a
//! clip's moment where the editor asks ([`crate::Player::pose_model`]).
//!
//! The skinned vertices come out in the mesh node's own space with the file's import scale
//! applied, so the entity's transform places them as it places a plain model. The
//! skeleton's space is the glTF scene's (ADR 0068), so the palette goes back through the
//! node's place in the file and the scale: the model transforms start from
//! `root = S · node⁻¹`, and each inverse bind matrix is followed by `S⁻¹` (the vertices
//! were scaled by `S` when the mesh was made).

use crate::assets::{GltfFile, gltf_primitives};
use engine_anim::{
    CompressSettings, CompressedClip, Pose, Skeleton, compress, local_to_model, skinning_matrices,
};
use engine_assets::ImportedScene;
use engine_assets::animation::{node_parents, raw_clip, shell_distances, skin_skeleton};
use engine_core::math::{Affine3A, Quat, Vec3};
use engine_render::mesh::OwnedMeshData;

/// What poses a skinned mesh (see the module).
#[derive(Debug)]
pub struct Rig {
    skeleton: Skeleton,
    /// The skeleton joint of each skin joint (what the vertices' joint indices index).
    joints: Vec<u16>,
    /// Per skin joint, its inverse bind matrix followed by the import scale's inverse.
    inverse_bind: Vec<Affine3A>,
    /// From the skeleton's space into the mesh node's, the import scale applied.
    root: Affine3A,
    /// The rest pose's skinning matrices.
    rest: Vec<[f32; 12]>,
    clips: Vec<RigClip>,
}

/// One of a skinned mesh's clips: an animation of its file that moves its skeleton.
#[derive(Debug)]
pub struct RigClip {
    name: String,
    clip: CompressedClip,
    source_bytes: usize,
    max_error: f32,
}

impl RigClip {
    /// The animation's name in its file.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its length in seconds.
    pub fn duration(&self) -> f32 {
        self.clip.duration()
    }

    /// Its samples a second.
    pub fn sample_rate(&self) -> f32 {
        self.clip.sample_rate()
    }

    /// Its samples.
    pub fn sample_count(&self) -> u32 {
        self.clip.sample_count()
    }

    /// Bytes it takes compressed, and the source's.
    pub fn bytes(&self) -> (usize, usize) {
        (self.clip.size_bytes(), self.source_bytes)
    }

    /// The largest error compression made, in the file's units (ADR 0068).
    pub fn max_error(&self) -> f32 {
        self.max_error
    }
}

impl Rig {
    /// The skin's joints the vertices are weighted to.
    pub fn joint_count(&self) -> usize {
        self.joints.len()
    }

    /// The skeleton.
    pub fn skeleton(&self) -> &Skeleton {
        &self.skeleton
    }

    /// The file's clips that move its skeleton, in the file's order.
    pub fn clips(&self) -> &[RigClip] {
        &self.clips
    }
}

/// A part of a skinned mesh as the player draws it: its vertices (the import scale applied)
/// and their joints and weights, one set of four or two per vertex.
#[derive(Debug)]
pub(crate) struct SkinPart {
    pub(crate) mesh: OwnedMeshData,
    pub(crate) joints: Vec<[u16; 4]>,
    pub(crate) weights: Vec<[f32; 4]>,
}

/// The node that draws mesh `mesh` with a skin, the first in the file's order: the node and
/// its skin.
pub(crate) fn skinned_node(scene: &ImportedScene, mesh: usize) -> Option<(usize, usize)> {
    scene
        .nodes
        .iter()
        .enumerate()
        .find_map(|(i, n)| match (n.mesh, n.skin) {
            (Some(m), Some(skin)) if m == mesh => Some((i, skin)),
            _ => None,
        })
}

/// Node `node`'s place in its file: its transform and its ancestors'.
fn node_world(scene: &ImportedScene, parents: &[Option<usize>], node: usize) -> Affine3A {
    let mut world = Affine3A::IDENTITY;
    let mut at = Some(node);
    // A file whose parents loop is cut after as many steps as it has nodes.
    for _ in 0..scene.nodes.len() {
        let Some(n) = at else {
            break;
        };
        let d = &scene.nodes[n];
        let local = Affine3A::from_scale_rotation_translation(
            Vec3::from(d.scale),
            Quat::from_array(d.rotation),
            Vec3::from(d.translation),
        );
        world = local * world;
        at = parents[n];
    }
    world
}

/// `primitives` as one part with their influences: as many sets a vertex as the most any
/// has (the others padded with zero weights); a vertex with none follows the skin's first
/// joint.
fn part(primitives: &[engine_assets::Primitive], scale: f32) -> SkinPart {
    let mesh = gltf_primitives(primitives, scale);
    let sets = primitives
        .iter()
        .map(engine_assets::Primitive::influence_sets)
        .max()
        .unwrap_or(1)
        .clamp(1, 2);
    let mut joints = Vec::with_capacity(mesh.positions.len() * sets);
    let mut weights = Vec::with_capacity(mesh.positions.len() * sets);
    for p in primitives {
        let own = p.influence_sets();
        for v in 0..p.positions.len() {
            for set in 0..sets {
                let at = v * own + set;
                match (set < own).then(|| (p.joints.get(at), p.weights.get(at))) {
                    Some((Some(&j), Some(&w))) => {
                        joints.push(j);
                        weights.push(w);
                    }
                    _ => {
                        joints.push([0; 4]);
                        weights.push(if set == 0 && own == 0 {
                            [1.0, 0.0, 0.0, 0.0]
                        } else {
                            [0.0; 4]
                        });
                    }
                }
            }
        }
    }
    SkinPart {
        mesh,
        joints,
        weights,
    }
}

/// Mesh `mesh` of `file` skinned: its parts (the whole mesh, or a part per primitive with
/// `split`) and its rig, the file's clips compressed within ADR 0068's default error; or
/// why it cannot be. `None`: no node draws it with a skin.
pub(crate) fn build(
    file: &GltfFile,
    mesh: usize,
    split: bool,
) -> Result<Option<(Vec<SkinPart>, Rig)>, String> {
    engine_profiler::zone!("assets::skins::build");
    let scene = &file.scene;
    let Some((node, skin)) = skinned_node(scene, mesh) else {
        return Ok(None);
    };
    let primitives = &scene
        .meshes
        .get(mesh)
        .ok_or_else(|| String::from("the file has no such mesh"))?
        .primitives;
    let skeleton = skin_skeleton(scene, skin).map_err(|e| e.to_string())?;
    let scale = file.meta.model.applied_scale();
    let parents = node_parents(scene);
    let root =
        Affine3A::from_scale(Vec3::splat(scale)) * node_world(scene, &parents, node).inverse();
    let unscale = Affine3A::from_scale(Vec3::splat(1.0 / scale));
    let inverse_bind: Vec<Affine3A> = skeleton.inverse_bind.iter().map(|m| *m * unscale).collect();
    let parts = if split {
        primitives
            .iter()
            .map(|p| part(core::slice::from_ref(p), scale))
            .collect()
    } else {
        vec![part(primitives, scale)]
    };
    // A vertex weighted to a joint the skin has not would read past its palette.
    let joints = skeleton.skin_joints.len();
    if let Some(bad) = parts
        .iter()
        .flat_map(|p| p.joints.iter().flatten())
        .find(|&&j| usize::from(j) >= joints)
    {
        return Err(format!(
            "a vertex is weighted to joint {bad}, past the skin's {joints}"
        ));
    }
    let mut model = vec![Affine3A::IDENTITY; skeleton.skeleton.joint_count()];
    local_to_model(
        &skeleton.skeleton,
        skeleton.skeleton.rest(),
        &root,
        &mut model,
    );
    let mut rest = vec![[0.0; 12]; joints];
    skinning_matrices(&model, &skeleton.skin_joints, &inverse_bind, &mut rest);
    // The file's clips that move this skeleton.
    let shells = shell_distances(scene, skin, &skeleton);
    let settings = CompressSettings {
        joint_shell_distances: shells,
        ..CompressSettings::default()
    };
    let clips = (0..scene.animations.len())
        .filter_map(|a| raw_clip(scene, a, &skeleton))
        .map(|raw| {
            let (clip, stats) = compress(&raw, &skeleton.skeleton, &settings);
            RigClip {
                name: raw.name.clone(),
                clip,
                source_bytes: stats.source_bytes,
                max_error: stats.max_error,
            }
        })
        .collect();
    Ok(Some((
        parts,
        Rig {
            skeleton: skeleton.skeleton,
            joints: skeleton.skin_joints,
            inverse_bind,
            root,
            rest,
            clips,
        },
    )))
}

/// Where poses are made, kept between frames: sized for the largest rig posed, so posing
/// allocates nothing after the first.
#[derive(Debug, Default)]
pub(crate) struct Posing {
    pose: Option<Pose>,
    model: Vec<Affine3A>,
    palette: Vec<[f32; 12]>,
}

impl Posing {
    /// `rig`'s skinning matrices at `pose` (a clip and a time in it, held at its ends), its
    /// rest pose's without one or for a clip it has not.
    pub(crate) fn palette<'a>(
        &'a mut self,
        rig: &'a Rig,
        pose: Option<(usize, f32)>,
    ) -> &'a [[f32; 12]] {
        let Some((clip, time)) = pose.filter(|&(c, _)| c < rig.clips.len()) else {
            return &rig.rest;
        };
        let n = rig.skeleton.joint_count();
        let sampled = self.pose.get_or_insert_with(|| Pose::identity(n));
        rig.clips[clip]
            .clip
            .sample(time, rig.skeleton.rest(), sampled);
        if self.model.len() < n {
            self.model.resize(n, Affine3A::IDENTITY);
        }
        local_to_model(&rig.skeleton, sampled, &rig.root, &mut self.model[..n]);
        let joints = rig.joints.len();
        if self.palette.len() < joints {
            self.palette.resize(joints, [0.0; 12]);
        }
        skinning_matrices(
            &self.model[..n],
            &rig.joints,
            &rig.inverse_bind,
            &mut self.palette[..joints],
        );
        &self.palette[..joints]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_scene::AssetMeta;
    use std::path::Path;

    /// A test model of `tools/fetch-test-assets.ps1`, or `None` (skipped) without it.
    fn sample(path: &str, scale: f32) -> Option<GltfFile> {
        let full = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(path);
        let Ok(bytes) = std::fs::read(&full) else {
            println!(
                "skipped: {} missing (tools/fetch-test-assets.ps1)",
                full.display()
            );
            return None;
        };
        let dir = full.parent().unwrap().to_path_buf();
        let scene = engine_assets::import_gltf(&bytes, Some(&dir)).unwrap();
        let mut meta = AssetMeta::default();
        meta.model.scale = scale;
        Some(GltfFile {
            scene,
            dir,
            meta,
            path: full,
        })
    }

    /// A vertex skinned by a palette: its influences' matrices weighted.
    fn skinned(part: &SkinPart, palette: &[[f32; 12]], v: usize) -> Vec3 {
        let p = Vec3::from(part.mesh.positions[v]);
        let sets = part.joints.len() / part.mesh.positions.len();
        let mut out = Vec3::ZERO;
        let mut total = 0.0;
        for set in 0..sets {
            let (j, w) = (part.joints[v * sets + set], part.weights[v * sets + set]);
            for k in 0..4 {
                let m = palette[usize::from(j[k])];
                let row = |r: usize| {
                    Vec3::new(m[r * 4], m[r * 4 + 1], m[r * 4 + 2]).dot(p) + m[r * 4 + 3]
                };
                out += w[k] * Vec3::new(row(0), row(1), row(2));
                total += w[k];
            }
        }
        out / total.max(1e-12)
    }

    /// E12-1a: a skinned sample's mesh is found skinned, its rest pose skins its vertices
    /// to finite places near the mesh, its clips come with it; and its import scale scales
    /// the skinned vertices as it scales the mesh (the palette conjugated by the scale).
    #[test]
    fn a_skinned_mesh_has_a_rig_whose_rest_pose_and_scale_hold() {
        for path in [
            "CesiumMan/CesiumMan.gltf",
            "RiggedSimple/RiggedSimple.gltf",
            "Fox/Fox.gltf",
        ] {
            let (Some(one), Some(two)) = (sample(path, 1.0), sample(path, 2.0)) else {
                continue;
            };
            let mesh = (0..one.scene.meshes.len())
                .find(|&m| skinned_node(&one.scene, m).is_some())
                .unwrap();
            let (parts, rig) = build(&one, mesh, false).unwrap().unwrap();
            let (parts2, rig2) = build(&two, mesh, false).unwrap().unwrap();
            println!(
                "{path}: {} joints, {} vertices, clips {:?}",
                rig.joint_count(),
                parts[0].mesh.positions.len(),
                rig.clips()
                    .iter()
                    .map(|c| (c.name(), c.duration(), c.bytes().0))
                    .collect::<Vec<_>>()
            );
            assert!(!rig.clips().is_empty(), "{path}: its clips");
            assert!(rig.clips().iter().all(|c| c.duration() > 0.0));
            let part = &parts[0];
            let (lo, hi) = part.mesh.positions.iter().fold(
                (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
                |(lo, hi), &p| (lo.min(Vec3::from(p)), hi.max(Vec3::from(p))),
            );
            let reach = (hi - lo).length().max(1e-3) * 2.0;
            let step = (part.mesh.positions.len() / 50).max(1);
            for v in (0..part.mesh.positions.len()).step_by(step) {
                let rest = skinned(part, &rig.rest, v);
                assert!(rest.is_finite(), "{path}: vertex {v}");
                assert!(
                    (rest - (lo + hi) * 0.5).length() < reach,
                    "{path}: vertex {v} at {rest}, the mesh within {lo}..{hi}"
                );
                let doubled = skinned(&parts2[0], &rig2.rest, v);
                assert!(
                    (doubled - rest * 2.0).length() < 1e-3 * reach,
                    "{path}: vertex {v}: {doubled} against twice {rest}"
                );
            }
            // A clip's moment poses it elsewhere than its rest.
            let mut posing = Posing::default();
            let clip = &rig.clips()[0];
            let palette = posing
                .palette(&rig, Some((0, clip.duration() * 0.5)))
                .to_vec();
            let moved = (0..part.mesh.positions.len()).step_by(step).any(|v| {
                (skinned(part, &palette, v) - skinned(part, &rig.rest, v)).length() > 1e-4 * reach
            });
            assert!(moved, "{path}: {} moves it", clip.name());
            // No clip, or one it has not: the rest pose.
            assert_eq!(posing.palette(&rig, None), &rig.rest[..]);
            assert_eq!(posing.palette(&rig, Some((99, 0.0))), &rig.rest[..]);
        }
    }

    /// A plain mesh is not skinned.
    #[test]
    fn a_plain_mesh_has_no_skin() {
        let Some(file) = sample("MetalRoughSpheres/MetalRoughSpheres.gltf", 1.0) else {
            return;
        };
        for m in 0..file.scene.meshes.len() {
            assert!(skinned_node(&file.scene, m).is_none());
            assert!(build(&file, m, false).unwrap().is_none());
        }
    }
}
