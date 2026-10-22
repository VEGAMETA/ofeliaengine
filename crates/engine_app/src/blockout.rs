//! Blockout brushes in the player (ADR 0110): a scene's [`Brush`] entities, grouped by
//! their nearest [`BrushGroup`] ancestor and evaluated in the tree's order, each drawn as
//! a mesh of the polygons it owns with a grid texture of its tint.
//!
//! ```text
//! [entity 1d3c…]
//! Name = "Blockout"
//! BrushGroup.baked = none
//!
//! [entity 6a0e…]
//! Parent = @1d3c…
//! Brush.op = Cut
//! Brush.shape = Box
//! Brush.size = [1, 2.1, 0.4]
//! ```
//!
//! The player keeps one [`engine_brush::Group`] per group, in its frame's presentation
//! step (where models turn into renderers): only the brushes whose component, place in the
//! tree or placement within the group changed are evaluated again, and only the brushes
//! whose polygons that changed get new meshes. A brush with no group acts on nothing but
//! itself.
//!
//! A brush's child brushes act on it first (ADR 0110's nesting, Godot's rule): they are
//! combined into it in the tree's order ([`engine_brush::combine`]), and what they make of
//! it acts in its place with its op. A window cut below a wall cuts that wall only.

use engine_brush::{Group, Plane, Shape, Triangles, triangles};
use engine_core::math::{Affine3A, DMat3, DVec3, Mat3, Vec3};
use engine_ecs::{
    Changed, Entity, GlobalTransform, Parent, QueryState, Transform, With, Without, World,
};
use engine_reflect::Reflect;
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable, TextureRef};
use engine_render::mesh::{GeometryPool, MeshData, MeshId};
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_rhi::{Allocator, Device};
use engine_runtime::MeshRenderer;
use engine_scene::AssetRef;
use std::collections::{HashMap, HashSet};

/// A grid unit in metres: brushes' planes are held on a 1 mm grid.
const UNIT: f64 = 0.001;

/// How far each polygon grows in its plane, in grid units, so T-junctions show no cracks.
const GROW: f64 = 0.05;

/// What a brush does to what the brushes before it in its group made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum BrushOp {
    /// Adds itself.
    #[default]
    Add,
    /// Takes itself out.
    Cut,
    /// Keeps only what lies inside it.
    Intersect,
}

/// What a brush is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum BrushShape {
    /// A box.
    #[default]
    Box,
    /// A ramp rising towards +z.
    Wedge,
    /// Stairs rising towards +z, [`Brush::segments`] steps.
    Stairs,
    /// A prism around y, [`Brush::segments`] sides.
    Cylinder,
    /// A polyhedron of [`Brush::segments`] sides around y and half as many rings.
    Sphere,
    /// A cone of [`Brush::segments`] sides around y, its top [`Brush::top`] of its base (0:
    /// pointed).
    Cone,
    /// A ring lying on its base of [`Brush::segments`] convex parts, its tube of half as many
    /// sides: the size's x and z its outer diameters, its y the tube's.
    Torus,
    /// The planes in [`Brush::planes`] (a brush whose faces were edited).
    Planes,
}

/// A plane of an edited brush: `normal · x = offset` in millimetres of the brush's own
/// space, the brush behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub struct BrushPlane {
    /// The convex part it bounds.
    pub part: u32,
    /// Its normal, whole numbers.
    pub normal: [i32; 3],
    /// Its offset.
    pub offset: i64,
}

/// A blockout brush (ADR 0110): convex parts that add to, cut out of or intersect what the
/// brushes before it in its [`BrushGroup`] made. Its origin is its base's middle; its
/// placement is its entity's transform.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct Brush {
    /// What it does.
    pub op: BrushOp,
    /// What it is.
    pub shape: BrushShape,
    /// Its size along x, y (its height) and z, metres.
    pub size: Vec3,
    /// Stairs' steps; the sides of cylinders, cones, spheres and tori.
    pub segments: u32,
    /// A cone's top, as a share of its base: 0 a point, between 0 and 1 truncated.
    #[reflect(min = 0.0, max = 1.0)]
    pub top: f32,
    /// A room's walls, metres thick; 0 a solid brush. A room's size is its inside, its
    /// walls stand around it, and rooms open into each other where one's inside overlaps
    /// another's walls, whatever their order (team decision, 2026-10-21). Only an adding
    /// brush is a room.
    #[reflect(min = 0.0)]
    pub hollow: f32,
    /// Its planes when [`BrushShape::Planes`].
    pub planes: Vec<BrushPlane>,
    /// Physics collides with it (made with the colliders, a later step).
    pub collides: bool,
    /// Its color, linear.
    #[reflect(hint = "color")]
    pub tint: [f32; 4],
}
engine_ecs::reflect_component!(Brush);

impl Default for Brush {
    /// A 2 m cube adding itself, light grey.
    fn default() -> Self {
        Self::new(BrushShape::Box, Vec3::splat(2.0))
    }
}

impl Brush {
    /// An adding brush of `shape` and `size` (metres): stairs of 8 steps, cylinders and
    /// cones of 16 sides, spheres of 12, tori of 24; cones pointed.
    pub fn new(shape: BrushShape, size: Vec3) -> Self {
        Self {
            op: BrushOp::Add,
            shape,
            size,
            segments: match shape {
                BrushShape::Stairs => 8,
                BrushShape::Cylinder | BrushShape::Cone => 16,
                BrushShape::Sphere => 12,
                BrushShape::Torus => 24,
                _ => 0,
            },
            top: 0.0,
            hollow: 0.0,
            planes: Vec::new(),
            collides: true,
            tint: [0.72, 0.72, 0.72, 1.0],
        }
    }

    /// The same doing `op`.
    pub fn with_op(self, op: BrushOp) -> Self {
        Self { op, ..self }
    }

    /// The same with its top `top` of its base (a cone: truncated).
    pub fn with_top(self, top: f32) -> Self {
        Self { top, ..self }
    }

    /// The same as a room with walls `hollow` metres thick (0: solid).
    pub fn with_hollow(self, hollow: f32) -> Self {
        Self { hollow, ..self }
    }

    /// Whether it is a room: adding, with walls.
    pub fn is_room(&self) -> bool {
        self.walls().is_some()
    }

    /// Its convex parts' planes in its own grid (millimetres): a room's walls, its shape's
    /// planes moved out by their thickness; none when its size is not positive or out of
    /// bounds.
    pub fn parts(&self) -> Vec<Vec<Plane>> {
        let parts = self.shape_parts();
        match self.walls() {
            Some(by) => engine_brush::shape::grown(&parts, by).unwrap_or_default(),
            None => parts,
        }
    }

    /// A room's inside in its own grid: its shape at its size; none for a solid brush.
    pub fn inside(&self) -> Vec<Vec<Plane>> {
        if self.is_room() {
            self.shape_parts()
        } else {
            Vec::new()
        }
    }

    /// A room's walls' thickness in its grid.
    fn walls(&self) -> Option<i64> {
        let by = (f64::from(self.hollow) / UNIT).round() as i64;
        (by > 0 && self.op == BrushOp::Add).then_some(by)
    }

    /// Its shape's convex parts at its size, in its own grid.
    fn shape_parts(&self) -> Vec<Vec<Plane>> {
        let size =
            [self.size.x, self.size.y, self.size.z].map(|s| (f64::from(s) / UNIT).round() as i64);
        let shape = match self.shape {
            BrushShape::Box => Shape::Box,
            BrushShape::Wedge => Shape::Wedge,
            BrushShape::Stairs => Shape::Stairs(self.segments),
            BrushShape::Cylinder => Shape::Cylinder(self.segments),
            BrushShape::Sphere => Shape::Sphere(self.segments),
            BrushShape::Cone => Shape::Cone(
                self.segments,
                (self.top.clamp(0.0, 1.0) * 1000.0).round() as u32,
            ),
            BrushShape::Torus => Shape::Torus(self.segments, (self.segments / 2).max(4)),
            BrushShape::Planes => {
                let count = self.planes.iter().map(|p| p.part + 1).max().unwrap_or(0);
                return (0..count)
                    .map(|k| {
                        self.planes
                            .iter()
                            .filter(|p| p.part == k)
                            .filter_map(|p| Plane::new(p.normal.map(i64::from), p.offset))
                            .collect()
                    })
                    .collect();
            }
        };
        engine_brush::shape::parts(shape, size).unwrap_or_default()
    }
}

/// One brush's share of a group's surface, for baking it (ADR 0110 E2b-6): its triangles
/// in the group's space, metres, with the grid's texture coordinates (metres too) and the
/// brush's tint.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BakedBrush {
    /// Its tint, linear.
    pub tint: [f32; 4],
    /// Corners.
    pub positions: Vec<[f32; 3]>,
    /// Unit normals, one per corner.
    pub normals: Vec<[f32; 3]>,
    /// Texture coordinates: metres along the face's projection, as the grid is drawn.
    pub uvs: Vec<[f32; 2]>,
    /// Corners of the triangles, counter-clockwise from in front.
    pub indices: Vec<u32>,
}

/// The blockout's grid as a PNG: a metre's cell 256 px wide, dark lines on its edges and
/// lighter ones every 25 cm (sRGB). What a baked group's materials show.
pub fn grid_png() -> Vec<u8> {
    let t = grid_texture();
    engine_render::png::encode_png(
        t.width,
        t.height,
        engine_render::png::PngFormat::Rgba,
        &t.levels[0],
    )
    .unwrap_or_default()
}

/// A group of brushes (ADR 0110, Godot's `CSGCombiner3D`): its descendant brushes act on
/// each other in the tree's order; brushes of other groups do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub struct BrushGroup {
    /// The glTF file the group was baked into (the editor's Blockout: Bake to Mesh, ADR
    /// 0110 E2b-6), `none` while it is live: models of the file's parts draw it, and its
    /// brushes, not drawn, still collide and still make its navigation.
    pub baked: AssetRef,
}

impl BrushGroup {
    /// Whether the group was baked into a file.
    pub fn is_baked(&self) -> bool {
        !self.baked.is_none()
    }
}
engine_ecs::reflect_component!(BrushGroup);

/// Marks the entities whose `MeshRenderer` the blockout made, so it goes with the brush.
#[derive(Debug, Clone, Copy, Default)]
struct FromBrush;
engine_ecs::component!(FromBrush);

/// What made a brush's planes in its group: its component and its placement there.
#[derive(Debug, Clone, PartialEq)]
struct Input {
    brush: Brush,
    linear: [f32; 9],
    offset: [f32; 3],
}

#[derive(Debug, Default)]
struct GroupState {
    group: Group,
    entities: Vec<Entity>,
    inputs: Vec<Input>,
    /// For each brush, the place of the brush it is below, nearest: its host.
    hosts: Vec<Option<usize>>,
    /// Each brush as its own planes make it, before its children act on it, and what a
    /// host's children made of it.
    own: Vec<engine_brush::Brush>,
    made: Vec<Option<Vec<engine_brush::Solid>>>,
    /// Changes whenever its pieces may have: what colliders made from it follow.
    epoch: u64,
    /// Baked: its brushes' meshes are not drawn.
    baked: bool,
}

type Brushes = QueryState<Entity, With<Brush>>;
type ChangedBrushes = QueryState<Entity, Changed<Brush>>;
type ChangedGroups = QueryState<Entity, Changed<BrushGroup>>;
type Moved = QueryState<Entity, (With<Brush>, Changed<Transform>)>;
type MovedParents = QueryState<Entity, (With<engine_ecs::Children>, Changed<Transform>)>;
type Reparented = QueryState<Entity, Changed<Parent>>;
type Orphans = QueryState<Entity, (With<FromBrush>, Without<Brush>)>;

/// The player's blockout: every group evaluated, every brush's mesh.
#[derive(Default)]
pub struct Blockout {
    groups: HashMap<Entity, GroupState>,
    /// Each brush's group and place in it.
    places: HashMap<Entity, (Entity, usize)>,
    meshes: HashMap<Entity, MeshId>,
    /// Meshes made again or dropped: the scene's instances draw them until it is next
    /// extracted (a tick), so they are freed in the first [`Self::sync`] after that.
    retired: Vec<MeshId>,
    materials: HashMap<[u32; 4], MaterialId>,
    grid: Option<TextureId>,
    /// The grid could not be made: not tried again, the brushes stay plain.
    grid_failed: bool,
    count: usize,
    /// The world's `ActivityEpoch` when last looked at: brushes turned off or on.
    activity: u64,
    queries: Option<Queries>,
    triangles: Triangles,
    polygons: Vec<(engine_brush::Owner, engine_brush::Polygon)>,
    /// The groups' epochs so far.
    epochs: u64,
}

struct Queries {
    brushes: Brushes,
    changed: ChangedBrushes,
    groups: ChangedGroups,
    moved: Moved,
    moved_parents: MovedParents,
    reparented: Reparented,
    orphans: Orphans,
}

impl Blockout {
    /// No brushes yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many brushes have a mesh.
    pub fn meshes(&self) -> usize {
        self.meshes.len()
    }

    /// The brush groups evaluated, each with a number that changes whenever its pieces may
    /// have (a brush in no group is a group of its own, by its entity): what colliders
    /// follow (ADR 0110).
    pub fn groups(&self) -> impl Iterator<Item = (Entity, u64)> + '_ {
        self.groups.iter().map(|(&e, s)| (e, s.epoch))
    }

    /// Group `group`'s colliders into `out` (cleared first): the pieces of its brushes that
    /// collide (`Brush::collides`), each the points of a convex hull in the group's space,
    /// metres.
    pub fn hulls(&self, group: Entity, out: &mut Vec<Vec<Vec3>>) {
        out.clear();
        let Some(state) = self.groups.get(&group) else {
            return;
        };
        for (b, solid) in state.group.pieces() {
            if !state
                .inputs
                .get(b as usize)
                .is_some_and(|i| i.brush.collides)
            {
                continue;
            }
            let mut points: Vec<Vec3> = solid
                .corners()
                .map(|p| (p.to_f64() * UNIT).as_vec3())
                .collect();
            points.sort_unstable_by(|a, b| {
                a.to_array()
                    .partial_cmp(&b.to_array())
                    .unwrap_or(core::cmp::Ordering::Equal)
            });
            points.dedup();
            out.push(points);
        }
    }

    /// Group `group`'s visible surface for baking (ADR 0110 E2b-6) into `out` (cleared
    /// first): a part per brush that shows any of it (a cut's the walls of its holes), in
    /// the group's space, as the player draws it.
    pub fn baked(&self, group: Entity, out: &mut Vec<BakedBrush>) {
        out.clear();
        let Some(state) = self.groups.get(&group) else {
            return;
        };
        let mut polygons = Vec::new();
        let mut t = Triangles::default();
        for (i, input) in state.inputs.iter().enumerate() {
            state.group.polygons_of(i, &mut polygons);
            t.clear();
            triangles(
                polygons.iter().map(|(_, p)| p),
                state.group.planes(),
                GROW,
                UNIT,
                &mut t,
            );
            if t.indices.is_empty() {
                continue;
            }
            out.push(BakedBrush {
                tint: input.brush.tint,
                positions: t
                    .positions
                    .iter()
                    .map(|p| (*p * UNIT).as_vec3().to_array())
                    .collect(),
                normals: t.normals.iter().map(|n| n.as_vec3().to_array()).collect(),
                uvs: t
                    .uvs
                    .iter()
                    .map(|uv| [uv[0] as f32, uv[1] as f32])
                    .collect(),
                indices: t.indices.clone(),
            });
        }
    }

    /// The visible surface of every group's colliding brushes in the world (`world` places
    /// the groups), as triangles counter-clockwise seen from outside, into `vertices` and
    /// `triangles` (cleared first): what a navigation mesh is baked from (ADR 0110). The
    /// pieces' insides stay out: Recast would find walkable islands in them.
    pub fn surface_triangles(
        &self,
        world: &World,
        vertices: &mut Vec<Vec3>,
        triangles: &mut Vec<[u32; 3]>,
    ) {
        vertices.clear();
        triangles.clear();
        for (&key, state) in &self.groups {
            let placed = world
                .get::<GlobalTransform>(key)
                .map_or(Affine3A::IDENTITY, |g| g.0);
            state.group.for_each_polygon(|owner, polygon| {
                if !state
                    .inputs
                    .get(owner.brush as usize)
                    .is_some_and(|i| i.brush.collides)
                {
                    return;
                }
                let first = vertices.len() as u32;
                vertices.extend(
                    polygon
                        .corners
                        .iter()
                        .map(|c| placed.transform_point3((c.to_f64() * UNIT).as_vec3())),
                );
                for i in 1..polygon.corners.len().saturating_sub(1) as u32 {
                    triangles.push([first, first + i, first + i + 1]);
                }
            });
        }
    }

    /// The edges of every group's colliding pieces in the world (`world` places the groups)
    /// into `out` (cleared first), each once: what the editor's overlay shows of the
    /// colliders.
    pub fn collider_edges(&self, world: &World, out: &mut Vec<[Vec3; 2]>) {
        out.clear();
        // A face's neighbour lists its edge again, backwards; equal corners land on equal
        // floats.
        let mut seen = HashSet::new();
        let bits = |p: Vec3| p.to_array().map(f32::to_bits);
        for (&key, state) in &self.groups {
            let placed = world
                .get::<GlobalTransform>(key)
                .map_or(Affine3A::IDENTITY, |g| g.0);
            let at =
                |p: &engine_brush::Point| placed.transform_point3((p.to_f64() * UNIT).as_vec3());
            for (b, solid) in state.group.pieces() {
                if !state
                    .inputs
                    .get(b as usize)
                    .is_some_and(|i| i.brush.collides)
                {
                    continue;
                }
                for face in &solid.faces {
                    let c = &face.polygon.corners;
                    for i in 0..c.len() {
                        let (a, b) = (at(&c[i]), at(&c[(i + 1) % c.len()]));
                        let (x, y) = (bits(a), bits(b));
                        if seen.insert(if x < y { (x, y) } else { (y, x) }) {
                            out.push([a, b]);
                        }
                    }
                }
            }
        }
    }

    /// Brings the brushes' meshes in step with the world: run where models turn into
    /// renderers, after the frame's ticks; the next tick's extraction gives the scene the
    /// new meshes. `frame` is the frame's number; `extracted`: the scene was extracted this
    /// frame, so the meshes let go of before are drawn no more and are freed (after this
    /// frame).
    pub fn sync(
        &mut self,
        world: &mut World,
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
        frame: u64,
        extracted: bool,
    ) {
        engine_profiler::zone!("blockout::sync");
        if extracted {
            for mesh in self.retired.drain(..) {
                geometry.remove_mesh(mesh, frame);
            }
        }
        let q = self.queries.get_or_insert_with(|| Queries {
            brushes: QueryState::new(world),
            changed: QueryState::new(world),
            groups: QueryState::new(world),
            moved: QueryState::new(world),
            moved_parents: QueryState::new(world),
            reparented: QueryState::new(world),
            orphans: QueryState::new(world),
        });
        let count = q.brushes.iter_readonly(world).count();
        // No brushes, and none before: nothing to look at (a scene without brushes pays
        // for this count only). The other queries report what they missed when brushes
        // come: everything, so the first scan takes it all.
        if count == 0 && self.count == 0 {
            return;
        }
        // Every query runs, so that each forgets what it reported.
        let changed = [
            q.changed.iter_readonly(world).count(),
            q.groups.iter_readonly(world).count(),
            q.moved.iter_readonly(world).count(),
            q.moved_parents.iter_readonly(world).count(),
            q.reparented.iter_readonly(world).count(),
        ];
        let orphans: Vec<Entity> = q.orphans.iter_readonly(world).collect();
        for e in orphans {
            world.remove::<FromBrush>(e);
            world.remove::<MeshRenderer>(e);
        }
        let activity = world
            .resource::<engine_runtime::ActivityEpoch>()
            .map_or(0, |a| a.0);
        if count == self.count && changed.iter().all(|&c| c == 0) && activity == self.activity {
            return;
        }
        self.count = count;
        self.activity = activity;
        // A brush that is off (disabled, or below a disabled entity) acts on nothing.
        let all: Vec<Entity> = q
            .brushes
            .iter_readonly(world)
            .filter(|&e| !engine_runtime::is_inactive(world, e))
            .collect();
        self.scan(world, &all, geometry, materials);
    }

    /// Makes the grid texture, once a brush is drawn, and puts it on the brushes'
    /// materials. Run where the device and the texture pool are at hand.
    pub fn make_grid(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        textures: &mut TexturePool,
        materials: &mut MaterialTable,
    ) {
        if self.grid.is_some() || self.grid_failed || self.materials.is_empty() {
            return;
        }
        match textures.add(device, allocator, grid_texture()) {
            Ok(id) => {
                self.grid = Some(id);
                for (&tint, &m) in &self.materials {
                    materials.set(m, material(tint, self.grid));
                }
            }
            Err(e) => {
                log::warn!(target: "engine_app::blockout", "the blockout grid texture: {e:?}");
                self.grid_failed = true;
            }
        }
    }

    fn scan(
        &mut self,
        world: &mut World,
        all: &[Entity],
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
    ) {
        engine_profiler::zone!("blockout::scan");
        // The groups and their brushes in the tree's order.
        let mut keys: Vec<Entity> = all.iter().map(|&e| group_of(world, e)).collect();
        keys.sort_unstable_by_key(|e| e.to_bits());
        keys.dedup();
        let mut seen = Vec::with_capacity(keys.len());
        let mut changed_entities: Vec<Entity> = Vec::new();
        for key in keys {
            let mut entities = Vec::new();
            if world.get::<BrushGroup>(key).is_some() {
                collect(world, key, &mut entities);
            } else {
                entities.push(key);
            }
            let inputs: Vec<Input> = entities.iter().map(|&e| input(world, key, e)).collect();
            let hosts = hosts_of(world, &entities);
            let state = self.groups.entry(key).or_default();
            let mut redone = state.entities != entities || state.hosts != hosts;
            if redone {
                // Brushes came, went or moved in the tree: the group from nothing.
                for e in &state.entities {
                    if !entities.contains(e) {
                        changed_entities.push(*e);
                        self.places.remove(e);
                    }
                }
                state.group = Group::new();
                let own: Vec<engine_brush::Brush> = inputs
                    .iter()
                    .map(|i| to_brush(i, state.group.planes_mut()))
                    .collect();
                let (brushes, made) = operands(&own, &hosts, None, state.group.planes());
                state.group.rebuild(brushes);
                for (i, &e) in entities.iter().enumerate() {
                    self.places.insert(e, (key, i));
                }
                state.entities = entities;
                state.inputs = inputs;
                state.hosts = hosts;
                state.own = own;
                state.made = made;
            } else {
                let mut touched = vec![false; inputs.len()];
                for (i, input) in inputs.into_iter().enumerate() {
                    if input != state.inputs[i] {
                        state.own[i] = to_brush(&input, state.group.planes_mut());
                        state.inputs[i] = input;
                        touched[i] = true;
                    }
                }
                if touched.contains(&true) {
                    let (brushes, made) = operands(
                        &state.own,
                        &state.hosts,
                        Some((&touched, &state.made)),
                        state.group.planes(),
                    );
                    state.made = made;
                    for (i, b) in brushes.into_iter().enumerate() {
                        if state.group.brushes()[i] != b {
                            state.group.set(i, b);
                            redone = true;
                        }
                    }
                }
            }
            if redone {
                self.epochs += 1;
                state.epoch = self.epochs;
            }
            for i in state.group.take_changed() {
                changed_entities.push(state.entities[i as usize]);
            }
            // Baked or live again: its brushes hidden or shown, their meshes kept.
            let baked = world
                .get::<BrushGroup>(key)
                .is_some_and(BrushGroup::is_baked);
            if state.baked != baked {
                state.baked = baked;
                for &e in &state.entities {
                    if world.get::<FromBrush>(e).is_some()
                        && let Some(r) = world.get_mut::<MeshRenderer>(e)
                    {
                        r.visible = u32::from(!baked);
                    }
                }
            }
            seen.push(key);
        }
        // Groups whose brushes all went.
        let gone: Vec<Entity> = self
            .groups
            .keys()
            .copied()
            .filter(|k| !seen.contains(k))
            .collect();
        for k in gone {
            if let Some(state) = self.groups.remove(&k) {
                for e in &state.entities {
                    self.places.remove(e);
                }
                changed_entities.extend(state.entities);
            }
        }
        changed_entities.sort_unstable_by_key(|e| e.to_bits());
        changed_entities.dedup();
        for e in changed_entities {
            self.remesh(world, e, geometry, materials);
        }
    }

    /// Makes brush `e`'s mesh again (or drops it, when it shows nothing or is gone); the
    /// old one is drawn until the scene's next extraction.
    fn remesh(
        &mut self,
        world: &mut World,
        e: Entity,
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
    ) {
        if let Some(old) = self.meshes.remove(&e) {
            self.retired.push(old);
        }
        let place = self
            .places
            .get(&e)
            .and_then(|&(key, i)| self.groups.get(&key).map(|s| (s, i)));
        let Some((state, index)) = place else {
            if world.remove::<FromBrush>(e).is_some() {
                world.remove::<MeshRenderer>(e);
            }
            return;
        };
        state.group.polygons_of(index, &mut self.polygons);
        self.triangles.clear();
        triangles(
            self.polygons.iter().map(|(_, p)| p),
            state.group.planes(),
            GROW,
            UNIT,
            &mut self.triangles,
        );
        let input = &state.inputs[index];
        let tint = input.brush.tint;
        let visible = u32::from(!state.baked);
        if self.triangles.indices.is_empty() {
            if world.remove::<FromBrush>(e).is_some() {
                world.remove::<MeshRenderer>(e);
            }
            return;
        }
        // From the group's grid to the brush's own space, in metres.
        let linear = Mat3::from_cols_array(&input.linear).as_dmat3();
        let offset = Vec3::from_array(input.offset).as_dvec3();
        let inverse = linear.inverse();
        let t = &self.triangles;
        let positions: Vec<[f32; 3]> = t
            .positions
            .iter()
            .map(|p| (inverse * (*p - offset) * UNIT).as_vec3().to_array())
            .collect();
        let normals: Vec<[f32; 3]> = t
            .normals
            .iter()
            .map(|n| {
                (linear.transpose() * *n)
                    .normalize_or_zero()
                    .as_vec3()
                    .to_array()
            })
            .collect();
        let uvs: Vec<[f32; 2]> = t
            .uvs
            .iter()
            .map(|uv| [uv[0] as f32, uv[1] as f32])
            .collect();
        let mut indices = t.indices.clone();
        if linear.determinant() < 0.0 {
            // Mirrored: the triangles turn the other way in the brush's space.
            for tri in indices.chunks_mut(3) {
                tri.swap(1, 2);
            }
        }
        let data = MeshData {
            positions: &positions,
            normals: &normals,
            tangents: &[],
            uvs: &uvs,
            indices: &indices,
            meshlets: &[],
        };
        let mesh = match geometry.add_mesh(&data) {
            Ok(m) => m,
            Err(err) => {
                log::warn!(target: "engine_app::blockout", "a brush's mesh: {err:?}");
                return;
            }
        };
        self.meshes.insert(e, mesh);
        let key = tint.map(f32::to_bits);
        let grid = self.grid;
        let material = *self
            .materials
            .entry(key)
            .or_insert_with(|| materials.add(material(key, grid)));
        world.insert(
            e,
            MeshRenderer {
                mesh,
                material,
                visible,
            },
        );
        if world.get::<FromBrush>(e).is_none() {
            world.insert(e, FromBrush);
            if world.get::<GlobalTransform>(e).is_none() {
                world.insert(e, GlobalTransform::IDENTITY);
            }
        }
    }
}

/// The group brush `e` belongs to: its nearest ancestor with a [`BrushGroup`], or itself.
fn group_of(world: &World, e: Entity) -> Entity {
    let mut at = world.parent(e);
    while let Some(p) = at {
        if world.get::<BrushGroup>(p).is_some() {
            return p;
        }
        at = world.parent(p);
    }
    e
}

/// The brushes below `e` in the tree's order, not those of groups within it.
fn collect(world: &World, e: Entity, out: &mut Vec<Entity>) {
    for child in world.children(e) {
        if world.get::<BrushGroup>(child).is_some() {
            continue;
        }
        if world.get::<Brush>(child).is_some() && !engine_runtime::is_inactive(world, child) {
            out.push(child);
        }
        collect(world, child, out);
    }
}

/// For each of a group's brushes `entities` (in the tree's order), the place among them of
/// the brush it is below, nearest: its children act on that one first.
fn hosts_of(world: &World, entities: &[Entity]) -> Vec<Option<usize>> {
    let place: HashMap<Entity, usize> = entities.iter().enumerate().map(|(i, &e)| (e, i)).collect();
    entities
        .iter()
        .map(|&e| {
            let mut at = world.parent(e);
            while let Some(p) = at {
                if world.get::<BrushGroup>(p).is_some() {
                    return None;
                }
                if let Some(&k) = place.get(&p) {
                    return Some(k);
                }
                at = world.parent(p);
            }
            None
        })
        .collect()
}

/// What each host's children made of it, as [`operands`] keeps it.
type Made = Vec<Option<Vec<engine_brush::Solid>>>;

/// A group's brushes as its evaluation takes them (ADR 0110's nesting), and what each
/// host's children made of it: a brush with child brushes is what they make of it
/// ([`engine_brush::combine`], its op kept), each child, combined into it, an empty brush
/// in its own place. `own` are the brushes' own; with `reuse` (the brushes touched, what the
/// hosts were made of before), a host above none of them keeps what it was made of.
fn operands(
    own: &[engine_brush::Brush],
    hosts: &[Option<usize>],
    reuse: Option<(&[bool], &Made)>,
    planes: &engine_brush::Planes,
) -> (Vec<engine_brush::Brush>, Made) {
    let n = own.len();
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, &h) in hosts.iter().enumerate() {
        if let Some(h) = h {
            kids[h].push(i);
        }
    }
    // What changed, up to the hosts it is below.
    let mut stale = vec![reuse.is_none(); n];
    if let Some((touched, _)) = reuse {
        for (i, &t) in touched.iter().enumerate() {
            let mut at = t.then_some(i);
            while let Some(k) = at {
                stale[k] = true;
                at = hosts[k];
            }
        }
    }
    let mut made: Made = vec![None; n];
    // A host's children come after it in the tree's order: from the last, each host's
    // children are made before it.
    for i in (0..n).rev() {
        if kids[i].is_empty() {
            continue;
        }
        if !stale[i]
            && let Some((_, before)) = reuse
        {
            made[i].clone_from(&before[i]);
            continue;
        }
        let mut level = Vec::with_capacity(kids[i].len() + 1);
        let mut owners = Vec::with_capacity(kids[i].len() + 1);
        level.push(own[i].clone());
        owners.push(i as u32);
        for &k in &kids[i] {
            level.push(engine_brush::Brush {
                made: made[k].clone(),
                ..own[k].clone()
            });
            owners.push(k as u32);
        }
        made[i] = Some(engine_brush::combine(&level, &owners, planes).0);
    }
    let operands = (0..n)
        .map(|i| match hosts[i] {
            Some(_) => engine_brush::Brush::default(),
            None => engine_brush::Brush {
                made: made[i].clone(),
                ..own[i].clone()
            },
        })
        .collect();
    (operands, made)
}

/// What makes brush `e`'s planes in group `group`: its component, and its transforms up to
/// the group, in millimetres.
fn input(world: &World, group: Entity, e: Entity) -> Input {
    let local = |x: Entity| {
        world
            .get::<Transform>(x)
            .map_or(engine_core::math::Affine3A::IDENTITY, Transform::to_affine)
    };
    let mut m = local(e);
    let mut at = world.parent(e);
    while let Some(p) = at {
        if p == group || group == e {
            break;
        }
        m = local(p) * m;
        at = world.parent(p);
    }
    if group == e {
        m = engine_core::math::Affine3A::IDENTITY;
    }
    Input {
        brush: world.get::<Brush>(e).cloned().unwrap_or_default(),
        linear: Mat3::from(m.matrix3).to_cols_array(),
        offset: (Vec3::from(m.translation) / UNIT as f32).to_array(),
    }
}

/// The brush as its group's evaluation takes it: its parts' planes in the group's grid.
fn to_brush(input: &Input, planes: &mut engine_brush::Planes) -> engine_brush::Brush {
    let linear: DMat3 = Mat3::from_cols_array(&input.linear).as_dmat3();
    let offset: DVec3 = Vec3::from_array(input.offset).as_dvec3();
    let mut placed = |parts: Vec<Vec<Plane>>| -> Vec<Vec<engine_brush::PlaneId>> {
        parts
            .into_iter()
            .filter_map(|part| {
                part.iter()
                    .map(|p| p.transformed(linear, offset).map(|q| planes.add(q)))
                    .collect::<Option<Vec<_>>>()
            })
            .collect()
    };
    let parts = placed(input.brush.parts());
    let inside = placed(input.brush.inside());
    engine_brush::Brush {
        op: match input.brush.op {
            BrushOp::Add => engine_brush::Op::Add,
            BrushOp::Cut => engine_brush::Op::Cut,
            BrushOp::Intersect => engine_brush::Op::Intersect,
        },
        parts,
        inside,
        made: None,
    }
}

/// A brush's material: its tint over the grid.
fn material(tint: [u32; 4], grid: Option<TextureId>) -> MaterialDesc {
    MaterialDesc {
        base_color: tint.map(f32::from_bits),
        base_color_texture: grid.map(TextureRef::new),
        roughness: 0.85,
        metallic: 0.0,
        ..MaterialDesc::default()
    }
}

/// The grid: a metre's cell, 256 texels wide, dark lines on its edges and lighter ones
/// every 25 cm, with its smaller levels.
fn grid_texture() -> TextureLevels {
    const SIZE: usize = 256;
    let srgb = |linear: f32| (linear.powf(1.0 / 2.2) * 255.0).round() as u8;
    let mut level: Vec<f32> = vec![0.0; SIZE * SIZE];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let line = |v: usize, every: usize, width: usize| {
                let m = v % every;
                m < width / 2 + width % 2 || m >= every - width / 2
            };
            let shade = if line(x, SIZE, 4) || line(y, SIZE, 4) {
                0.18
            } else if line(x, SIZE / 4, 2) || line(y, SIZE / 4, 2) {
                0.42
            } else {
                0.62
            };
            level[y * SIZE + x] = shade;
        }
    }
    let mut levels = Vec::new();
    let mut size = SIZE;
    loop {
        levels.push(
            level
                .iter()
                .flat_map(|&v| {
                    let c = srgb(v);
                    [c, c, c, 255]
                })
                .collect::<Vec<u8>>(),
        );
        if size == 1 {
            break;
        }
        let half = size / 2;
        let mut next = vec![0.0; half * half];
        for y in 0..half {
            for x in 0..half {
                let at = |dx: usize, dy: usize| level[(2 * y + dy) * size + 2 * x + dx];
                next[y * half + x] = (at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1)) / 4.0;
            }
        }
        level = next;
        size = half;
    }
    TextureLevels {
        width: SIZE as u32,
        height: SIZE as u32,
        format: TextureFormat::Rgba8Srgb,
        levels,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_hollow_brush_is_walls_around_its_size_and_a_cut_never_is() {
        let size = Vec3::new(4.0, 3.0, 4.0);
        let room = Brush::new(BrushShape::Box, size).with_hollow(0.2);
        assert!(room.is_room());
        assert_eq!(room.inside(), Brush::new(BrushShape::Box, size).parts());
        assert_eq!(
            room.parts(),
            engine_brush::shape::grown(&room.inside(), 200).unwrap()
        );
        let cut = room.clone().with_op(BrushOp::Cut);
        assert!(!cut.is_room() && cut.inside().is_empty());
        assert_eq!(cut.parts(), room.inside(), "a cut keeps its size");
        // In its group's grid, moved 3 m: a room there too.
        let mut planes = engine_brush::Planes::new();
        let input = Input {
            brush: room,
            linear: Mat3::IDENTITY.to_cols_array(),
            offset: [3000.0, 0.0, 0.0],
        };
        let b = to_brush(&input, &mut planes);
        assert!(b.is_room());
        assert_eq!((b.parts.len(), b.inside.len()), (1, 1));
    }

    /// The volume the group's surface encloses, in cubic grid units.
    fn volume(g: &Group) -> i64 {
        let mut v = 0.0;
        g.for_each_polygon(|_, p| {
            let c: Vec<engine_core::math::DVec3> = p.corners.iter().map(|c| c.to_f64()).collect();
            for i in 1..c.len() - 1 {
                v += c[0].dot(c[i].cross(c[i + 1])) / 6.0;
            }
        });
        v.round() as i64
    }

    #[test]
    fn collider_edges_list_each_edge_once() {
        let mut world = World::new();
        let group = world.spawn((Transform::IDENTITY, BrushGroup::default()));
        let b = world.spawn((
            Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
            Brush::new(BrushShape::Box, Vec3::new(2.0, 1.0, 3.0)),
        ));
        world.set_parent(b, Some(group)).unwrap();
        let mut entities = Vec::new();
        collect(&world, group, &mut entities);
        let inputs: Vec<Input> = entities.iter().map(|&e| input(&world, group, e)).collect();
        let hosts = hosts_of(&world, &entities);
        let mut g = Group::new();
        let own: Vec<engine_brush::Brush> =
            inputs.iter().map(|i| to_brush(i, g.planes_mut())).collect();
        let (brushes, made) = operands(&own, &hosts, None, g.planes());
        g.rebuild(brushes);
        let mut blockout = Blockout::default();
        blockout.groups.insert(
            group,
            GroupState {
                group: g,
                entities,
                inputs,
                hosts,
                own,
                made,
                epoch: 0,
                baked: false,
            },
        );
        let mut edges = Vec::new();
        blockout.collider_edges(&world, &mut edges);
        // A box's twelve, not each twice (once per face beside it).
        assert_eq!(edges.len(), 12, "{edges:?}");
        let length: f32 = edges.iter().map(|[a, b]| a.distance(*b)).sum();
        assert!((length - 4.0 * (2.0 + 1.0 + 3.0)).abs() < 1e-4, "{length}");
    }

    #[test]
    fn a_window_below_a_wall_cuts_that_wall_and_not_the_floor() {
        let mut world = World::new();
        let group = world.spawn((Transform::IDENTITY, BrushGroup::default()));
        let brush = |world: &mut World, parent: Entity, at: Vec3, b: Brush| {
            let e = world.spawn((Transform::from_translation(at), b));
            world.set_parent(e, Some(parent)).unwrap();
            e
        };
        let box_of = |x: f32, y: f32, z: f32| Brush::new(BrushShape::Box, Vec3::new(x, y, z));
        // Boxes stand on their base, centred across. A floor 10 × 1 × 10 m; a wall
        // 2 × 4 × 1 m on it; a window 1 × 2 × 1.2 m below the wall, from 0.5 m down in the
        // floor to 1.5 m up the wall.
        let _floor = brush(
            &mut world,
            group,
            Vec3::new(0.0, -1.0, 0.0),
            box_of(10.0, 1.0, 10.0),
        );
        let wall = brush(&mut world, group, Vec3::ZERO, box_of(2.0, 4.0, 1.0));
        let window = brush(
            &mut world,
            wall,
            Vec3::new(0.0, -0.5, 0.0),
            box_of(1.0, 2.0, 1.2).with_op(BrushOp::Cut),
        );
        let evaluate = |world: &World| {
            let mut entities = Vec::new();
            collect(world, group, &mut entities);
            let inputs: Vec<Input> = entities.iter().map(|&e| input(world, group, e)).collect();
            let hosts = hosts_of(world, &entities);
            let mut g = Group::new();
            let own: Vec<engine_brush::Brush> =
                inputs.iter().map(|i| to_brush(i, g.planes_mut())).collect();
            let (brushes, made) = operands(&own, &hosts, None, g.planes());
            g.rebuild(brushes);
            (g, hosts, own, made)
        };
        let m3 = |v: f64| (v * 1e9).round() as i64;
        let (g, hosts, own, made) = evaluate(&world);
        assert_eq!(hosts, [None, None, Some(1)]);
        // The floor whole; the wall less the window's 1 × 1.5 × 1 m inside it.
        assert_eq!(volume(&g), m3(100.0 + 8.0 - 1.5));
        // Unchanged brushes reuse what their hosts were made of.
        let touched = [true, false, false];
        let (again, _) = operands(&own, &hosts, Some((&touched, &made)), g.planes());
        assert_eq!(again[1].made, made[1]);
        // The window a sibling of the wall, where it is in the world: it cuts the floor too
        // (1 × 0.5 × 1.2 m more).
        world.set_parent(window, Some(group)).unwrap();
        world.get_mut::<Transform>(window).unwrap().translation = Vec3::new(0.0, -0.5, 0.0);
        let (g, hosts, _, _) = evaluate(&world);
        assert_eq!(hosts, [None, None, None]);
        assert_eq!(volume(&g), m3(100.0 + 8.0 - 1.5 - 0.6));
    }
}
