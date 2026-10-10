//! Extraction: the renderable entities of the ECS world, mirrored into a GPU scene.

use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Mat4};
use engine_core::sync::CachePadded;
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
    /// Whether the sink places moving instances between ticks itself
    /// ([`Self::set_motion`], [`Self::set_alpha`]): a [`GpuScene`] does it on the GPU, so
    /// the CPU's work per frame does not grow with the moving instances. Otherwise
    /// [`RenderExtract::interpolate`] blends them on the CPU every frame.
    fn interpolates(&self) -> bool {
        false
    }
    /// Moves one between two ticks, from its transform at the tick before to the last
    /// tick's (called when [`Self::interpolates`]; [`Self::set_affine`] stops it).
    fn set_motion(&mut self, id: Self::Id, from: &Affine3A, to: &Affine3A) -> bool {
        let _ = from;
        self.set_affine(id, to)
    }
    /// [`Self::set_motion`] from the transform it was last given (called when
    /// [`Self::interpolates`]: extraction gives every move, so that is the last tick's).
    fn step_motion(&mut self, id: Self::Id, to: &Affine3A) -> bool {
        self.set_affine(id, to)
    }
    /// [`Self::step_motion`] for the (instance, transform) pairs of `runs` runs, `run(k)`
    /// listing run `k`'s, in order. A [`GpuScene`] spreads them over its pool
    /// ([`GpuScene::set_jobs`]).
    ///
    /// # Safety
    ///
    /// No instance appears twice across the runs, and every transform pointer is valid
    /// for reads during the call.
    unsafe fn step_motions<'r>(
        &mut self,
        runs: usize,
        run: &(dyn Fn(usize) -> &'r [(Self::Id, *const Affine3A)] + Sync),
    ) {
        for k in 0..runs {
            for &(id, to) in run(k) {
                // SAFETY: guaranteed by the caller.
                self.step_motion(id, unsafe { &*to });
            }
        }
    }
    /// Where between the last two ticks the next frame places moving instances (called
    /// when [`Self::interpolates`]).
    fn set_alpha(&mut self, alpha: f32) {
        let _ = alpha;
    }
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
    fn interpolates(&self) -> bool {
        true
    }
    fn set_motion(&mut self, id: InstanceId, from: &Affine3A, to: &Affine3A) -> bool {
        GpuScene::set_motion(self, id, from, to)
    }
    fn step_motion(&mut self, id: InstanceId, to: &Affine3A) -> bool {
        GpuScene::step_motion(self, id, to)
    }
    unsafe fn step_motions<'r>(
        &mut self,
        runs: usize,
        run: &(dyn Fn(usize) -> &'r [(InstanceId, *const Affine3A)] + Sync),
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe { GpuScene::step_motions(self, runs, run) };
    }
    fn set_alpha(&mut self, alpha: f32) {
        GpuScene::set_alpha(self, alpha);
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
/// so motion stays smooth whatever the tick rate. With a [`GpuScene`] the GPU places
/// them ([`SceneSink::interpolates`]): `sync` gives each moving instance its two ticks'
/// transforms once per tick, and `interpolate` only the frame's factor.
///
/// Extraction only reads the world: the simulation stays deterministic, and its state
/// hash is unaffected. [`Self::sync_on`] spreads the comparisons over a thread pool: the
/// entities of the changed chunks are compared in runs of rows in parallel, and the
/// scene is told what changed on the calling thread, in the chunks' order, with the
/// same results whatever the number of threads.
pub struct RenderExtract<Id = InstanceId> {
    changes: Changes,
    /// By entity index.
    entries: Vec<Option<Entry<Id>>>,
    /// Instances that moved in the last tick, and (when the CPU interpolates) their
    /// transforms before it, in the same order.
    moving: Vec<Entity>,
    moving_before: Vec<Affine3A>,
    moving_next: Vec<Entity>,
    moving_before_next: Vec<Affine3A>,
    /// This sync's runs of rows of changed chunks, and what each left for the scene (on
    /// cache lines of its own: a job pushes to it for every entity).
    runs: Vec<Run>,
    works: Vec<CachePadded<RunWork<Id>>>,
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

/// Rows of a changed chunk per run of [`RenderExtract::sync_on`]'s parallel part.
const RUN_ROWS: usize = 1024;
/// Fewer rows to visit than this are compared on the calling thread.
const PARALLEL_MIN_ROWS: usize = 4096;

/// A run of rows of a changed chunk: pointers into the world's chunk, read while the sync
/// borrows the world.
#[derive(Clone, Copy)]
struct Run {
    entities: *const Entity,
    globals: *const GlobalTransform,
    renderers: *const MeshRenderer,
    len: usize,
}

// SAFETY: the pointers are only read, while the sync borrows the world.
unsafe impl Send for Run {}
// SAFETY: as above.
unsafe impl Sync for Run {}

/// What a run of rows left for the serial part of a sync.
struct RunWork<Id> {
    /// Entities whose instance draws the same and whose world transform changed (or, without
    /// interpolation, every such entity): their instances with the world transforms in the
    /// chunk; and, when the CPU interpolates, the transforms before; in the same order.
    moved_entities: Vec<Entity>,
    moved: Vec<(Id, *const Affine3A)>,
    before: Vec<Affine3A>,
    /// Entities handled whole on the calling thread: added, edited, hidden, or holding a
    /// stale entry.
    slow: Vec<(Entity, *const GlobalTransform, MeshRenderer)>,
    visited: u32,
}

impl<Id> Default for RunWork<Id> {
    fn default() -> Self {
        Self {
            moved_entities: Vec::new(),
            moved: Vec::new(),
            before: Vec::new(),
            slow: Vec::new(),
            visited: 0,
        }
    }
}

/// Shared with the pool's workers: run `k`'s work and the entries of its entities are
/// touched only by the call for run `k` (an entity is in one row of one chunk).
struct Shared<Id> {
    entries: *mut Option<Entry<Id>>,
    entries_len: usize,
    works: *mut CachePadded<RunWork<Id>>,
}

// SAFETY: every element is used by one thread at a time (see the type); `Id` moves
// between threads with the entries.
unsafe impl<Id: Send> Sync for Shared<Id> {}

// SAFETY: the pointers are only read, into the world's chunks while the sync borrows the
// world (see `RenderExtract::sync_on`); `Id` is shared with the scene's workers.
unsafe impl<Id: Sync> Sync for RunWork<Id> {}

/// The parallel part of a sync for one run of rows: compares each entity with its entry,
/// updates the entries of those that only moved, and leaves the rest to the serial part.
///
/// # Safety
///
/// `run` points into the world's chunks, `shared.works` has an element at `k`, and no
/// other call uses run `k`'s work or its entities' entries at the same time.
unsafe fn extract_run<Id: Copy + Eq>(
    shared: &Shared<Id>,
    k: usize,
    run: &Run,
    tick: u64,
    interpolation: bool,
    keep_before: bool,
) {
    // SAFETY: guaranteed by the caller.
    let (work, entities, globals, renderers) = unsafe {
        (
            &mut *shared.works.add(k),
            core::slice::from_raw_parts(run.entities, run.len),
            core::slice::from_raw_parts(run.globals, run.len),
            core::slice::from_raw_parts(run.renderers, run.len),
        )
    };
    work.moved_entities.clear();
    work.moved.clear();
    work.before.clear();
    work.slow.clear();
    work.visited = run.len as u32;
    for ((&entity, global), renderer) in entities.iter().zip(globals).zip(renderers) {
        let index = entity.index() as usize;
        // SAFETY: the entity's own entry (see the function).
        let entry = (index < shared.entries_len)
            .then(|| unsafe { (*shared.entries.add(index)).as_mut() })
            .flatten();
        match entry {
            Some(entry)
                if entry.entity == entity
                    && renderer.is_visible()
                    && entry.mesh == renderer.mesh
                    && entry.material == renderer.material =>
            {
                if !interpolation {
                    // The scene compares with what it holds.
                    work.moved_entities.push(entity);
                    work.moved.push((entry.id, &global.0));
                } else if !same(&global.0, &entry.current) {
                    if keep_before {
                        work.before.push(entry.current);
                    }
                    work.moved_entities.push(entity);
                    work.moved.push((entry.id, &global.0));
                    entry.current = global.0;
                    entry.moved_tick = tick;
                }
            }
            _ => work.slow.push((entity, global, *renderer)),
        }
    }
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
            moving_before: Vec::new(),
            moving_next: Vec::new(),
            moving_before_next: Vec::new(),
            runs: Vec::new(),
            works: Vec::new(),
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
    pub fn sync<S: SceneSink<Id = Id>>(&mut self, world: &mut World, scene: &mut S)
    where
        Id: Send + Sync,
    {
        self.sync_on(world, scene, None);
    }

    /// [`Self::sync`] with the comparisons spread over `jobs`' workers when many entities
    /// are visited (see the type's documentation). Allocates only when more changed rows
    /// than ever before are visited.
    pub fn sync_on<S: SceneSink<Id = Id>>(
        &mut self,
        world: &mut World,
        scene: &mut S,
        jobs: Option<&ThreadPool>,
    ) where
        Id: Send + Sync,
    {
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
            self.moving_before.clear();
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
        // Additions and edits: the chunks where a world transform or a renderer changed, in
        // runs of rows.
        let tick = self.tick;
        let interpolation = self.interpolation;
        let gpu_motion = interpolation && scene.interpolates();
        let keep_before = interpolation && !gpu_motion;
        self.runs.clear();
        let runs = &mut self.runs;
        let mut rows = 0;
        let world = &*world;
        self.changes
            .for_each_chunk_readonly(world, |(entities, globals, renderers)| {
                rows += entities.len();
                for start in (0..entities.len()).step_by(RUN_ROWS) {
                    runs.push(Run {
                        entities: entities[start..].as_ptr(),
                        globals: globals[start..].as_ptr(),
                        renderers: renderers[start..].as_ptr(),
                        len: (entities.len() - start).min(RUN_ROWS),
                    });
                }
            });
        let n = self.runs.len();
        if self.works.len() < n {
            self.works.resize_with(n, CachePadded::default);
        }
        // Each entity against its entry: on the pool's workers when there are many.
        {
            let shared = Shared {
                entries: self.entries.as_mut_ptr(),
                entries_len: self.entries.len(),
                works: self.works.as_mut_ptr(),
            };
            let runs = &self.runs;
            let compare = |k: usize, _: core::ops::Range<usize>| {
                // SAFETY: one call per run; the runs point into the world's chunks, borrowed
                // for the sync, and an entity is in one run.
                unsafe { extract_run(&shared, k, &runs[k], tick, interpolation, keep_before) };
            };
            match jobs {
                Some(pool) if rows >= PARALLEL_MIN_ROWS => pool.parallel_for(n, 1, compare),
                _ => (0..n).for_each(|k| compare(k, k..k + 1)),
            }
        }
        // What changed, into the scene, in the runs' order: additions and edits, then the
        // moves at once (the scene may spread them over its pool).
        self.moving_next.clear();
        self.moving_before_next.clear();
        let works = core::mem::take(&mut self.works);
        for work in &works[..n] {
            stats.visited += work.visited;
            for &(entity, global, renderer) in &work.slow {
                // SAFETY: points into the world's chunk, unchanged during the sync.
                let global = unsafe { &*global };
                self.extract_one(entity, global, renderer, scene, &mut stats, gpu_motion);
            }
            if !interpolation {
                for &(id, global) in &work.moved {
                    // SAFETY: as above.
                    stats.moved += u32::from(scene.set_affine(id, unsafe { &*global }));
                }
                continue;
            }
            stats.moved += work.moved.len() as u32;
            self.moving_next.extend_from_slice(&work.moved_entities);
            self.moving_before_next.extend_from_slice(&work.before);
        }
        if gpu_motion {
            let works = &works[..n];
            // SAFETY: an instance is in the runs once (an entity is in one row of one chunk,
            // and entities do not share instances); the transforms point into the world's
            // chunks, unchanged during the sync. Each goes from what the scene was last
            // given: the entry's transform before.
            unsafe { scene.step_motions(n, &|k| works[k].moved.as_slice()) };
        }
        self.works = works;
        if interpolation {
            // Movers of the previous tick that stand still now: at their last transform.
            for &entity in &self.moving {
                if let Some(entry) = self
                    .entries
                    .get(entity.index() as usize)
                    .and_then(Option::as_ref)
                    .filter(|e| e.entity == entity && e.moved_tick != tick)
                {
                    scene.set_affine(entry.id, &entry.current);
                }
            }
            core::mem::swap(&mut self.moving, &mut self.moving_next);
            core::mem::swap(&mut self.moving_before, &mut self.moving_before_next);
        }
        self.stats = stats;
    }

    /// The serial part of a sync for an entity the parallel part left: added, edited,
    /// hidden, or holding a stale entry.
    fn extract_one<S: SceneSink<Id = Id>>(
        &mut self,
        entity: Entity,
        global: &GlobalTransform,
        renderer: MeshRenderer,
        scene: &mut S,
        stats: &mut ExtractStats,
        gpu_motion: bool,
    ) {
        let index = entity.index() as usize;
        if index >= self.entries.len() {
            self.entries.resize_with(index + 1, || None);
        }
        let slot = &mut self.entries[index];
        // A previous holder of the index that was not logged (cannot happen with the logs
        // on; kept for safety).
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
                if !self.interpolation {
                    // The scene compares with what it holds.
                    stats.moved += u32::from(scene.set_affine(entry.id, &global.0));
                } else if !same(&global.0, &entry.current) {
                    stats.moved += 1;
                    self.moving_next.push(entity);
                    if !gpu_motion {
                        self.moving_before_next.push(entry.current);
                    }
                    if gpu_motion {
                        // From what the scene was last given: `entry.current`.
                        scene.step_motion(entry.id, &global.0);
                    }
                    entry.current = global.0;
                    entry.moved_tick = self.tick;
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
    }

    /// Places the instances that moved in the last tick at `alpha` between their last
    /// two transforms ([`crate::FixedStep::alpha`]). Call it once per render frame, after the
    /// frame's ticks and before rendering. Costs one blend per moving instance, or nothing
    /// when the sink places them itself ([`SceneSink::interpolates`]).
    pub fn interpolate<S: SceneSink<Id = Id>>(&mut self, scene: &mut S, alpha: f32) {
        engine_profiler::zone!("runtime::RenderExtract::interpolate");
        if !self.interpolation {
            return;
        }
        if scene.interpolates() {
            // The sink places them (a `GpuScene`: on the GPU).
            scene.set_alpha(alpha);
            return;
        }
        for (&entity, previous) in self.moving.iter().zip(&self.moving_before) {
            if let Some(entry) = self
                .entries
                .get(entity.index() as usize)
                .and_then(Option::as_ref)
                .filter(|e| e.entity == entity)
            {
                scene.set_affine(entry.id, &lerp(previous, &entry.current, alpha));
            }
        }
    }

    /// Instances that moved in the last tick (interpolated every frame).
    pub fn moving(&self) -> usize {
        self.moving.len()
    }
}
