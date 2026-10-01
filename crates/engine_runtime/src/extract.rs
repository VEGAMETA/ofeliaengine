//! Extraction: the renderable entities of the ECS world, mirrored into a GPU scene.

use engine_core::math::{Affine3A, Mat4};
use engine_ecs::{
    Changed, Entity, GlobalTransform, Or, QueryState, World, bytemuck, pod_component,
};
use engine_render::gpu_scene::{GpuScene, InstanceId, SceneInstance};
use engine_render::material::MaterialId;
use engine_render::mesh::MeshId;

/// Draws the entity's mesh with a material at its [`GlobalTransform`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct MeshRenderer {
    /// The mesh in the geometry pool.
    pub mesh: MeshId,
    /// The material in the material table.
    pub material: MaterialId,
    /// Nonzero: drawn. Zero: hidden (the entity keeps its other components).
    pub visible: u32,
}

pod_component!(MeshRenderer);

impl MeshRenderer {
    /// A visible renderer.
    pub fn new(mesh: MeshId, material: MaterialId) -> Self {
        Self {
            mesh,
            material,
            visible: 1,
        }
    }

    /// Whether it is drawn.
    pub fn is_visible(&self) -> bool {
        self.visible != 0
    }
}

/// Where extracted instances go: a [`GpuScene`], or a stand-in in tests.
pub trait SceneSink {
    /// Handle of an instance.
    type Id: Copy + Eq;
    /// Adds an instance.
    fn insert(&mut self, instance: SceneInstance) -> Self::Id;
    /// Removes one.
    fn remove(&mut self, id: Self::Id) -> bool;
    /// Moves one (a transform equal to the current one should cost nothing).
    fn set_transform(&mut self, id: Self::Id, world_from_object: &Mat4) -> bool;
    /// Moves one to an ECS world transform (the same as [`Self::set_transform`] with its
    /// matrix; implementations may skip the conversion).
    fn set_affine(&mut self, id: Self::Id, world_from_object: &Affine3A) -> bool {
        self.set_transform(id, &Mat4::from(*world_from_object))
    }
    /// Changes its mesh (unchanged: nothing).
    fn set_mesh(&mut self, id: Self::Id, mesh: MeshId);
    /// Changes its material (unchanged: nothing).
    fn set_material(&mut self, id: Self::Id, material: MaterialId);
}

impl SceneSink for GpuScene {
    type Id = InstanceId;
    fn insert(&mut self, instance: SceneInstance) -> InstanceId {
        GpuScene::insert(self, instance)
    }
    fn remove(&mut self, id: InstanceId) -> bool {
        GpuScene::remove(self, id)
    }
    fn set_transform(&mut self, id: InstanceId, world_from_object: &Mat4) -> bool {
        GpuScene::set_transform(self, id, world_from_object)
    }
    fn set_affine(&mut self, id: InstanceId, world_from_object: &Affine3A) -> bool {
        GpuScene::set_affine(self, id, world_from_object)
    }
    fn set_mesh(&mut self, id: InstanceId, mesh: MeshId) {
        GpuScene::set_mesh(self, id, mesh);
    }
    fn set_material(&mut self, id: InstanceId, material: MaterialId) {
        GpuScene::set_material(self, id, material);
    }
}

/// What the last [`RenderExtract::sync`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtractStats {
    /// Entities visited: those in chunks where a world transform or a renderer changed.
    pub visited: u32,
    /// Instances added.
    pub inserted: u32,
    /// Instances removed (despawned, renderer or world transform removed, hidden).
    pub removed: u32,
    /// Instances whose world transform changed.
    pub moved: u32,
    /// Whether the world was mirrored from scratch (first sync, after a clear or a
    /// snapshot restore).
    pub rebuilt: bool,
}

/// An entity's instance.
#[derive(Debug, Clone, Copy)]
struct Entry<Id> {
    entity: Entity,
    id: Id,
    /// What the instance draws (compared here: most changes are moves).
    mesh: MeshId,
    material: MaterialId,
    /// Its world transform at the last tick.
    current: Affine3A,
    /// The tick it last moved in.
    moved_tick: u64,
}

type Changes = QueryState<
    (Entity, &'static GlobalTransform, &'static MeshRenderer),
    Or<(Changed<GlobalTransform>, Changed<MeshRenderer>)>,
>;

/// Mirrors the entities that have a [`MeshRenderer`] and a [`GlobalTransform`] into a
/// scene (ADR 0064).
///
/// [`Self::sync`] runs after every simulation tick (after transform propagation): it
/// adds, edits and removes instances. Its cost follows the changes: it visits the chunks
/// whose `GlobalTransform` or `MeshRenderer` changed (chunk granularity: unchanged
/// entities sharing a chunk with a changed one are compared and skipped), and takes
/// removals from the world's removal log ([`World::track_removals`]), so a static world
/// costs a scan of chunk versions. [`Self::interpolate`] runs once per render frame and
/// places the instances that moved in the last tick between their last two transforms,
/// so motion stays smooth whatever the tick rate.
///
/// Extraction only reads the world: the simulation stays deterministic, and its state
/// hash is unaffected.
pub struct RenderExtract<Id = InstanceId> {
    changes: Changes,
    /// By entity index.
    entries: Vec<Option<Entry<Id>>>,
    /// Instances that moved in the last tick, with their transform before it.
    moving: Vec<(Entity, Affine3A)>,
    moving_next: Vec<(Entity, Affine3A)>,
    removed: Vec<Entity>,
    epoch: Option<u64>,
    tick: u64,
    interpolation: bool,
    stats: ExtractStats,
}

impl<Id> core::fmt::Debug for RenderExtract<Id> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RenderExtract")
            .field("moving", &self.moving.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// Bytes equal: what `TransformPropagation` writes is either the same bytes or a change.
fn same(a: &Affine3A, b: &Affine3A) -> bool {
    bytemuck::bytes_of(a) == bytemuck::bytes_of(b)
}

/// Component-wise blend of two affine transforms. For the small motion of one tick this
/// is indistinguishable from interpolating rotation and scale separately (a 6° turn per
/// tick shrinks the blended basis by under 0.2 % halfway).
fn lerp(a: &Affine3A, b: &Affine3A, t: f32) -> Affine3A {
    let (a, b) = (a.to_cols_array(), b.to_cols_array());
    Affine3A::from_cols_array(&core::array::from_fn(|i| a[i] + (b[i] - a[i]) * t))
}

impl<Id: Copy + Eq> RenderExtract<Id> {
    /// State for `world`; starts the world's removal logs of [`MeshRenderer`] and
    /// [`GlobalTransform`]. With `interpolation`, moving instances are placed between
    /// ticks by [`Self::interpolate`]; without, they jump to each tick's transform.
    pub fn new(world: &mut World, interpolation: bool) -> Self {
        world.track_removals::<MeshRenderer>();
        world.track_removals::<GlobalTransform>();
        Self {
            changes: QueryState::new(world),
            entries: Vec::new(),
            moving: Vec::new(),
            moving_next: Vec::new(),
            removed: Vec::new(),
            epoch: None,
            tick: 0,
            interpolation,
            stats: ExtractStats::default(),
        }
    }

    /// What the last [`Self::sync`] did.
    pub fn stats(&self) -> ExtractStats {
        self.stats
    }

    /// The instance of `entity`, if it has one.
    pub fn instance(&self, entity: Entity) -> Option<Id> {
        self.entry(entity).map(|e| e.id)
    }

    fn entry(&self, entity: Entity) -> Option<&Entry<Id>> {
        self.entries
            .get(entity.index() as usize)?
            .as_ref()
            .filter(|e| e.entity == entity)
    }

    /// Mirrors the changes since the previous call into `scene`. Call it after each
    /// simulation tick, after transform propagation.
    pub fn sync<S: SceneSink<Id = Id>>(&mut self, world: &mut World, scene: &mut S) {
        engine_profiler::zone!("runtime::RenderExtract::sync");
        self.tick += 1;
        let mut stats = ExtractStats::default();
        if self.epoch != Some(world.resync_epoch()) {
            // First sync, or entities vanished without logs: start over.
            for entry in self.entries.iter_mut().filter_map(Option::take) {
                scene.remove(entry.id);
                stats.removed += 1;
            }
            self.moving.clear();
            self.changes = QueryState::new(world);
            world.drain_removed::<MeshRenderer>(|_| {});
            world.drain_removed::<GlobalTransform>(|_| {});
            self.epoch = Some(world.resync_epoch());
            stats.rebuilt = true;
        }
        // Removals, from the logs.
        let removed = &mut self.removed;
        world.drain_removed::<MeshRenderer>(|e| removed.push(e));
        world.drain_removed::<GlobalTransform>(|e| removed.push(e));
        for k in 0..self.removed.len() {
            let entity = self.removed[k];
            if let Some(slot) = self.entries.get_mut(entity.index() as usize)
                && slot.is_some_and(|e| e.entity == entity)
                && let Some(entry) = slot.take()
            {
                scene.remove(entry.id);
                stats.removed += 1;
            }
        }
        self.removed.clear();
        // Additions and edits: the chunks where a world transform or a renderer changed.
        let tick = self.tick;
        let interpolation = self.interpolation;
        let entries = &mut self.entries;
        let moving_next = &mut self.moving_next;
        moving_next.clear();
        self.changes
            .for_each_readonly(world, |(entity, global, renderer)| {
                stats.visited += 1;
                let index = entity.index() as usize;
                if index >= entries.len() {
                    entries.resize_with(index + 1, || None);
                }
                let slot = &mut entries[index];
                // A previous holder of the index that was not logged (cannot happen with
                // the logs on; kept for safety).
                if slot.is_some_and(|e| e.entity != entity)
                    && let Some(stale) = slot.take()
                {
                    scene.remove(stale.id);
                    stats.removed += 1;
                }
                if !renderer.is_visible() {
                    if let Some(entry) = slot.take() {
                        scene.remove(entry.id);
                        stats.removed += 1;
                    }
                    return;
                }
                match slot {
                    Some(entry) => {
                        if entry.mesh != renderer.mesh {
                            entry.mesh = renderer.mesh;
                            scene.set_mesh(entry.id, renderer.mesh);
                        }
                        if entry.material != renderer.material {
                            entry.material = renderer.material;
                            scene.set_material(entry.id, renderer.material);
                        }
                        if !interpolation {
                            // The scene compares with what it holds.
                            stats.moved += u32::from(scene.set_affine(entry.id, &global.0));
                        } else if !same(&global.0, &entry.current) {
                            stats.moved += 1;
                            moving_next.push((entity, entry.current));
                            entry.current = global.0;
                            entry.moved_tick = tick;
                        }
                    }
                    None => {
                        let id = scene.insert(SceneInstance {
                            mesh: renderer.mesh,
                            material: renderer.material,
                            world_from_object: Mat4::from(global.0),
                        });
                        *slot = Some(Entry {
                            entity,
                            id,
                            mesh: renderer.mesh,
                            material: renderer.material,
                            current: global.0,
                            moved_tick: 0,
                        });
                        stats.inserted += 1;
                    }
                }
            });
        if interpolation {
            // Movers of the previous tick that stand still now: at their last transform.
            for k in 0..self.moving.len() {
                let entity = self.moving[k].0;
                if let Some(entry) = self.entry(entity).copied()
                    && entry.moved_tick != tick
                {
                    scene.set_affine(entry.id, &entry.current);
                }
            }
            core::mem::swap(&mut self.moving, &mut self.moving_next);
        }
        self.stats = stats;
    }

    /// Places the instances that moved in the last tick at `alpha` between their last
    /// two transforms ([`crate::FixedStep::alpha`]). Call it once per render frame, after the
    /// frame's ticks and before rendering. Costs one blend per moving instance.
    pub fn interpolate<S: SceneSink<Id = Id>>(&mut self, scene: &mut S, alpha: f32) {
        engine_profiler::zone!("runtime::RenderExtract::interpolate");
        if !self.interpolation {
            return;
        }
        for &(entity, previous) in &self.moving {
            if let Some(entry) = self
                .entries
                .get(entity.index() as usize)
                .and_then(Option::as_ref)
                .filter(|e| e.entity == entity)
            {
                scene.set_affine(entry.id, &lerp(&previous, &entry.current, alpha));
            }
        }
    }

    /// Instances that moved in the last tick (interpolated every frame).
    pub fn moving(&self) -> usize {
        self.moving.len()
    }
}
