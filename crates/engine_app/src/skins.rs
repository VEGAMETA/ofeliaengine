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
    AnimGraph, Blender, CompressSettings, CompressedClip, GraphState, GraphWork, Layer, LayerMode,
    Pose, RootMotion, RootMotionMode, Skeleton, compress, local_to_model, remove_root_motion,
    root_motion_over, rotation_angle, skinning_matrices, twist_of, yaw_of,
};
use engine_assets::ImportedScene;
use engine_assets::animation::{node_parents, raw_clip, shell_distances, skin_skeleton};
use engine_core::math::{Affine3A, Quat, Vec3, Vec3A};
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
    /// Root motion's joint (ADR 0124's E12-2c): the topmost skin joint, whose ground motion
    /// it is; the frame from that joint's parent's space into the mesh's (the rest pose's);
    /// and up, the file's (glTF's +Y in its scene), seen from the parent and in the mesh.
    motion_root: usize,
    motion_frame: Affine3A,
    motion_up: Vec3,
    mesh_up: Vec3,
}

/// One of a skinned mesh's clips: an animation of its file that moves its skeleton, with
/// what the editor's Clips show of it (ADR 0124's E12-1b). Lengths are in the model's units:
/// the file's with its import scale applied.
#[derive(Debug)]
pub struct RigClip {
    name: String,
    clip: CompressedClip,
    source_bytes: usize,
    max_error: f32,
    root_speed: f32,
    seam: (f32, usize),
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

    /// Its samples, compressed (what an animation graph samples, ADR 0124's E12-5c).
    pub(crate) fn compressed(&self) -> &CompressedClip {
        &self.clip
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

    /// The largest error compression made where the skin is (ADR 0068's virtual vertices),
    /// in the model's units.
    pub fn max_error(&self) -> f32 {
        self.max_error
    }

    /// How fast its root joint crosses the ground from its first moment to its last, in the
    /// model's units a second: what its root motion would move a character at, near zero
    /// for a clip made in place.
    pub fn root_speed(&self) -> f32 {
        self.root_speed
    }

    /// Its loop's seam: how far its skin jumps from its last moment back to its first, the
    /// root's crossing of the ground and its turn about +Y taken out (as root motion takes
    /// them), in the model's units; and the skeleton joint that jumps most.
    pub fn seam(&self) -> (f32, usize) {
        self.seam
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

    /// Where its skeleton's roots hang (ADR 0124's E12-4e-2): the space above them (the mesh
    /// node's, the import scale applied) in which [`crate::Player::model_joints`] places the
    /// joints. A clip's keys of a root joint are in this space, the others' in their
    /// parent joint's.
    pub fn root_place(&self) -> Affine3A {
        self.root
    }

    /// How root motion is read from a clip (ADR 0124's E12-2c): the motion of its topmost
    /// skin joint along the clip's own axes (E12-3e: by default across and about the mesh's
    /// up).
    pub(crate) fn motion_mode_of(&self, axes: Axes) -> RootMotionMode {
        RootMotionMode {
            horizontal: axes.across,
            vertical: axes.up,
            yaw: axes.turn,
            up: self.motion_up,
        }
    }

    /// Clip `k`'s root motion over `from..to` seconds of play (wrapping at its end,
    /// backwards when `to < from`), in its root joint's parent's space as the pose shows it
    /// with the motion taken out along `axes` (E12-3e): the root kept facing as at the clip's
    /// first moment. None for a clip it has not.
    pub(crate) fn clip_motion(
        &self,
        k: usize,
        from: f32,
        to: f32,
        axes: Axes,
    ) -> Option<RootMotion> {
        let clip = &self.clips.get(k)?.clip;
        let (root, mode) = (self.motion_root, self.motion_mode_of(axes));
        let motion = root_motion_over(clip, &self.skeleton, root, from, to, mode);
        let first = clip.sample_joint(root, 0.0, self.skeleton.rest().get(root));
        let facing = twist_of(first.rotation, mode.up);
        Some(RootMotion {
            translation: facing * motion.translation,
            rotation: motion.rotation,
        })
    }

    /// A root motion from [`Self::clip_motion`] in the mesh's space: the root's crossing of
    /// the ground, and its turn about [`Self::mesh_up`] in radians.
    pub(crate) fn mesh_motion(&self, motion: RootMotion) -> (Vec3, f32) {
        (
            self.motion_frame.transform_vector3(motion.translation),
            motion.angle_about(self.motion_up),
        )
    }

    /// Up in the mesh's space (ADR 0124's E12-2c): the file's, glTF's +Y in its scene, which
    /// is the mesh's own +Y unless the mesh hangs under a node turned otherwise.
    pub(crate) fn mesh_up(&self) -> Vec3 {
        self.mesh_up
    }

    /// Clip `k`'s root motion over `from..to` seconds of play (wrapping at its end, backwards
    /// when `to < from`), in its file's scene (+Y up, the file's units), as the editor's
    /// preview stands the model: how far its root crossed the ground, and its turn about +Y
    /// in radians, counterclockwise seen from above (ADR 0124's E12-1b-3). None for a clip it
    /// has not.
    pub fn scene_motion(&self, k: usize, from: f32, to: f32) -> Option<(Vec3, f32)> {
        let motion = self.clip_motion(k, from, to, Axes::default())?;
        let frame = self.root.inverse() * self.motion_frame;
        Some((
            frame.transform_vector3(motion.translation),
            motion.angle_about(self.motion_up),
        ))
    }

    /// The file's clips that move its skeleton, in the file's order.
    pub fn clips(&self) -> &[RigClip] {
        &self.clips
    }

    /// Up in the model's space (ADR 0124's E12-3d): the file's, glTF's +Y in its scene.
    pub fn up(&self) -> Vec3 {
        self.mesh_up
    }

    /// Each joint's place in the model's space in the rest pose (ADR 0124's E12-3d), in the
    /// skeleton's order.
    pub fn rest_places(&self, out: &mut Vec<Vec3>) {
        out.clear();
        let mut posing = Posing::default();
        let joints = posing.joints(self, Look::default(), [None; 2]);
        out.extend(joints.iter().map(|m| Vec3::from(m.translation)));
    }

    /// Joint `joint`'s place in the model's space at each of clip `k`'s samples (ADR 0124's
    /// E12-3c), its root motion left in: the path the editor's trail draws and its speed
    /// curve is read from. Empty for a clip or a joint it has not.
    pub fn joint_path(&self, k: usize, joint: usize, out: &mut Vec<Vec3>) {
        out.clear();
        let Some(clip) = self.clips.get(k) else {
            return;
        };
        if joint >= self.skeleton.joint_count() {
            return;
        }
        let mut posing = Posing::default();
        let (rate, duration) = (clip.sample_rate().max(1e-6), clip.duration());
        for s in 0..clip.sample_count() {
            let t = (s as f32 / rate).min(duration);
            if let Some(m) = posing
                .joints(self, Look::at(Some((k, t))), [None; 2])
                .get(joint)
            {
                out.push(Vec3::from(m.translation));
            }
        }
    }

    /// From the skeleton's space into the mesh node's, the import scale applied: its
    /// inverse stands the skinned mesh as its file's scene does.
    pub(crate) fn root(&self) -> Affine3A {
        self.root
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
    // The file's clips that move this skeleton, within ADR 0068's error where the skin is
    // drawn: its lengths are the model's, so in the file's units they are divided by the
    // import scale.
    let shells = shell_distances(scene, skin, &skeleton);
    let defaults = CompressSettings::default();
    let settings = CompressSettings {
        error_threshold: defaults.error_threshold / scale,
        shell_distance: defaults.shell_distance / scale,
        joint_shell_distances: shells,
        ..defaults
    };
    let top = skeleton
        .skin_joints
        .iter()
        .min()
        .map_or(0, |&j| usize::from(j));
    // Root motion's frame (E12-2c): the top joint's parent where the rest pose holds it, in
    // the mesh's space. Up is the file's: the skeleton holds every ancestor of its joints, so
    // its space is the glTF scene's, +Y up; `root` takes that into the mesh's.
    let motion_frame = skeleton.skeleton.parent(top).map_or(root, |p| model[p]);
    let unit = |v: Vec3A| {
        Vec3::from(v)
            .try_normalize()
            .filter(|v| v.is_finite())
            .unwrap_or(Vec3::Y)
    };
    let mesh_up = unit(root.matrix3 * Vec3A::Y);
    let motion_up = unit(motion_frame.matrix3.inverse() * (root.matrix3 * Vec3A::Y));
    let clips = (0..scene.animations.len())
        .filter_map(|a| raw_clip(scene, a, &skeleton))
        .map(|raw| {
            let (clip, stats) = compress(&raw, &skeleton.skeleton, &settings);
            let (speed, seam, joint) = ends(
                &clip,
                &skeleton.skeleton,
                &settings.joint_shell_distances,
                top,
            );
            RigClip {
                name: raw.name.clone(),
                clip,
                source_bytes: stats.source_bytes,
                max_error: stats.max_error * scale,
                root_speed: speed * scale,
                seam: (seam * scale, joint),
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
            motion_root: top,
            motion_frame,
            motion_up,
            mesh_up,
        },
    )))
}

/// `clip`'s ends compared (see [`RigClip::root_speed`] and [`RigClip::seam`]), in the file's
/// units: its first and last moments posed in the glTF scene's space (+Y up), the last one
/// brought back so that its root joint `root` stands where and as the first one's does on
/// the ground, then each joint's virtual vertices (ADR 0068's shell distances) compared.
fn ends(
    clip: &CompressedClip,
    skeleton: &Skeleton,
    shells: &[f32],
    root: usize,
) -> (f32, f32, usize) {
    let n = skeleton.joint_count();
    if root >= n {
        return (0.0, 0.0, 0);
    }
    let mut pose = Pose::identity(n);
    let mut first = vec![Affine3A::IDENTITY; n];
    let mut last = vec![Affine3A::IDENTITY; n];
    let length = clip.duration();
    clip.sample(0.0, skeleton.rest(), &mut pose);
    local_to_model(skeleton, &pose, &Affine3A::IDENTITY, &mut first);
    clip.sample(length, skeleton.rest(), &mut pose);
    local_to_model(skeleton, &pose, &Affine3A::IDENTITY, &mut last);
    let ground = |m: &Affine3A| Vec3::new(m.translation.x, 0.0, m.translation.z);
    let facing = |m: &Affine3A| yaw_of(m.to_scale_rotation_translation().1);
    let (a, b) = (first[root], last[root]);
    let speed = if length > 0.0 {
        (ground(&b) - ground(&a)).length() / length
    } else {
        0.0
    };
    let back = Affine3A::from_translation(ground(&a))
        * Affine3A::from_quat((facing(&a) * facing(&b).conjugate()).normalize())
        * Affine3A::from_translation(-ground(&b));
    let mut seam = (0.0f32, root);
    for j in 0..n {
        let (_, r0, t0) = first[j].to_scale_rotation_translation();
        let (_, r1, t1) = (back * last[j]).to_scale_rotation_translation();
        let shell = shells.get(j).copied().unwrap_or(0.0);
        let jump = (t1 - t0).length() + 2.0 * shell * (0.5 * rotation_angle(r0, r1)).sin();
        if jump > seam.0 {
            seam = (jump, j);
        }
    }
    (speed, seam.0, seam.1)
}

/// A clip fading out under another (ADR 0124's E12-2b): it, its moment, and the other's
/// weight (the rest is this one's).
pub(crate) type Fade = Option<(usize, f32, f32)>;

/// What of a clip's root motion is taken out of its pose to move the character (ADR 0124's
/// E12-3e, its model's `.meta`'s): its crossing of the ground, its rise and fall, its turn
/// about up. By default the ground's two: a jump stays in the pose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Axes {
    pub(crate) across: bool,
    pub(crate) up: bool,
    pub(crate) turn: bool,
}

impl Default for Axes {
    fn default() -> Self {
        Self {
            across: true,
            up: false,
            turn: true,
        }
    }
}

/// What a skinned model shows (ADR 0124's E12-2): a clip at a moment (none: its rest pose),
/// the clip fading out under it, and whether its root motion is taken out of the pose (the
/// animator moving the character by it instead, E12-2c), along which axes for each of the
/// two clips (E12-3e).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Look {
    pub(crate) pose: Option<(usize, f32)>,
    pub(crate) fade: Fade,
    pub(crate) in_place: bool,
    pub(crate) axes: [Axes; 2],
    /// One of its animator's own clips (E12-4e), its joints keyed over the rest pose in place
    /// of a clip of the model's.
    pub(crate) own: Option<OwnPose>,
    /// The clip fading out when it is one of its animator's own clips (in place of `fade`),
    /// and the weight of what fades in.
    pub(crate) own_fade: Option<(OwnPose, f32)>,
    /// Its animator's animation graph (ADR 0124's E12-5c) in place of all of the above, a
    /// frame this far from its last tick to its next.
    pub(crate) graph: Option<f32>,
}

/// One of an animator's own clips shown on its character (ADR 0124's E12-4e): the clip's file
/// and the moment, an animator's running time (wrapped or held as the clip plays) or the
/// editor's playhead (held within the clip).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct OwnPose {
    pub(crate) file: engine_scene::AssetId,
    pub(crate) time: f32,
    pub(crate) playhead: bool,
}

impl Look {
    /// A clip's moment as made (its root motion in it), or the rest pose.
    pub(crate) fn at(pose: Option<(usize, f32)>) -> Self {
        Self {
            pose,
            ..Self::default()
        }
    }
}

/// Where poses are made, kept between frames: sized for the largest rig posed, so posing
/// allocates nothing after the first.
#[derive(Debug, Default)]
pub(crate) struct Posing {
    pose: Option<Pose>,
    from: Option<Pose>,
    blended: Option<Pose>,
    blender: Blender,
    blender_joints: usize,
    model: Vec<Affine3A>,
    palette: Vec<[f32; 12]>,
    /// Where animation graphs' poses are worked out (ADR 0124's E12-5c), and the joints it
    /// has room for (a graph of a smaller rig poses in it too).
    graph_work: Option<GraphWork>,
    graph_joints: usize,
}

impl Posing {
    /// `rig`'s local pose as `look` shows it, into `self.pose`: false for the rest pose (no
    /// clip, or one it has not). What fades in is a clip of the model's at a time in it (held
    /// at its ends, its root motion taken out or not), or one of its animator's own clips
    /// (E12-4e; `own[0]` that clip as read) with the joints it keys over the rest pose (one
    /// keying none: the rest pose). What fades out under it is either too (`own[1]`).
    fn local(
        &mut self,
        rig: &Rig,
        look: Look,
        own: [Option<&crate::field_clips::FieldClip>; 2],
    ) -> bool {
        let rest = rig.skeleton.rest();
        let n = rest.len();
        let root = rig.motion_root;
        let sampled = self.pose.get_or_insert_with(|| Pose::identity(n));
        let posed = if let Some(shown) = look.own {
            match own[0].filter(|c| c.keys_joints()) {
                Some(clip) => {
                    sampled.copy_from(rest);
                    clip.pose_joints(shown.time, shown.playhead, &rig.skeleton, sampled);
                    true
                }
                None => false,
            }
        } else {
            let Some((clip, time)) = look.pose.filter(|&(c, _)| c < rig.clips.len()) else {
                return false;
            };
            rig.clips[clip].clip.sample(time, rest, sampled);
            if look.in_place {
                let mode = rig.motion_mode_of(look.axes[0]);
                remove_root_motion(sampled, &rig.clips[clip].clip, &rig.skeleton, root, mode);
            }
            true
        };
        let other = self.from.get_or_insert_with(|| Pose::identity(n));
        let weight = if let Some((from, from_time, weight)) = look
            .fade
            .filter(|&(c, _, w)| c < rig.clips.len() && w < 1.0)
        {
            rig.clips[from].clip.sample(from_time, rest, other);
            if look.in_place {
                let mode = rig.motion_mode_of(look.axes[1]);
                remove_root_motion(other, &rig.clips[from].clip, &rig.skeleton, root, mode);
            }
            weight
        } else if let Some((shown, weight)) = look.own_fade.filter(|&(_, w)| w < 1.0) {
            match own[1].filter(|c| c.keys_joints()) {
                Some(clip) => {
                    other.copy_from(rest);
                    clip.pose_joints(shown.time, shown.playhead, &rig.skeleton, other);
                }
                // Out of the rest pose.
                None if posed => other.copy_from(rest),
                None => return false,
            }
            weight
        } else {
            return posed;
        };
        if !posed {
            sampled.copy_from(rest);
        }
        if self.blender_joints < n {
            self.blender = Blender::new(n);
            self.blender_joints = n;
        }
        let out = self.blended.get_or_insert_with(|| Pose::identity(n));
        out.copy_from(rest);
        let layers = [
            Layer {
                pose: &*other,
                weight: 1.0 - weight,
                mask: None,
                mode: LayerMode::Normal,
            },
            Layer {
                pose: &*sampled,
                weight,
                mask: None,
                mode: LayerMode::Normal,
            },
        ];
        self.blender.blend(rest, &layers, out);
        core::mem::swap(sampled, out);
        true
    }

    /// `rig`'s local pose as `graph` (cooked for it) shows character state `state` a frame
    /// `alpha` of the way from its last tick to its next (ADR 0124's E12-5c), into
    /// `self.pose`: false for the rest pose (nothing plays).
    fn local_graph(
        &mut self,
        rig: &Rig,
        graph: &AnimGraph,
        state: &GraphState,
        alpha: f32,
    ) -> bool {
        let rest = rig.skeleton.rest();
        let n = rest.len();
        if self.graph_joints < n {
            self.graph_work = None;
        }
        let work = self.graph_work.get_or_insert_with(|| GraphWork::new(n));
        self.graph_joints = self.graph_joints.max(n);
        let sampled = self.pose.get_or_insert_with(|| Pose::identity(n));
        let clips = &rig.clips;
        graph.pose(
            state,
            alpha,
            |k| clips.get(usize::from(k)).map(|c| &c.clip),
            rest,
            work,
            sampled,
        )
    }

    /// `rig`'s skinning matrices as `look` shows it ([`Self::local`]), its rest pose's
    /// without a clip or for one it has not.
    pub(crate) fn palette<'a>(
        &'a mut self,
        rig: &'a Rig,
        look: Look,
        own: [Option<&crate::field_clips::FieldClip>; 2],
    ) -> &'a [[f32; 12]] {
        let posed = self.local(rig, look, own);
        self.skinned(rig, posed)
    }

    /// `rig`'s skinning matrices as `graph` shows `state` ([`Self::local_graph`]), its rest
    /// pose's while nothing plays.
    pub(crate) fn palette_graph<'a>(
        &'a mut self,
        rig: &'a Rig,
        graph: &AnimGraph,
        state: &GraphState,
        alpha: f32,
    ) -> &'a [[f32; 12]] {
        let posed = self.local_graph(rig, graph, state, alpha);
        self.skinned(rig, posed)
    }

    /// `rig`'s skinning matrices for the local pose in `self.pose` (`posed`), else its rest
    /// pose's.
    fn skinned<'a>(&'a mut self, rig: &'a Rig, posed: bool) -> &'a [[f32; 12]] {
        if !posed {
            return &rig.rest;
        }
        let Some(sampled) = self.pose.as_ref() else {
            return &rig.rest;
        };
        let n = rig.skeleton.joint_count();
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

    /// `rig`'s joints where `look` puts them ([`Self::local`]; its rest pose without a clip,
    /// or for one it has not), in the skeleton's order: in the mesh node's space with the
    /// import scale applied, where the skinned vertices are (ADR 0124's E12-1c).
    pub(crate) fn joints<'a>(
        &'a mut self,
        rig: &Rig,
        look: Look,
        own: [Option<&crate::field_clips::FieldClip>; 2],
    ) -> &'a [Affine3A] {
        let posed = self.local(rig, look, own);
        self.placed(rig, posed)
    }

    /// `rig`'s joints where `graph` puts them for `state` ([`Self::local_graph`]), as
    /// [`Self::joints`] places them.
    pub(crate) fn joints_graph<'a>(
        &'a mut self,
        rig: &Rig,
        graph: &AnimGraph,
        state: &GraphState,
        alpha: f32,
    ) -> &'a [Affine3A] {
        let posed = self.local_graph(rig, graph, state, alpha);
        self.placed(rig, posed)
    }

    /// `rig`'s joints for the local pose in `self.pose` (`posed`), else its rest pose's.
    fn placed<'a>(&'a mut self, rig: &Rig, posed: bool) -> &'a [Affine3A] {
        let n = rig.skeleton.joint_count();
        if self.model.len() < n {
            self.model.resize(n, Affine3A::IDENTITY);
        }
        let local = match (posed, self.pose.as_ref()) {
            (true, Some(sampled)) => sampled,
            _ => rig.skeleton.rest(),
        };
        local_to_model(&rig.skeleton, local, &rig.root, &mut self.model[..n]);
        &self.model[..n]
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
    /// Each at its scale in meters (the Fox is made in centimetres), and at twice it.
    #[test]
    fn a_skinned_mesh_has_a_rig_whose_rest_pose_and_scale_hold() {
        for (path, scale) in [
            ("CesiumMan/CesiumMan.gltf", 1.0),
            ("RiggedSimple/RiggedSimple.gltf", 1.0),
            ("Fox/Fox.gltf", 0.01),
        ] {
            let (Some(one), Some(two)) = (sample(path, scale), sample(path, 2.0 * scale)) else {
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
                    .map(|c| (
                        c.name(),
                        c.duration(),
                        c.bytes().0,
                        c.max_error(),
                        c.root_speed(),
                        c.seam().0,
                        rig.skeleton().names()[c.seam().1].as_str()
                    ))
                    .collect::<Vec<_>>()
            );
            assert!(!rig.clips().is_empty(), "{path}: its clips");
            assert!(rig.clips().iter().all(|c| c.duration() > 0.0));
            // Within ADR 0068's error where the skin is, in the model's units; and the
            // lengths a clip shows double with the import scale.
            for (c, c2) in rig.clips().iter().zip(rig2.clips()) {
                assert!(
                    c.max_error() <= 1.01e-4,
                    "{path} {}: {}",
                    c.name(),
                    c.max_error()
                );
                assert!(
                    c2.max_error() <= 1.01e-4,
                    "{path} {}: {}",
                    c.name(),
                    c2.max_error()
                );
                assert!(c.root_speed().is_finite() && c.seam().0.is_finite());
                let near = |a: f32, b: f32| (a - b).abs() <= 2e-3 * b.abs().max(1.0);
                assert!(
                    near(c2.root_speed(), 2.0 * c.root_speed()),
                    "{path} {}: {} against twice {}",
                    c.name(),
                    c2.root_speed(),
                    c.root_speed()
                );
                assert!(
                    near(c2.seam().0, 2.0 * c.seam().0),
                    "{path} {}: seam {} against twice {}",
                    c.name(),
                    c2.seam().0,
                    c.seam().0
                );
            }
            let part = &parts[0];
            let (lo, hi) = part.mesh.positions.iter().fold(
                (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
                |(lo, hi), &p| (lo.min(Vec3::from(p)), hi.max(Vec3::from(p))),
            );
            let reach = (hi - lo).length().max(1e-3) * 2.0;
            // The rest pose is where the file's positions are: its box theirs, which a drop
            // rests on the floor (ADR 0124's E12-1c).
            let (rest_lo, rest_hi) = (0..part.mesh.positions.len()).fold(
                (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
                |(lo, hi), v| {
                    let p = skinned(part, &rig.rest, v);
                    (lo.min(p), hi.max(p))
                },
            );
            println!("{path}: rest pose {rest_lo}..{rest_hi}, positions {lo}..{hi}");
            assert!(
                (rest_lo - lo).abs().max_element() < 5e-4 * reach
                    && (rest_hi - hi).abs().max_element() < 5e-4 * reach,
                "{path}: the rest pose's box {rest_lo}..{rest_hi}, the positions' {lo}..{hi}"
            );
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
                .palette(&rig, Look::at(Some((0, clip.duration() * 0.5))), [None; 2])
                .to_vec();
            let moved = (0..part.mesh.positions.len()).step_by(step).any(|v| {
                (skinned(part, &palette, v) - skinned(part, &rig.rest, v)).length() > 1e-4 * reach
            });
            assert!(moved, "{path}: {} moves it", clip.name());
            // The joints where a pose puts them skin as its palette does (ADR 0124's
            // E12-1c): each skin joint's place times its inverse bind matrix.
            let mut again = vec![[0.0f32; 12]; rig.joints.len()];
            for pose in [None, Some((0, clip.duration() * 0.5))] {
                let joints = posing.joints(&rig, Look::at(pose), [None; 2]).to_vec();
                assert_eq!(joints.len(), rig.skeleton.joint_count());
                skinning_matrices(&joints, &rig.joints, &rig.inverse_bind, &mut again);
                let palette = posing.palette(&rig, Look::at(pose), [None; 2]);
                let same = palette.iter().zip(&again).all(|(a, b)| {
                    a.iter()
                        .zip(b)
                        .all(|(x, y)| (x - y).abs() <= 1e-4 * (1.0 + x.abs()))
                });
                assert!(same, "{path} {pose:?}: the joints skin as the palette does");
            }
            // A fade (E12-2b): at no weight the clip fading out, at full weight the other,
            // between, between them; where the skin is.
            let (early, late) = (clip.duration() * 0.1, clip.duration() * 0.5);
            let posed = |posing: &mut Posing, fade: Fade| {
                let look = Look {
                    fade,
                    ..Look::at(Some((0, late)))
                };
                posing.palette(&rig, look, [None; 2]).to_vec()
            };
            let off = |a: &[[f32; 12]], b: &[[f32; 12]]| {
                (0..part.mesh.positions.len())
                    .step_by(step)
                    .map(|v| (skinned(part, a, v) - skinned(part, b, v)).length())
                    .fold(0.0f32, f32::max)
            };
            let at_early = posing
                .palette(&rig, Look::at(Some((0, early))), [None; 2])
                .to_vec();
            let at_late = posed(&mut posing, None);
            let apart = off(&at_early, &at_late);
            assert!(
                apart > 1e-3 * reach,
                "{path}: {} moves between",
                clip.name()
            );
            let faded_out = posed(&mut posing, Some((0, early, 0.0)));
            assert!(off(&faded_out, &at_early) < 1e-4 * reach, "{path}");
            assert_eq!(posed(&mut posing, Some((0, early, 1.0))), at_late);
            let half = posed(&mut posing, Some((0, early, 0.5)));
            let (to_early, to_late) = (off(&half, &at_early), off(&half, &at_late));
            assert!(
                to_early > 0.1 * apart && to_late > 0.1 * apart,
                "{path}: halfway is neither ({to_early}, {to_late} of {apart})"
            );
            // Root motion (E12-2c): up seen from the root's parent is the mesh's up; a clip's
            // motion over its whole length crosses the ground as far as its root speed says;
            // taken out of the pose, the root stays where the clip's first moment has it.
            let up = rig.mesh_up();
            let seen = rig
                .motion_frame
                .transform_vector3(rig.motion_up)
                .normalize();
            assert!(
                (seen - up).length() < 1e-4 && (up.length() - 1.0).abs() < 1e-5,
                "{path}: up {up}, from the root's parent {seen}"
            );
            let ground = |v: Vec3| v - up * v.dot(up);
            for (k, c) in rig.clips().iter().enumerate() {
                let motion = rig
                    .clip_motion(k, 0.0, c.duration(), Axes::default())
                    .unwrap();
                let (moved, turned) = rig.mesh_motion(motion);
                let crossed = ground(moved).length();
                println!(
                    "{path} {}: root motion {moved} ({crossed} at {} a second), turn {turned}",
                    c.name(),
                    c.root_speed()
                );
                assert!(
                    (crossed - c.root_speed() * c.duration()).abs() <= 1e-3 * reach.max(crossed),
                    "{path} {}: {crossed} against {} a second",
                    c.name(),
                    c.root_speed()
                );
                assert!(
                    moved.dot(up).abs() <= 1e-4 * reach,
                    "{path}: on the ground: {moved}"
                );
            }
            let in_place = |posing: &mut Posing, time: f32| {
                let look = Look {
                    in_place: true,
                    ..Look::at(Some((0, time)))
                };
                let joint = posing.joints(&rig, look, [None; 2])[rig.motion_root];
                Vec3::from(joint.translation)
            };
            let (start, later) = (in_place(&mut posing, 0.0), in_place(&mut posing, late));
            assert!(
                ground(later - start).length() < 1e-3 * reach,
                "{path}: the root kept in place: {start} then {later}"
            );
            // No clip, or one it has not: the rest pose.
            assert_eq!(
                posing.palette(&rig, Look::default(), [None; 2]),
                &rig.rest[..]
            );
            assert_eq!(
                posing.palette(&rig, Look::at(Some((99, 0.0))), [None; 2]),
                &rig.rest[..]
            );
        }
    }

    /// E12-1b: a clip's ends: its root's speed across the ground, and its loop's seam with
    /// the root's crossing and turn taken out (a walk forward and a turn on the spot loop;
    /// an arm raised and not lowered jumps, at the arm).
    #[test]
    fn a_clips_ends_give_its_root_speed_and_its_loops_seam() {
        use engine_anim::{Interpolation, JointDesc, RawClip, RawJointTracks, RawTrack, Transform};
        let joints = [
            ("hips", None, Vec3::new(0.0, 1.0, 0.0)),
            ("spine", Some(0), Vec3::new(0.0, 0.5, 0.0)),
            ("arm", Some(1), Vec3::new(0.3, 0.3, 0.0)),
        ]
        .map(|(name, parent, at)| JointDesc {
            name: name.into(),
            parent,
            rest: Transform::new(at, Quat::IDENTITY, Vec3::ONE),
        });
        let (skeleton, _) = Skeleton::new(&joints).unwrap();
        let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0).collect();
        let track = |f: &dyn Fn(f32) -> Quat| RawTrack {
            interpolation: Interpolation::Linear,
            times: times.clone(),
            values: times.iter().map(|&t| f(t)).collect(),
        };
        let clip = |hips: RawJointTracks, arm: Option<RawTrack<Quat>>| {
            let raw = RawClip {
                name: String::from("clip"),
                duration: 1.0,
                joints: vec![
                    hips,
                    RawJointTracks {
                        // A sway that comes back: the same at both ends.
                        rotation: Some(track(&|t| {
                            Quat::from_rotation_x(0.3 * (core::f32::consts::TAU * t).sin())
                        })),
                        ..RawJointTracks::default()
                    },
                    RawJointTracks {
                        rotation: arm,
                        ..RawJointTracks::default()
                    },
                ],
            };
            compress(&raw, &skeleton, &CompressSettings::default()).0
        };
        let shells = [0.2, 0.2, 0.4];
        // Forward 1.5 m in a second, bobbing: 1.5 m/s, and it loops.
        let walk = clip(
            RawJointTracks {
                translation: Some(RawTrack {
                    interpolation: Interpolation::Linear,
                    times: times.clone(),
                    values: times
                        .iter()
                        .map(|&t| {
                            Vec3::new(
                                0.0,
                                1.0 + 0.05 * (core::f32::consts::TAU * t).cos(),
                                1.5 * t,
                            )
                        })
                        .collect(),
                }),
                ..RawJointTracks::default()
            },
            None,
        );
        let (speed, seam, _) = ends(&walk, &skeleton, &shells, 0);
        assert!((speed - 1.5).abs() < 1e-3, "walk: {speed} m/s");
        assert!(seam < 1e-3, "walk: seam {seam}");
        // A quarter turn on the spot: no speed, and it loops (the turn is the root's).
        let turn = clip(
            RawJointTracks {
                rotation: Some(track(&|t| {
                    Quat::from_rotation_y(core::f32::consts::FRAC_PI_2 * t)
                })),
                ..RawJointTracks::default()
            },
            None,
        );
        let (speed, seam, _) = ends(&turn, &skeleton, &shells, 0);
        assert!(speed < 1e-3, "turn: {speed} m/s");
        assert!(seam < 1e-3, "turn: seam {seam}");
        // The arm raised half a radian and left there: about 2 · 0.4 · sin(0.25) at the arm.
        let raise = clip(
            RawJointTracks::default(),
            Some(track(&|t| Quat::from_rotation_z(0.5 * t))),
        );
        let (speed, seam, joint) = ends(&raise, &skeleton, &shells, 0);
        assert!(speed < 1e-3, "raise: {speed} m/s");
        let expected = 2.0 * 0.4 * 0.25f32.sin();
        assert!(
            (seam - expected).abs() < 2e-3,
            "raise: seam {seam} against {expected}"
        );
        assert_eq!(skeleton.names()[joint], "arm");
    }

    /// ADR 0124's E12-4e: a clip made in the editor keying a joint poses the rig over its rest
    /// pose: that joint turned as keyed (its children carried with it), the joints it does
    /// not key at rest; a clip keying no joint leaves the rest pose's palette.
    #[test]
    fn a_clip_made_in_the_editor_poses_the_joints_it_keys() {
        let Some(file) = sample("CesiumMan/CesiumMan.gltf", 1.0) else {
            return;
        };
        let mesh = (0..file.scene.meshes.len())
            .find(|&m| skinned_node(&file.scene, m).is_some())
            .unwrap();
        let (_, rig) = build(&file, mesh, false).unwrap().unwrap();
        let skeleton = rig.skeleton();
        // A joint with a child: turned 90 degrees about up at a second.
        let joint = (0..skeleton.joint_count())
            .find(|&j| (0..skeleton.joint_count()).any(|c| skeleton.parent(c) == Some(j)))
            .unwrap();
        let child = (0..skeleton.joint_count())
            .find(|&c| skeleton.parent(c) == Some(joint))
            .unwrap();
        let name = skeleton.names()[joint].clone();
        let mut anim = engine_scene::AnimFile::default();
        let target = engine_scene::joint_target(&name, "rotation");
        anim.set_key(&target, 0.0, &[0.0, 0.0, 0.0]);
        anim.set_key(&target, 1.0, &[0.0, 90.0, 0.0]);
        let clip = crate::field_clips::FieldClip::new(&anim);
        assert!(clip.keys_joints());
        let file_id = engine_scene::AssetId::new();
        let look = |t: f32| Look {
            own: Some(OwnPose {
                file: file_id,
                time: t,
                playhead: true,
            }),
            ..Look::default()
        };
        let mut posing = Posing::default();
        let rest = posing.joints(&rig, Look::default(), [None; 2]).to_vec();
        let at_end = posing.joints(&rig, look(1.0), [Some(&clip), None]).to_vec();
        // The joint's own turn: its local rotation as keyed, at the playhead's end.
        let mut pose = engine_anim::Pose::identity(skeleton.joint_count());
        pose.copy_from(skeleton.rest());
        clip.pose_joints(1.0, true, skeleton, &mut pose);
        let keyed = Quat::from_rotation_y(90f32.to_radians());
        assert!(pose.rotations[joint].angle_between(keyed) < 1e-3);
        // An animator's time half way: 45 degrees; at 1 s the looping clip begins again.
        clip.pose_joints(0.5, false, skeleton, &mut pose);
        let half = Quat::from_rotation_y(45f32.to_radians());
        assert!(pose.rotations[joint].angle_between(half) < 1e-3);
        clip.pose_joints(1.0, false, skeleton, &mut pose);
        assert!(
            pose.rotations[joint].angle_between(Quat::IDENTITY) < 1e-3,
            "wrapped"
        );
        for j in (0..skeleton.joint_count()).filter(|&j| j != joint) {
            assert_eq!(
                pose.rotations[j],
                skeleton.rest().rotations[j],
                "joint {j} at rest"
            );
        }
        // Its child moved with it (unless it sits on the joint), the root's place kept.
        let moved =
            (Vec3::from(at_end[child].translation) - Vec3::from(rest[child].translation)).length();
        let offset = Vec3::from(skeleton.rest().translations[child]).length();
        assert!(
            offset < 1e-6 || moved > 1e-4 * offset,
            "the child moved {moved}"
        );
        let root = (0..skeleton.joint_count())
            .find(|&j| skeleton.parent(j).is_none())
            .unwrap();
        if root != joint {
            assert!(
                (Vec3::from(at_end[root].translation) - Vec3::from(rest[root].translation))
                    .length()
                    < 1e-5
            );
        }
        // At the start the clip keys the rest's turn of none: the joint's local turn is 0.
        clip.pose_joints(0.0, true, skeleton, &mut pose);
        assert!(pose.rotations[joint].angle_between(Quat::IDENTITY) < 1e-4);
        // A clip without joints' tracks: the rest pose's palette.
        let mut fields = engine_scene::AnimFile::default();
        fields.set_key("Transform.rotation", 0.0, &[0.0, 45.0, 0.0]);
        let plain = crate::field_clips::FieldClip::new(&fields);
        assert!(!plain.keys_joints());
        assert_eq!(
            posing.palette(&rig, look(0.5), [Some(&plain), None]),
            &rig.rest[..]
        );
        // Fades (a clip of the animator's own into another, or out of the rest pose of one
        // keying no joint): the joint's turn half way at half the weight.
        let mut held = engine_scene::AnimFile::default();
        held.set_key(&target, 0.0, &[0.0, 90.0, 0.0]);
        let mut zero = engine_scene::AnimFile::default();
        zero.set_key(&target, 0.0, &[0.0, 0.0, 0.0]);
        let (held, zero) = (
            crate::field_clips::FieldClip::new(&held),
            crate::field_clips::FieldClip::new(&zero),
        );
        let shown = OwnPose {
            file: file_id,
            time: 0.0,
            playhead: false,
        };
        let look = Look {
            own: Some(shown),
            own_fade: Some((shown, 0.5)),
            ..Look::default()
        };
        let turn = |posing: &mut Posing, own: [Option<&crate::field_clips::FieldClip>; 2]| {
            assert!(posing.local(&rig, look, own));
            posing.pose.as_ref().unwrap().rotations[joint]
        };
        let half = Quat::from_rotation_y(45f32.to_radians());
        let both = turn(&mut posing, [Some(&zero), Some(&held)]);
        assert!(
            both.angle_between(half) < 1e-3,
            "between two of its own clips"
        );
        let rest_turn = skeleton.rest().rotations[joint];
        let mid = rest_turn.slerp(Quat::from_rotation_y(90f32.to_radians()), 0.5);
        let from_rest = turn(&mut posing, [Some(&plain), Some(&held)]);
        assert!(
            from_rest.angle_between(mid) < 1e-3,
            "out of one keying no joint"
        );
        // Two keying no joint: the rest pose.
        assert!(!posing.local(&rig, look, [Some(&plain), Some(&plain)]));
        assert_eq!(
            posing.palette(&rig, look, [Some(&plain), Some(&plain)]),
            &rig.rest[..]
        );
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
