//! Assets dropped from the FileSystem dock (ADR 0107 E4-3): the entities a model file
//! becomes, and where a model rests on the surface under the pointer.

use engine_app::{Model, builtin};
use engine_assets::GltfOutline;
use engine_core::math::{Affine3A, Quat, Vec3};
use engine_ecs::Transform;
use engine_scene::{AssetId, AssetRef};

/// A model file's entities as a drop makes them: one per node of the file's scene under a
/// root named after the file, or, for a file of one node, that node as the root. A skinned
/// model is one entity, its joints none (ADR 0124's E12-1c, question 6a; see
/// [`ModelPlan::skinned`]).
#[derive(Debug, Clone)]
pub(crate) struct ModelPlan {
    /// The entities, parents before children; the first is the root.
    pub(crate) nodes: Vec<PlannedNode>,
    /// The parts the models name with their paths, for the player, the inspector and
    /// saving.
    pub(crate) paths: Vec<(AssetRef, String)>,
    /// The file's meshes and materials as parts (`Seat`, or `#2` for an unnamed one or one
    /// whose name another has).
    pub(crate) meshes: Vec<String>,
    pub(crate) materials: Vec<String>,
    /// The model's box in the root's space (none without bounds in the file).
    pub(crate) bounds: Option<(Vec3, Vec3)>,
}

/// An entity of a [`ModelPlan`].
#[derive(Debug, Clone)]
pub(crate) struct PlannedNode {
    pub(crate) name: String,
    /// In its parent's space; the root's translation is where it is dropped.
    pub(crate) transform: Transform,
    /// Its parent's index in [`ModelPlan::nodes`] (none: the root).
    pub(crate) parent: Option<usize>,
    pub(crate) model: Option<Model>,
    /// The materials of its mesh's further primitives, for a mesh of several
    /// (`engine_app::MaterialSlots`, ADR 0115); empty for one of a single material.
    pub(crate) slots: Vec<AssetRef>,
}

impl ModelPlan {
    /// The plan of glTF file `file`, at `path` under the project's folder, from its
    /// outline; its root named `name`. `scale` is the file's import setting (E4-9): the
    /// player scales the meshes by it, and the plan the nodes' places and the box.
    pub(crate) fn new(
        file: AssetId,
        path: &str,
        name: &str,
        outline: &GltfOutline,
        scale: f32,
    ) -> Result<Self, String> {
        let mesh_names: Vec<&str> = outline.meshes.iter().map(|m| m.name.as_str()).collect();
        let material_names: Vec<&str> = outline.materials.iter().map(String::as_str).collect();
        let meshes: Vec<String> = (0..mesh_names.len())
            .map(|i| part_name(&mesh_names, i))
            .collect();
        let materials: Vec<String> = (0..material_names.len())
            .map(|i| part_name(&material_names, i))
            .collect();
        let mut plan = Self {
            nodes: vec![PlannedNode {
                name: name.to_owned(),
                transform: Transform::IDENTITY,
                parent: None,
                model: None,
                slots: Vec::new(),
            }],
            paths: Vec::new(),
            meshes,
            materials,
            bounds: None,
        };
        if let Some(mesh_of) = plan.skinned(file, path, outline, scale) {
            plan.bounds = bounds(&plan, &mesh_of, outline, scale);
            return Ok(plan);
        }
        // The scene's nodes under the root, parents first; each node once (a malformed
        // file's cycles are cut).
        let mut mesh_of: Vec<Option<usize>> = vec![None];
        let mut seen = vec![false; outline.nodes.len()];
        let mut stack: Vec<(usize, usize)> = outline.roots.iter().rev().map(|&r| (r, 0)).collect();
        while let Some((i, parent)) = stack.pop() {
            if i >= outline.nodes.len() || core::mem::replace(&mut seen[i], true) {
                continue;
            }
            let n = &outline.nodes[i];
            let mesh = drawn(outline, i);
            let (model, slots) = match mesh.map(|m| plan.model(file, path, outline, m)) {
                Some((model, slots)) => (Some(model), slots),
                None => (None, Vec::new()),
            };
            let index = plan.nodes.len();
            plan.nodes.push(PlannedNode {
                name: node_name(outline, i, mesh),
                transform: Transform::from_translation(Vec3::from(n.translation) * scale)
                    .with_rotation(Quat::from_array(n.rotation))
                    .with_scale(Vec3::from(n.scale)),
                parent: Some(parent),
                model,
                slots,
            });
            mesh_of.push(mesh);
            for &c in n.children.iter().rev() {
                stack.push((c, index));
            }
        }
        if mesh_of.iter().all(Option::is_none) {
            return Err(format!("{name} has no meshes to place"));
        }
        // A file of one node: that node is the root, where it is dropped, turned and
        // scaled as the file has it.
        if plan.nodes.len() == 2
            && let Some(node) = plan.nodes.pop()
        {
            plan.nodes[0].model = node.model;
            plan.nodes[0].slots = node.slots;
            plan.nodes[0].transform.rotation = node.transform.rotation;
            plan.nodes[0].transform.scale = node.transform.scale;
            mesh_of[0] = mesh_of.pop().flatten();
        }
        plan.bounds = bounds(&plan, &mesh_of, outline, scale);
        Ok(plan)
    }

    /// A skinned model's entities (ADR 0124's E12-1c, question 6a): its skeleton's joints
    /// are not entities, nor are the nodes that draw nothing. The first node that draws its
    /// mesh skinned is the root, turned and scaled as the file has it (its place is where it
    /// is dropped, as for a file of one node); the file's other meshes go under the nearest
    /// node above them that is an entity (the root when none is), at their places in the
    /// file, a mesh under a joint where the skeleton's rest pose holds it. Each entity's
    /// mesh, parents first; none for a file that draws nothing skinned, which keeps an
    /// entity per node.
    fn skinned(
        &mut self,
        file: AssetId,
        path: &str,
        outline: &GltfOutline,
        scale: f32,
    ) -> Option<Vec<Option<usize>>> {
        if outline.nodes.iter().all(|n| n.skin.is_none()) {
            return None;
        }
        // The scene's nodes, parents first, each once (a malformed file's cycles are cut),
        // with its parent's visit and its place in the file (translations scaled as the
        // meshes are).
        let mut visits: Vec<(usize, Option<usize>, Affine3A)> = Vec::new();
        let mut seen = vec![false; outline.nodes.len()];
        let mut stack: Vec<(usize, Option<usize>)> =
            outline.roots.iter().rev().map(|&r| (r, None)).collect();
        while let Some((i, parent)) = stack.pop() {
            if i >= outline.nodes.len() || core::mem::replace(&mut seen[i], true) {
                continue;
            }
            let n = &outline.nodes[i];
            let local = Affine3A::from_scale_rotation_translation(
                Vec3::from(n.scale),
                Quat::from_array(n.rotation),
                Vec3::from(n.translation) * scale,
            );
            let at = parent.map_or(local, |p| visits[p].2 * local);
            let index = visits.len();
            visits.push((i, parent, at));
            for &c in n.children.iter().rev() {
                stack.push((c, Some(index)));
            }
        }
        let first = visits.iter().position(|&(i, _, _)| {
            outline.nodes[i].skin.is_some() && drawn(outline, i).is_some()
        })?;
        let (i, _, root_at) = visits[first];
        let mesh = drawn(outline, i)?;
        let (model, slots) = self.model(file, path, outline, mesh);
        let (s, r, _) = root_at.to_scale_rotation_translation();
        let root = &mut self.nodes[0];
        root.transform = Transform::IDENTITY.with_rotation(r).with_scale(s);
        root.model = Some(model);
        root.slots = slots;
        let mut mesh_of = vec![Some(mesh)];
        // The entity of each visit that is one.
        let mut entity: Vec<Option<usize>> = vec![None; visits.len()];
        entity[first] = Some(0);
        for (v, &(i, parent, at)) in visits.iter().enumerate() {
            let Some(m) = drawn(outline, i).filter(|_| v != first) else {
                continue;
            };
            let mut above = parent;
            let (p, p_at) = loop {
                match above {
                    Some(a) => match entity[a] {
                        Some(e) => break (e, visits[a].2),
                        None => above = visits[a].1,
                    },
                    None => break (0, root_at),
                }
            };
            // Under a parent scaled to nothing it is nowhere: kept where the file has it.
            let local = if p_at.matrix3.determinant() != 0.0 {
                p_at.inverse() * at
            } else {
                at
            };
            let (s, r, t) = local.to_scale_rotation_translation();
            let (model, slots) = self.model(file, path, outline, m);
            entity[v] = Some(self.nodes.len());
            self.nodes.push(PlannedNode {
                name: node_name(outline, i, Some(m)),
                transform: Transform::from_translation(t)
                    .with_rotation(r)
                    .with_scale(s),
                parent: Some(p),
                model: Some(model),
                slots,
            });
            mesh_of.push(Some(m));
        }
        Some(mesh_of)
    }

    /// The model of mesh `m`: its part with the first primitive's material (the default
    /// one without), and for a mesh of several materials the further primitives' in its
    /// slots (ADR 0115); a mesh of one material draws whole, without slots.
    fn model(
        &mut self,
        file: AssetId,
        path: &str,
        outline: &GltfOutline,
        m: usize,
    ) -> (Model, Vec<AssetRef>) {
        let mesh = AssetRef::part(file, &self.meshes[m]);
        self.add_path(mesh, format!("{path}#{}", self.meshes[m]));
        let primitives = &outline.meshes[m].primitives;
        let mut material_of = |k: Option<usize>| match k.filter(|&k| k < self.materials.len()) {
            Some(k) => {
                let material = AssetRef::part(file, &self.materials[k]);
                self.add_path(material, format!("{path}#{}", self.materials[k]));
                material
            }
            None => builtin::MATERIAL,
        };
        let first = material_of(primitives.first().and_then(|p| p.material));
        let several = primitives
            .iter()
            .any(|p| Some(p.material) != primitives.first().map(|f| f.material));
        let slots = if several {
            primitives[1..]
                .iter()
                .map(|p| material_of(p.material))
                .collect()
        } else {
            Vec::new()
        };
        (Model::new(mesh, first), slots)
    }

    fn add_path(&mut self, asset: AssetRef, path: String) {
        if self.paths.iter().all(|(a, _)| *a != asset) {
            self.paths.push((asset, path));
        }
    }
}

/// The mesh node `i` draws, for one with primitives.
fn drawn(outline: &GltfOutline, i: usize) -> Option<usize> {
    outline.nodes[i].mesh.filter(|&m| {
        outline
            .meshes
            .get(m)
            .is_some_and(|m| !m.primitives.is_empty())
    })
}

/// Node `i`'s entity's name: its own, else its mesh's (`mesh`), else `Node i`.
fn node_name(outline: &GltfOutline, i: usize, mesh: Option<usize>) -> String {
    let named = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    named(&outline.nodes[i].name)
        .or_else(|| mesh.and_then(|m| named(&outline.meshes[m].name)))
        .unwrap_or_else(|| format!("Node {i}"))
}

/// The part that names thing `i` of `names`: its name when nothing else has it, else
/// `#i` (also for names starting with `#`, which read as indices).
pub(crate) fn part_name(names: &[&str], i: usize) -> String {
    let name = names[i];
    if !name.is_empty()
        && !name.starts_with('#')
        && names.iter().filter(|&&n| n == name).count() == 1
    {
        name.to_owned()
    } else {
        format!("#{i}")
    }
}

/// The box around the plan's meshes in its root's space (the root's translation left
/// out), from the primitives' bounds, scaled by the file's import scale as the meshes are.
fn bounds(
    plan: &ModelPlan,
    mesh_of: &[Option<usize>],
    outline: &GltfOutline,
    scale: f32,
) -> Option<(Vec3, Vec3)> {
    let mut placed: Vec<Affine3A> = Vec::with_capacity(plan.nodes.len());
    let mut out: Option<(Vec3, Vec3)> = None;
    for (k, node) in plan.nodes.iter().enumerate() {
        let t = node.transform;
        let local = Affine3A::from_scale_rotation_translation(t.scale, t.rotation, t.translation);
        let at = match node.parent {
            Some(p) => placed[p] * local,
            None => Affine3A::from_scale_rotation_translation(t.scale, t.rotation, Vec3::ZERO),
        };
        placed.push(at);
        let Some(m) = mesh_of[k] else {
            continue;
        };
        for (lo, hi) in outline.meshes[m].primitives.iter().filter_map(|p| p.bounds) {
            for corner in 0..8 {
                let pick = |bit: usize, axis: usize| {
                    if corner & bit == 0 {
                        lo[axis]
                    } else {
                        hi[axis]
                    }
                };
                let corner = Vec3::new(pick(1, 0), pick(2, 1), pick(4, 2)) * scale;
                let p = at.transform_point3(corner);
                out = Some(out.map_or((p, p), |(a, b)| (a.min(p), b.max(p))));
            }
        }
    }
    out
}

/// Where a model's root goes for its box (`bounds`, in the root's space) to rest on the
/// surface through `point` facing `normal`: the box's lowest point along the normal on
/// the surface. Without a box, the root's origin is on it.
pub(crate) fn resting_point(point: Vec3, normal: Vec3, bounds: Option<(Vec3, Vec3)>) -> Vec3 {
    let Some((lo, hi)) = bounds else {
        return point;
    };
    let n = normal.normalize_or(Vec3::Y);
    let lowest = Vec3::select(n.cmpge(Vec3::ZERO), lo, hi).dot(n);
    point - n * lowest
}

/// `point` with its coordinates along the surface facing `normal` (on the axes the normal
/// mostly does not follow) on the grid of `step`.
pub(crate) fn snap_along(point: Vec3, normal: Vec3, step: f32) -> Vec3 {
    if step <= 0.0 {
        return point;
    }
    let n = normal.normalize_or(Vec3::Y).abs();
    let snap = |v: f32, along: f32| {
        if along < 0.5 {
            (v / step).round() * step
        } else {
            v
        }
    };
    Vec3::new(snap(point.x, n.x), snap(point.y, n.y), snap(point.z, n.z))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_assets::{OutlineMesh, OutlineNode, OutlinePrimitive};

    fn node(name: &str, mesh: Option<usize>, children: Vec<usize>) -> OutlineNode {
        OutlineNode {
            name: name.to_owned(),
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            mesh,
            skin: None,
            children,
        }
    }

    /// A node drawing mesh `mesh` skinned.
    fn skinned(name: &str, mesh: usize, children: Vec<usize>) -> OutlineNode {
        OutlineNode {
            skin: Some(0),
            ..node(name, Some(mesh), children)
        }
    }

    fn names(plan: &ModelPlan) -> Vec<&str> {
        plan.nodes.iter().map(|n| n.name.as_str()).collect()
    }

    fn mesh(name: &str, materials: &[Option<usize>]) -> OutlineMesh {
        OutlineMesh {
            name: name.to_owned(),
            primitives: materials
                .iter()
                .map(|&material| OutlinePrimitive {
                    material,
                    bounds: Some(([-0.5, 0.0, -0.5], [0.5, 1.0, 0.5])),
                })
                .collect(),
        }
    }

    fn file() -> AssetId {
        AssetId::parse("5f2c0a3e9d2b47c88e51f0d4a7b3c912").unwrap()
    }

    #[test]
    fn a_file_of_one_node_is_one_entity_turned_and_scaled_as_the_file_has_it() {
        let mut crate_node = node("Crate", Some(0), Vec::new());
        crate_node.translation = [5.0, 5.0, 5.0];
        crate_node.scale = [2.0; 3];
        let outline = GltfOutline {
            nodes: vec![crate_node],
            roots: vec![0],
            meshes: vec![mesh("Crate", &[Some(0)])],
            materials: vec![String::from("Wood")],
            maps: Vec::new(),
        };
        let plan = ModelPlan::new(file(), "props/crate.glb", "crate", &outline, 1.0).unwrap();
        assert_eq!(plan.nodes.len(), 1);
        let root = &plan.nodes[0];
        assert_eq!(root.name, "crate");
        assert_eq!(
            root.transform.translation,
            Vec3::ZERO,
            "where it is dropped"
        );
        assert_eq!(root.transform.scale, Vec3::splat(2.0));
        let model = root.model.unwrap();
        assert_eq!(model.mesh, AssetRef::part(file(), "Crate"));
        assert_eq!(model.material, AssetRef::part(file(), "Wood"));
        assert!(plan.paths.contains(&(
            AssetRef::part(file(), "Wood"),
            String::from("props/crate.glb#Wood")
        )));
        // Its box with the file's scale, without its translation.
        assert_eq!(
            plan.bounds,
            Some((Vec3::new(-1.0, 0.0, -1.0), Vec3::new(1.0, 2.0, 1.0)))
        );
        assert!(plan.nodes.iter().all(|n| n.slots.is_empty()));
    }

    #[test]
    fn a_tree_of_nodes_goes_under_a_root_named_after_the_file() {
        let mut leg = node("", Some(1), Vec::new());
        leg.translation = [0.0, -1.0, 0.0];
        let outline = GltfOutline {
            nodes: vec![
                node("Chair", None, vec![1, 2]),
                leg,
                node("Seat", Some(0), Vec::new()),
            ],
            roots: vec![0],
            // Two meshes of one name: by index.
            meshes: vec![mesh("Part", &[Some(0), Some(1)]), mesh("Part", &[None])],
            materials: vec![String::from("Wood"), String::new()],
            maps: Vec::new(),
        };
        let plan = ModelPlan::new(file(), "chair.glb", "chair", &outline, 1.0).unwrap();
        let names: Vec<&str> = plan.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            ["chair", "Chair", "Part", "Seat"],
            "an unnamed node takes its mesh's name"
        );
        let parents: Vec<Option<usize>> = plan.nodes.iter().map(|n| n.parent).collect();
        assert_eq!(parents, [None, Some(0), Some(1), Some(1)]);
        assert_eq!(plan.meshes, ["#0", "#1"]);
        assert_eq!(plan.materials, ["Wood", "#1"]);
        let leg_model = plan.nodes[2].model.unwrap();
        assert_eq!(leg_model.mesh, AssetRef::part(file(), "#1"));
        assert_eq!(
            leg_model.material,
            builtin::MATERIAL,
            "no material: the default one"
        );
        // The seat's mesh of two materials: the second in its slot (ADR 0115); the leg's
        // of one, none.
        assert_eq!(plan.nodes[3].slots, [AssetRef::part(file(), "#1")]);
        assert!(plan.nodes[2].slots.is_empty());
        // The leg's box a meter lower.
        let (lo, hi) = plan.bounds.unwrap();
        assert_eq!(lo.y, -1.0);
        assert_eq!(hi.y, 1.0);
        // Imported at half its size (its `.meta`, E4-9): the leg half a meter lower, the
        // box half as tall.
        let half = ModelPlan::new(file(), "chair.glb", "chair", &outline, 0.5).unwrap();
        assert_eq!(
            half.nodes[2].transform.translation,
            Vec3::new(0.0, -0.5, 0.0)
        );
        let (lo, hi) = half.bounds.unwrap();
        assert_eq!((lo.y, hi.y), (-0.5, 0.5));
    }

    /// ADR 0124's E12-1c (question 6a): a character as CesiumMan's file has it, a turned
    /// node above the skeleton and the skinned mesh beside the joints, is one entity.
    #[test]
    fn a_skinned_model_is_one_entity_without_its_joints_and_empty_nodes() {
        let mut up = node("Z_UP", None, vec![1]);
        up.rotation = Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2).to_array();
        let mut armature = node("Armature", None, vec![2, 4]);
        armature.translation = [0.0, 0.0, 1.0];
        let mut torso = node("Torso", None, vec![3]);
        torso.translation = [0.0, 0.0, 1.0];
        let outline = GltfOutline {
            nodes: vec![
                up,
                armature,
                torso,
                node("Arm", None, Vec::new()),
                skinned("Body", 0, Vec::new()),
            ],
            roots: vec![0],
            meshes: vec![mesh("Body", &[Some(0)])],
            materials: vec![String::from("Skin")],
            maps: Vec::new(),
        };
        let plan = ModelPlan::new(file(), "man.gltf", "man", &outline, 1.0).unwrap();
        assert_eq!(names(&plan), ["man"], "one entity");
        let root = &plan.nodes[0];
        let model = root.model.unwrap();
        assert_eq!(model.mesh, AssetRef::part(file(), "Body"));
        assert_eq!(model.material, AssetRef::part(file(), "Skin"));
        // Turned as the file has it (its z up), where it is dropped.
        assert_eq!(root.transform.translation, Vec3::ZERO);
        let turn = Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2);
        assert!(root.transform.rotation.dot(turn).abs() > 1.0 - 1e-6);
        assert!((root.transform.scale - Vec3::ONE).abs().max_element() < 1e-6);
        // Its box turned with it: the mesh's height (its y) along -z.
        let (lo, hi) = plan.bounds.unwrap();
        let near = |a: Vec3, b: Vec3| (a - b).abs().max_element() < 1e-6;
        assert!(near(lo, Vec3::new(-0.5, -0.5, -1.0)), "{lo}");
        assert!(near(hi, Vec3::new(0.5, 0.5, 0.0)), "{hi}");
        // The Fox's file: its skeleton under a node of its own beside the mesh, in
        // centimetres imported at a hundredth.
        let outline = GltfOutline {
            nodes: vec![
                node("root", None, vec![2]),
                skinned("fox", 0, Vec::new()),
                node("_rootJoint", None, Vec::new()),
            ],
            roots: vec![0, 1],
            meshes: vec![mesh("fox1", &[Some(0)])],
            materials: vec![String::from("fox_material")],
            maps: Vec::new(),
        };
        let plan = ModelPlan::new(file(), "Fox.gltf", "Fox", &outline, 0.01).unwrap();
        assert_eq!(names(&plan), ["Fox"]);
        assert_eq!(plan.nodes[0].transform, Transform::IDENTITY);
        let (lo, hi) = plan.bounds.unwrap();
        assert!(near(lo, Vec3::new(-0.005, 0.0, -0.005)), "{lo}");
        assert!(near(hi, Vec3::new(0.005, 0.01, 0.005)), "{hi}");
        // A file whose skin no node draws keeps an entity per node.
        let mut outline = outline;
        outline.nodes[1].mesh = None;
        outline.nodes.push(node("Rock", Some(0), Vec::new()));
        outline.roots.push(3);
        let plan = ModelPlan::new(file(), "Fox.gltf", "Fox", &outline, 1.0).unwrap();
        assert_eq!(names(&plan), ["Fox", "root", "_rootJoint", "fox", "Rock"]);
    }

    /// E12-1c: the file's other meshes stay, at their places in the file, under the nearest
    /// node above them that is an entity: a sword in the hand's joint where the rest pose
    /// holds it, a gem on the sword, a hat on the skinned body.
    #[test]
    fn a_skinned_models_other_meshes_stay_where_the_file_has_them() {
        let mut hand = node("Hand", None, vec![2]);
        hand.translation = [1.0, 2.0, 0.0];
        let mut sword = node("Sword", Some(1), vec![5]);
        sword.translation = [0.0, 0.5, 0.0];
        let mut body = skinned("Body", 0, vec![4]);
        body.translation = [0.0, 1.0, 0.0];
        let mut hat = node("Hat", Some(1), Vec::new());
        hat.translation = [0.0, 1.0, 0.0];
        let mut gem = node("", Some(2), Vec::new());
        gem.translation = [0.0, 0.25, 0.0];
        let outline = GltfOutline {
            nodes: vec![
                node("Armature", None, vec![1, 3]),
                hand,
                sword,
                body,
                hat,
                gem,
            ],
            roots: vec![0],
            meshes: vec![
                mesh("Body", &[Some(0)]),
                mesh("Prop", &[Some(1)]),
                mesh("Gem", &[Some(1)]),
            ],
            materials: vec![String::from("Skin"), String::from("Steel")],
            maps: Vec::new(),
        };
        for scale in [1.0, 0.5] {
            let plan = ModelPlan::new(file(), "knight.glb", "knight", &outline, scale).unwrap();
            assert_eq!(names(&plan), ["knight", "Sword", "Gem", "Hat"]);
            let parents: Vec<Option<usize>> = plan.nodes.iter().map(|n| n.parent).collect();
            assert_eq!(parents, [None, Some(0), Some(1), Some(0)]);
            let at: Vec<Vec3> = plan.nodes.iter().map(|n| n.transform.translation).collect();
            // The sword's place against the body's (a meter up): the hand's and its own.
            assert_eq!(
                at,
                [
                    Vec3::ZERO,
                    Vec3::new(1.0, 1.5, 0.0) * scale,
                    Vec3::new(0.0, 0.25, 0.0) * scale,
                    Vec3::new(0.0, 1.0, 0.0) * scale,
                ]
            );
            assert_eq!(
                plan.nodes[1].model.unwrap().mesh,
                AssetRef::part(file(), "Prop")
            );
            assert_eq!(
                plan.nodes[3].model.unwrap().material,
                AssetRef::part(file(), "Steel")
            );
            assert!(plan.paths.contains(&(
                AssetRef::part(file(), "Gem"),
                String::from("knight.glb#Gem")
            )));
            // The box from the body's feet (the root) to the sword's tip.
            let (lo, hi) = plan.bounds.unwrap();
            assert_eq!((lo.y, hi.y), (0.0, 2.75 * scale));
        }
    }

    /// E12-1c: the drop's plan of the real files (where tools/fetch-test-assets.ps1 put
    /// them): each character one entity, drawing its skinned mesh.
    #[test]
    fn the_test_characters_drop_as_one_entity() {
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets");
        for (path, mesh) in [
            ("CesiumMan/CesiumMan.gltf", "Cesium_Man"),
            ("Fox/Fox.gltf", "fox1"),
            ("RiggedSimple/RiggedSimple.gltf", "Cylinder"),
        ] {
            if !assets.join(path).exists() {
                println!("skipped: {path} missing");
                continue;
            }
            let outline = GltfOutline::read(&assets.join(path)).unwrap();
            let plan = ModelPlan::new(file(), path, "character", &outline, 1.0).unwrap();
            assert_eq!(names(&plan), ["character"], "{path}");
            assert_eq!(
                plan.nodes[0].model.unwrap().mesh,
                AssetRef::part(file(), mesh),
                "{path}"
            );
            let (lo, hi) = plan.bounds.unwrap();
            println!("{path}: {:?}, box {lo}..{hi}", plan.nodes[0].transform);
        }
    }

    #[test]
    fn a_file_without_meshes_is_not_placed() {
        let outline = GltfOutline {
            nodes: vec![node("Empty", None, Vec::new())],
            roots: vec![0],
            ..GltfOutline::default()
        };
        assert!(ModelPlan::new(file(), "empty.glb", "empty", &outline, 1.0).is_err());
    }

    #[test]
    fn a_model_rests_on_the_surface_by_its_box() {
        let bounds = Some((Vec3::new(-1.0, -0.5, -1.0), Vec3::new(1.0, 2.0, 1.0)));
        // On a floor: its bottom on it.
        assert_eq!(
            resting_point(Vec3::new(3.0, 1.0, 0.0), Vec3::Y, bounds),
            Vec3::new(3.0, 1.5, 0.0)
        );
        // On a wall facing -x: its +x side on it.
        assert_eq!(
            resting_point(Vec3::new(4.0, 1.0, 0.0), Vec3::NEG_X, bounds),
            Vec3::new(3.0, 1.0, 0.0)
        );
        assert_eq!(resting_point(Vec3::ONE, Vec3::Y, None), Vec3::ONE);
    }

    #[test]
    fn snapping_keeps_the_surface_and_rounds_along_it() {
        let p = Vec3::new(1.26, 0.37, -0.74);
        assert_eq!(snap_along(p, Vec3::Y, 0.5), Vec3::new(1.5, 0.37, -0.5));
        assert_eq!(snap_along(p, Vec3::X, 0.5), Vec3::new(1.26, 0.5, -0.5));
        assert_eq!(snap_along(p, Vec3::Y, 0.0), p);
    }
}
