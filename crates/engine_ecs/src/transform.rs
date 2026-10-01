//! Local and world transforms, and their propagation down the hierarchy.
//!
//! [`Transform`] is what game code and the editor write: translation, rotation and scale
//! relative to the parent (or the world, for a root). [`GlobalTransform`] is derived:
//! [`TransformPropagation::run`] recomputes it from the roots down, over [`Parent`] /
//! [`Children`]. Entities need both components to take part; a child without them
//! breaks the chain for its own subtree.
//!
//! Propagation is incremental (ADR 0064): it visits only the chunks whose `Transform`,
//! `Parent` or `GlobalTransform` was written since its previous run, recomputes their
//! entities and the subtrees below the ones whose world transform changed, and writes a
//! `GlobalTransform` only when its value changes. Chunks whose world transforms stayed
//! the same are not marked, so [`Changed<GlobalTransform>`](crate::Changed) downstream
//! (the renderer's extraction) skips them. A static scene costs one chunk-version scan.

use crate::component::ComponentId;
use crate::entity::Entity;
use crate::hierarchy::{Children, Parent};
use crate::query::{QueryState, With, Without};
use crate::world::World;
use crate::{bytemuck, pod_component, reflect_component};
use engine_core::math::{Affine3A, Mat4, Quat, Vec3};
use engine_reflect::Reflect;

/// Position, rotation and scale relative to the parent (or the world for a root).
///
/// Scale is applied first, then rotation, then translation. Non-uniform scale on a parent
/// shears rotated children, as in every engine that composes affine matrices.
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct Transform {
    /// Rotation (unit quaternion).
    pub rotation: Quat,
    /// Translation.
    pub translation: Vec3,
    /// Scale per axis.
    pub scale: Vec3,
    #[reflect(hidden, skip)]
    _padding: [u32; 2],
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    /// No translation, no rotation, scale 1.
    pub const IDENTITY: Self = Self {
        rotation: Quat::IDENTITY,
        translation: Vec3::ZERO,
        scale: Vec3::ONE,
        _padding: [0; 2],
    };

    /// A translation.
    pub const fn from_translation(translation: Vec3) -> Self {
        Self {
            translation,
            ..Self::IDENTITY
        }
    }

    /// A translation from coordinates.
    pub const fn from_xyz(x: f32, y: f32, z: f32) -> Self {
        Self::from_translation(Vec3::new(x, y, z))
    }

    /// A rotation.
    pub const fn from_rotation(rotation: Quat) -> Self {
        Self {
            rotation,
            ..Self::IDENTITY
        }
    }

    /// A scale.
    pub const fn from_scale(scale: Vec3) -> Self {
        Self {
            scale,
            ..Self::IDENTITY
        }
    }

    /// This transform with another rotation.
    pub const fn with_rotation(self, rotation: Quat) -> Self {
        Self { rotation, ..self }
    }

    /// This transform with another scale.
    pub const fn with_scale(self, scale: Vec3) -> Self {
        Self { scale, ..self }
    }

    /// The transform as an affine matrix.
    #[inline]
    pub fn to_affine(&self) -> Affine3A {
        Affine3A::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
}

reflect_component!(pod Transform);

/// World-space transform, written by [`TransformPropagation`]. Read it for rendering,
/// physics sync and audio; write [`Transform`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct GlobalTransform(pub Affine3A);

impl GlobalTransform {
    /// The identity.
    pub const IDENTITY: Self = Self(Affine3A::IDENTITY);

    /// World-space position.
    pub fn translation(&self) -> Vec3 {
        self.0.translation.into()
    }

    /// As a 4×4 matrix (for GPU upload).
    pub fn to_mat4(&self) -> Mat4 {
        Mat4::from(self.0)
    }
}

pod_component!(GlobalTransform);

/// Roots of hierarchies: no parent, at least one child.
type RootsWithChildren = (Without<Parent>, With<Children>);

/// A child in the hierarchy: one entity lookup fetches everything the full walk needs.
type Node = (
    &'static Parent,
    &'static Transform,
    &'static mut GlobalTransform,
    Option<&'static Children>,
);

/// Whether two affine transforms have the same bytes, the unused fourth lanes of the SIMD
/// columns included: the state hash and snapshots see them, so "unchanged" must mean
/// byte-identical to what a full pass would have written.
#[inline]
fn same_bits(a: &Affine3A, b: &Affine3A) -> bool {
    bytemuck::bytes_of(a) == bytemuck::bytes_of(b)
}

/// The columns of one archetype that propagation reads.
#[derive(Debug, Clone, Copy)]
struct Tracked {
    archetype: u32,
    global: usize,
    local: Option<usize>,
    parent: Option<usize>,
    children: Option<usize>,
}

/// Where an archetype keeps the components the pass reads (`NONE`: it has none), cached
/// per archetype so a walk does not search the column list of every entity it visits.
#[derive(Debug, Clone, Copy)]
struct Cols {
    local: u16,
    global: u16,
    parent: u16,
    children: u16,
}

const NONE: u16 = u16::MAX;

impl Cols {
    fn of(arch: &crate::storage::Archetype, ids: Ids) -> Self {
        let at = |id| arch.column_index(id).map_or(NONE, |c| c as u16);
        Self {
            local: at(ids.local),
            global: at(ids.global),
            parent: at(ids.parent),
            children: at(ids.children),
        }
    }

    #[inline]
    fn get(c: u16) -> Option<usize> {
        (c != NONE).then_some(c as usize)
    }

    #[inline]
    fn local(self) -> Option<usize> {
        Self::get(self.local)
    }

    #[inline]
    fn global(self) -> Option<usize> {
        Self::get(self.global)
    }

    #[inline]
    fn parent(self) -> Option<usize> {
        Self::get(self.parent)
    }

    #[inline]
    fn children(self) -> Option<usize> {
        Self::get(self.children)
    }
}

/// Component ids the pass works with.
#[derive(Debug, Clone, Copy)]
struct Ids {
    local: ComponentId,
    global: ComponentId,
    parent: ComponentId,
    children: ComponentId,
}

/// What the last [`TransformPropagation::run`] did, for tests and profiling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PropagationStats {
    /// Chunks whose `Transform`, `Parent` or `GlobalTransform` had changed.
    pub chunks: u32,
    /// Entities recomputed (those in changed chunks, and the subtrees below changes).
    pub recomputed: u32,
    /// World transforms whose value changed (written and marked).
    pub written: u32,
}

/// Recomputes [`GlobalTransform`] from [`Transform`] down the hierarchy.
///
/// Create it once per world and run it after game logic, before anything reads world
/// transforms. It needs `&mut World`, so in a [`Schedule`](crate::Schedule) it is an
/// exclusive step:
///
/// ```
/// # use engine_ecs::*;
/// let mut world = World::new();
/// let mut transforms = TransformPropagation::new(&mut world);
/// let mut schedule = Schedule::new();
/// schedule.add_exclusive("transforms", move |w| {
///     transforms.run(w);
/// });
/// let car = world.spawn((Transform::from_xyz(10.0, 0.0, 0.0), GlobalTransform::IDENTITY));
/// let wheel = world.spawn((Transform::from_xyz(1.0, 0.0, 0.0), GlobalTransform::IDENTITY));
/// world.set_parent(wheel, Some(car)).unwrap();
/// schedule.run(&mut world, None);
/// assert_eq!(world.get::<GlobalTransform>(wheel).unwrap().translation().x, 11.0);
/// ```
///
/// **Incremental.** An entity is a *candidate* when its chunk's `Transform`, `Parent` or
/// `GlobalTransform` column was written since the previous run (chunk granularity, as for
/// [`Changed`](crate::Changed)). Candidates are recomputed from their parent's world transform, roots
/// first and then by depth, together with their direct children (which picks up subtrees
/// that structural changes reconnected); below that, a subtree is only walked where a
/// world transform actually changed. Every result is bit-identical to recomputing the
/// whole hierarchy ([`Self::run_full`]): both evaluate the same products, and values
/// that are not recomputed are the ones a full pass would reproduce. Entities that a
/// full pass cannot reach (under a parent without transforms, or with a despawned
/// parent) are left alone by both.
pub struct TransformPropagation {
    world_id: u64,
    ids: Ids,
    /// Archetypes seen so far, those with a world transform, and every archetype's
    /// columns.
    seen: usize,
    tracked: Vec<Tracked>,
    cols: Vec<Cols>,
    last_run: u64,
    /// Candidates below the roots: (depth, entity).
    pending: Vec<(u32, Entity)>,
    /// Child lists still to visit (first child, count) with their parent's world
    /// transform, and whether to visit grandchildren even if a child does not change.
    stack: Vec<(Entity, u32, Affine3A, bool)>,
    stats: PropagationStats,
    // The full pass, kept as the reference.
    roots: QueryState<(&'static Transform, &'static mut GlobalTransform), Without<Parent>>,
    parents: QueryState<(&'static GlobalTransform, &'static Children), RootsWithChildren>,
    nodes: QueryState<Node>,
}

impl core::fmt::Debug for TransformPropagation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TransformPropagation")
            .field("tracked", &self.tracked.len())
            .field("last_run", &self.last_run)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl TransformPropagation {
    /// Creates the pass's state for `world`.
    pub fn new(world: &mut World) -> Self {
        let ids = Ids {
            local: world.register::<Transform>(),
            global: world.register::<GlobalTransform>(),
            parent: world.register::<Parent>(),
            children: world.register::<Children>(),
        };
        Self {
            world_id: world.id(),
            ids,
            seen: 0,
            tracked: Vec::new(),
            cols: Vec::new(),
            last_run: 0,
            pending: Vec::new(),
            stack: Vec::new(),
            stats: PropagationStats::default(),
            roots: QueryState::new(world),
            parents: QueryState::new(world),
            nodes: QueryState::new(world),
        }
    }

    /// What the last [`Self::run`] did.
    pub fn stats(&self) -> PropagationStats {
        self.stats
    }

    fn update_archetypes(&mut self, world: &World) {
        assert_eq!(
            self.world_id,
            world.id(),
            "transform propagation used with another world"
        );
        for index in self.seen..world.archetypes.len() {
            let arch = &world.archetypes[index];
            self.cols.push(Cols::of(arch, self.ids));
            if let Some(global) = arch.column_index(self.ids.global) {
                self.tracked.push(Tracked {
                    archetype: index as u32,
                    global,
                    local: arch.column_index(self.ids.local),
                    parent: arch.column_index(self.ids.parent),
                    children: arch.column_index(self.ids.children),
                });
            }
        }
        self.seen = world.archetypes.len();
    }

    /// Updates the world transforms that changed since the previous run (see the type's
    /// documentation). Returns whether any world transform changed value.
    pub fn run(&mut self, world: &mut World) -> bool {
        engine_profiler::zone!("ecs::TransformPropagation::run");
        self.update_archetypes(world);
        let tick = world.bump_change_tick();
        let last_run = core::mem::replace(&mut self.last_run, tick);
        let mut stats = PropagationStats::default();
        self.pending.clear();
        self.stack.clear();
        // Candidates: roots now, the others by depth below.
        for t in 0..self.tracked.len() {
            let tracked = self.tracked[t];
            let chunk_count = world.archetypes[tracked.archetype as usize].chunks.len();
            for chunk in 0..chunk_count {
                let arch = &world.archetypes[tracked.archetype as usize];
                let changed = |column: Option<usize>| {
                    column.is_some_and(|c| arch.column_version(chunk, c) > last_run)
                };
                if !(changed(Some(tracked.global))
                    || changed(tracked.local)
                    || changed(tracked.parent))
                {
                    continue;
                }
                stats.chunks += 1;
                let len = arch.chunk_len(chunk);
                for row in 0..len {
                    let arch = &world.archetypes[tracked.archetype as usize];
                    // SAFETY: `row < len` rows of a live chunk are initialized.
                    let entity = unsafe { *arch.entity_ptr(chunk, row) };
                    if tracked.parent.is_some() {
                        // A child: only if a full pass reaches it, after its ancestors.
                        if tracked.local.is_some()
                            && let Some(depth) = reach_depth(world, &self.cols, entity)
                        {
                            self.pending.push((depth, entity));
                        }
                        continue;
                    }
                    // A root: its own transform, then its children.
                    let (global, changed) = match tracked.local {
                        Some(local) => {
                            // SAFETY: the column holds an initialized `Transform` here.
                            let local =
                                unsafe { *arch.value_ptr(chunk, local, row).cast::<Transform>() };
                            let global = local.to_affine();
                            stats.recomputed += 1;
                            let changed = store(
                                world,
                                tracked.archetype,
                                tracked.global,
                                chunk,
                                row,
                                global,
                                tick,
                            );
                            stats.written += u32::from(changed);
                            (global, changed)
                        }
                        // A root without a local transform keeps its world transform and
                        // still passes it down.
                        // SAFETY: the column holds an initialized `GlobalTransform` here.
                        None => unsafe {
                            let global = (*arch
                                .value_ptr(chunk, tracked.global, row)
                                .cast::<GlobalTransform>())
                            .0;
                            (global, false)
                        },
                    };
                    if let Some(children) = tracked.children {
                        let arch = &world.archetypes[tracked.archetype as usize];
                        // SAFETY: the column holds an initialized `Children` here.
                        let c = unsafe { *arch.value_ptr(chunk, children, row).cast::<Children>() };
                        self.stack.push((c.first, c.count, global, changed));
                    }
                }
            }
        }
        // The candidates' direct children are recomputed whatever happened to the
        // candidate (structural changes can reconnect stale subtrees), compared before
        // they are written; below a change, a subtree is written without comparing.
        walk(world, &self.cols, &mut self.stack, tick, &mut stats);
        self.pending.sort_unstable_by_key(|&(depth, _)| depth);
        for k in 0..self.pending.len() {
            let entity = self.pending[k].1;
            // Its ancestors are final: any change above it was walked down already.
            let Some(loc) = world.entities.location(entity) else {
                continue;
            };
            let arch = &world.archetypes[loc.archetype as usize];
            let cols = self.cols[loc.archetype as usize];
            let (Some(local), Some(global), Some(parent)) =
                (cols.local(), cols.global(), cols.parent())
            else {
                continue;
            };
            let (chunk, row) = (loc.chunk as usize, loc.row as usize);
            // SAFETY: the columns hold initialized values of these types at this row.
            let (local_value, parent_entity) = unsafe {
                (
                    *arch.value_ptr(chunk, local, row).cast::<Transform>(),
                    (*arch.value_ptr(chunk, parent, row).cast::<Parent>()).get(),
                )
            };
            let Some(parent_global) = read_global(world, &self.cols, parent_entity) else {
                continue;
            };
            let value = parent_global * local_value.to_affine();
            stats.recomputed += 1;
            let changed = store(world, loc.archetype, global, chunk, row, value, tick);
            stats.written += u32::from(changed);
            let arch = &world.archetypes[loc.archetype as usize];
            if let Some(children) = cols.children() {
                // SAFETY: the column holds an initialized `Children` here.
                let c = unsafe { *arch.value_ptr(chunk, children, row).cast::<Children>() };
                self.stack.push((c.first, c.count, value, changed));
                walk(world, &self.cols, &mut self.stack, tick, &mut stats);
            }
        }
        self.stats = stats;
        stats.written > 0
    }

    /// Recomputes and writes every world transform from scratch, marking every chunk it
    /// visits: the reference [`Self::run`] is tested against, and the way to rebuild
    /// after writing `GlobalTransform` values by hand. Returns true.
    pub fn run_full(&mut self, world: &mut World) -> bool {
        engine_profiler::zone!("ecs::TransformPropagation::run_full");
        self.roots.for_each_chunk(world, |(locals, globals)| {
            for (l, g) in locals.iter().zip(globals.iter_mut()) {
                g.0 = l.to_affine();
            }
        });
        self.stack.clear();
        let stack = &mut self.stack;
        self.parents
            .for_each(world, |(g, c)| stack.push((c.first, c.count, g.0, true)));
        while let Some((first, count, global, _)) = self.stack.pop() {
            let mut child = first;
            for _ in 0..count {
                let next = match self.nodes.get(world, child) {
                    Some((parent, local, g, children)) => {
                        let child_global = global * local.to_affine();
                        g.0 = child_global;
                        if let Some(c) = children {
                            self.stack.push((c.first, c.count, child_global, true));
                        }
                        parent.next
                    }
                    // A child without transforms: its subtree is skipped.
                    None => match world.get::<Parent>(child) {
                        Some(p) => p.next,
                        None => break,
                    },
                };
                child = next;
            }
        }
        // The next incremental run starts from here.
        self.update_archetypes(world);
        self.last_run = world.bump_change_tick();
        true
    }
}

/// Writes `value` into the `GlobalTransform` at (`chunk`, `row`) of archetype
/// `archetype` (column `column`) if its bits differ, marking the column; returns whether
/// it wrote.
#[inline]
fn store(
    world: &World,
    archetype: u32,
    column: usize,
    chunk: usize,
    row: usize,
    value: Affine3A,
    tick: u64,
) -> bool {
    let arch = &world.archetypes[archetype as usize];
    let ptr = arch.value_ptr(chunk, column, row).cast::<GlobalTransform>();
    // SAFETY: the column holds an initialized `GlobalTransform` at this row.
    let old = unsafe { ptr.read() };
    if same_bits(&old.0, &value) {
        return false;
    }
    // SAFETY: as above; the caller holds the world exclusively (`run` takes `&mut World`
    // and only passes it on as a shared reference), so nothing else reads the value.
    unsafe { ptr.write(GlobalTransform(value)) };
    arch.mark_column(chunk, column, tick);
    true
}

/// Writes `value` into the `GlobalTransform` at (`chunk`, `row`) of archetype `archetype`
/// (column `column`) and marks the column.
#[inline]
fn store_unchecked(
    world: &World,
    archetype: u32,
    column: usize,
    chunk: usize,
    row: usize,
    value: Affine3A,
    tick: u64,
) {
    let arch = &world.archetypes[archetype as usize];
    let ptr = arch.value_ptr(chunk, column, row).cast::<GlobalTransform>();
    // SAFETY: the column holds an initialized `GlobalTransform` at this row; the caller
    // holds the world exclusively, as for `store`.
    unsafe { ptr.write(GlobalTransform(value)) };
    arch.mark_column(chunk, column, tick);
}

/// The world transform of `entity`, if it has one.
#[inline]
fn read_global(world: &World, cols: &[Cols], entity: Entity) -> Option<Affine3A> {
    let loc = world.entities.location(entity)?;
    let arch = &world.archetypes[loc.archetype as usize];
    let column = cols[loc.archetype as usize].global()?;
    // SAFETY: the column holds an initialized `GlobalTransform` at this row.
    Some(unsafe {
        (*arch
            .value_ptr(loc.chunk as usize, column, loc.row as usize)
            .cast::<GlobalTransform>())
        .0
    })
}

/// Depth of a child (1 below a root) when a full pass reaches it, i.e. every ancestor
/// below the root has a `Transform` and a `GlobalTransform` and the root a
/// `GlobalTransform` and `Children`; `None` otherwise (a broken chain, a despawned
/// ancestor).
fn reach_depth(world: &World, cols: &[Cols], entity: Entity) -> Option<u32> {
    let mut depth = 1u32;
    let mut current = parent_of(world, cols, entity)?;
    loop {
        let loc = world.entities.location(current)?;
        let arch = &world.archetypes[loc.archetype as usize];
        cols[loc.archetype as usize].global()?;
        cols[loc.archetype as usize].children()?;
        let Some(parent) = cols[loc.archetype as usize].parent() else {
            return Some(depth);
        };
        cols[loc.archetype as usize].local()?;
        // SAFETY: the column holds an initialized `Parent` at this row.
        current = unsafe {
            (*arch
                .value_ptr(loc.chunk as usize, parent, loc.row as usize)
                .cast::<Parent>())
            .get()
        };
        depth += 1;
    }
}

/// `entity`'s parent, if it has one.
#[inline]
fn parent_of(world: &World, cols: &[Cols], entity: Entity) -> Option<Entity> {
    let loc = world.entities.location(entity)?;
    let arch = &world.archetypes[loc.archetype as usize];
    let column = cols[loc.archetype as usize].parent()?;
    // SAFETY: the column holds an initialized `Parent` at this row.
    Some(unsafe {
        (*arch
            .value_ptr(loc.chunk as usize, column, loc.row as usize)
            .cast::<Parent>())
        .get()
    })
}

/// Visits the child lists on `stack`: every listed child is recomputed from its
/// parent's world transform, and its own children are visited only if its value
/// changed.
fn walk(
    world: &World,
    cols: &[Cols],
    stack: &mut Vec<(Entity, u32, Affine3A, bool)>,
    tick: u64,
    stats: &mut PropagationStats,
) {
    while let Some((first, count, parent_global, parent_changed)) = stack.pop() {
        let mut child = first;
        for _ in 0..count {
            let Some(loc) = world.entities.location(child) else {
                break;
            };
            let arch = &world.archetypes[loc.archetype as usize];
            let Some(parent) = cols[loc.archetype as usize].parent() else {
                break;
            };
            let (chunk, row) = (loc.chunk as usize, loc.row as usize);
            // SAFETY: the column holds an initialized `Parent` at this row.
            let link = unsafe { *arch.value_ptr(chunk, parent, row).cast::<Parent>() };
            // A child without transforms: its subtree is skipped (as in a full pass).
            if let (Some(local), Some(global)) = (
                cols[loc.archetype as usize].local(),
                cols[loc.archetype as usize].global(),
            ) {
                // SAFETY: the column holds an initialized `Transform` at this row.
                let local = unsafe { *arch.value_ptr(chunk, local, row).cast::<Transform>() };
                let value = parent_global * local.to_affine();
                stats.recomputed += 1;
                // Below a changed parent the child changes too: written without comparing
                // (a degenerate transform that maps both to the same value costs one
                // needless mark, never a wrong value).
                let wrote = if parent_changed {
                    store_unchecked(world, loc.archetype, global, chunk, row, value, tick);
                    true
                } else {
                    store(world, loc.archetype, global, chunk, row, value, tick)
                };
                if wrote {
                    stats.written += 1;
                    if let Some(children) = cols[loc.archetype as usize].children() {
                        // SAFETY: the column holds an initialized `Children` here.
                        let c = unsafe { *arch.value_ptr(chunk, children, row).cast::<Children>() };
                        stack.push((c.first, c.count, value, true));
                    }
                }
            }
            child = link.next;
        }
    }
}
