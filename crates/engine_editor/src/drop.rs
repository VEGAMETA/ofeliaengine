//! Assets dropped from the FileSystem dock (ADR 0107 E4-3): the entities a model file
//! becomes, and where a model rests on the surface under the pointer.

use engine_app::{Model, builtin};
use engine_assets::GltfOutline;
use engine_core::math::{Affine3A, Quat, Vec3};
use engine_ecs::Transform;
use engine_scene::{AssetId, AssetRef};

/// A model file's entities as a drop makes them: one per node of the file's scene under a
/// root named after the file, or, for a file of one node, that node as the root.
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
    /// The meshes of several materials, which draw with their first: what to say.
    pub(crate) mixed: Vec<String>,
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
            }],
            paths: Vec::new(),
            meshes,
            materials,
            bounds: None,
            mixed: Vec::new(),
        };
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
            let mesh = n.mesh.filter(|&m| {
                outline
                    .meshes
                    .get(m)
                    .is_some_and(|m| !m.primitives.is_empty())
            });
            let model = mesh.map(|m| plan.model(file, path, outline, m));
            let named = |s: &str| (!s.is_empty()).then(|| s.to_owned());
            let node_name = named(&n.name)
                .or_else(|| mesh.and_then(|m| named(&outline.meshes[m].name)))
                .unwrap_or_else(|| format!("Node {i}"));
            let index = plan.nodes.len();
            plan.nodes.push(PlannedNode {
                name: node_name,
                transform: Transform::from_translation(Vec3::from(n.translation) * scale)
                    .with_rotation(Quat::from_array(n.rotation))
                    .with_scale(Vec3::from(n.scale)),
                parent: Some(parent),
                model,
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
            plan.nodes[0].transform.rotation = node.transform.rotation;
            plan.nodes[0].transform.scale = node.transform.scale;
            mesh_of[0] = mesh_of.pop().flatten();
        }
        plan.bounds = bounds(&plan, &mesh_of, outline, scale);
        Ok(plan)
    }

    /// The model of mesh `m`: its part, with the first primitive's material (the default
    /// one without); a mesh of several materials noted in [`ModelPlan::mixed`].
    fn model(&mut self, file: AssetId, path: &str, outline: &GltfOutline, m: usize) -> Model {
        let mesh = AssetRef::part(file, &self.meshes[m]);
        self.add_path(mesh, format!("{path}#{}", self.meshes[m]));
        let primitives = &outline.meshes[m].primitives;
        let first = primitives.first().and_then(|p| p.material);
        let material = match first.filter(|&k| k < self.materials.len()) {
            Some(k) => {
                let material = AssetRef::part(file, &self.materials[k]);
                self.add_path(material, format!("{path}#{}", self.materials[k]));
                material
            }
            None => builtin::MATERIAL,
        };
        let mut kinds: Vec<Option<usize>> = Vec::new();
        for p in primitives {
            if !kinds.contains(&p.material) {
                kinds.push(p.material);
            }
        }
        if kinds.len() > 1 {
            let shown = |s: &str, i: usize, what: &str| {
                if s.is_empty() {
                    format!("{what} {i}")
                } else {
                    s.to_owned()
                }
            };
            let mesh_name = shown(&outline.meshes[m].name, m, "mesh");
            let material_name = first.map_or_else(
                || String::from("the default material"),
                |k| shown(&outline.materials[k], k, "material"),
            );
            self.mixed.push(format!(
                "{mesh_name} has {} materials and draws with {material_name} alone",
                kinds.len()
            ));
        }
        Model::new(mesh, material)
    }

    fn add_path(&mut self, asset: AssetRef, path: String) {
        if self.paths.iter().all(|(a, _)| *a != asset) {
            self.paths.push((asset, path));
        }
    }
}

/// The part that names thing `i` of `names`: its name when nothing else has it, else
/// `#i` (also for names starting with `#`, which read as indices).
fn part_name(names: &[&str], i: usize) -> String {
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
            children,
        }
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
        assert!(plan.mixed.is_empty());
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
        assert_eq!(plan.mixed.len(), 1, "{:?}", plan.mixed);
        assert!(plan.mixed[0].contains("2 materials"), "{}", plan.mixed[0]);
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
