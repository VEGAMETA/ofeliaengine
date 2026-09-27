//! The world: entities, archetypes, structural changes, direct access.

use crate::bundle::{Bundle, MAX_BUNDLE};
use crate::component::{Component, ComponentDesc, ComponentId, Components, RegisterError};
use crate::entity::{Entities, Entity, Location};
use crate::fxhash::FastMap;
use crate::storage::{Archetype, ChunkPool};
use core::any::TypeId;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

static NEXT_WORLD_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct BundleInfo {
    archetype: u32,
    columns: [u16; MAX_BUNDLE],
}

/// Entities and their components.
///
/// Deterministic for the same sequence of calls: entity allocation, row placement,
/// query order (canonical, by component names) and [`World::state_hash`] do not depend
/// on addresses, timing or the history of which archetypes were created first.
///
/// ```
/// use engine_ecs::{bytemuck, pod_component, QueryState, World};
///
/// #[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
/// #[repr(C)]
/// struct Position(f32, f32);
/// #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
/// #[repr(C)]
/// struct Velocity(f32, f32);
/// pod_component!(Position, Velocity);
///
/// let mut world = World::new();
/// let e = world.spawn((Position(0.0, 0.0), Velocity(1.0, 2.0)));
/// let mut movement = QueryState::<(&mut Position, &Velocity)>::new(&mut world);
/// movement.for_each(&mut world, |(p, v)| {
///     p.0 += v.0;
///     p.1 += v.1;
/// });
/// assert_eq!(world.get::<Position>(e), Some(&Position(1.0, 2.0)));
/// ```
pub struct World {
    id: u64,
    pub(crate) components: Components,
    pub(crate) archetypes: Vec<Archetype>,
    /// Archetype indices sorted by their canonical key.
    pub(crate) canonical: Vec<u32>,
    by_ids: FastMap<Box<[ComponentId]>, u32>,
    bundles: FastMap<TypeId, BundleInfo>,
    pub(crate) entities: Entities,
    pub(crate) pool: ChunkPool,
    pub(crate) change_tick: u64,
    pub(crate) sim_tick: u64,
    scratch: Vec<ComponentId>,
    pub(crate) resources: crate::resource::Resources,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for World {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("World")
            .field("entities", &self.entities.alive)
            .field("archetypes", &self.archetypes.len())
            .field("components", &self.components.len())
            .field("sim_tick", &self.sim_tick)
            .finish()
    }
}

// SAFETY: see `Archetype`'s impls; the pool only holds chunk memory owned by the world.
unsafe impl Send for World {}
// SAFETY: shared access only reads.
unsafe impl Sync for World {}

/// Moves `entity`'s row from its archetype to `target`. Target columns the source has are
/// copied; the others are left uninitialized for the caller. Source columns the target
/// lacks are forgotten: the caller has moved them out or dropped them.
fn move_row(
    archetypes: &mut [Archetype],
    pool: &mut ChunkPool,
    entities: &mut Entities,
    entity: Entity,
    from: Location,
    target: u32,
    tick: u64,
) -> Location {
    debug_assert_ne!(from.archetype, target);
    let (src, dst) = if from.archetype < target {
        let (a, b) = archetypes.split_at_mut(target as usize);
        (&mut a[from.archetype as usize], &mut b[0])
    } else {
        let (a, b) = archetypes.split_at_mut(from.archetype as usize);
        (&mut b[0], &mut a[target as usize])
    };
    let (chunk, row) = dst.push_row(pool, entity, tick);
    for column in 0..dst.columns.len() {
        if let Some(source) = src.column_index(dst.columns[column].id) {
            // SAFETY: an initialized source value, uninitialized room in the target;
            // different archetypes never share chunks.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.value_ptr(from.chunk as usize, source, from.row as usize),
                    dst.value_ptr(chunk as usize, column, row as usize),
                    dst.columns[column].size as usize,
                );
            }
        }
    }
    if let Some(moved) = src.fill_hole(pool, from.chunk as usize, from.row as usize, tick) {
        entities.set_location(moved.index(), from);
    }
    let location = Location {
        archetype: target,
        chunk,
        row,
    };
    entities.set_location(entity.index(), location);
    location
}

impl World {
    /// An empty world.
    pub fn new() -> Self {
        let mut world = Self {
            id: NEXT_WORLD_ID.fetch_add(1, Ordering::Relaxed),
            components: Components::default(),
            archetypes: Vec::new(),
            canonical: Vec::new(),
            by_ids: FastMap::default(),
            bundles: FastMap::default(),
            entities: Entities::default(),
            pool: ChunkPool::default(),
            change_tick: 1,
            sim_tick: 0,
            scratch: Vec::new(),
            resources: crate::resource::Resources::default(),
        };
        world.archetype_for_sorted(&[]);
        world
    }

    /// Unique id of this world instance (queries remember it to catch mix-ups).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Number of live entities.
    pub fn len(&self) -> usize {
        self.entities.alive as usize
    }

    /// Whether there are no entities.
    pub fn is_empty(&self) -> bool {
        self.entities.alive == 0
    }

    /// The component registry.
    pub fn components(&self) -> &Components {
        &self.components
    }

    /// All archetypes, in creation order.
    pub fn archetypes(&self) -> &[Archetype] {
        &self.archetypes
    }

    /// Current change tick. Every write stamps the chunk column it touches with a new
    /// tick; [`Changed`](crate::Changed) filters compare against the query's previous run.
    /// Change ticks are bookkeeping, not simulated state: snapshots do not restore them.
    pub fn change_tick(&self) -> u64 {
        self.change_tick
    }

    #[inline]
    pub(crate) fn bump_change_tick(&mut self) -> u64 {
        self.change_tick += 1;
        self.change_tick
    }

    /// Fixed simulation tick (ADR 0019). Part of the simulated state: snapshots save it
    /// and the state hash includes it.
    pub fn sim_tick(&self) -> u64 {
        self.sim_tick
    }

    /// Sets the simulation tick.
    pub fn set_sim_tick(&mut self, tick: u64) {
        self.sim_tick = tick;
    }

    /// Advances the simulation tick by one and returns the new value.
    pub fn advance_sim_tick(&mut self) -> u64 {
        self.sim_tick += 1;
        self.sim_tick
    }

    /// Registers `T` (idempotent). Components are also registered on first use.
    pub fn register<T: Component>(&mut self) -> ComponentId {
        self.components.register::<T>()
    }

    /// Registers a component defined at run time (scripts).
    ///
    /// # Safety
    ///
    /// Values stored under this id must match `desc.layout`, be safe to send and share
    /// between threads, be dropped correctly by `desc.drop`, and, when `desc.pod` is
    /// set, be plain data: no padding, no pointers, every bit pattern valid.
    pub unsafe fn register_dynamic(
        &mut self,
        desc: ComponentDesc,
    ) -> Result<ComponentId, RegisterError> {
        self.components.add(desc)
    }

    /// Id of `T`, if registered.
    pub fn component_id<T: Component>(&self) -> Option<ComponentId> {
        self.components.id_of::<T>()
    }

    /// The components `entity` has (in canonical order), if it is alive.
    pub fn component_ids_of(&self, entity: Entity) -> Option<&[ComponentId]> {
        let loc = self.entities.location(entity)?;
        Some(self.archetypes[loc.archetype as usize].components())
    }

    fn archetype_for_sorted(&mut self, ids: &[ComponentId]) -> u32 {
        if let Some(&index) = self.by_ids.get(ids) {
            return index;
        }
        let archetype = Archetype::new(ids.into(), &self.components);
        let index = self.archetypes.len() as u32;
        let at = self
            .canonical
            .partition_point(|&a| self.archetypes[a as usize].key < archetype.key);
        self.canonical.insert(at, index);
        self.archetypes.push(archetype);
        self.by_ids.insert(ids.into(), index);
        index
    }

    fn bundle_info<B: Bundle>(&mut self) -> BundleInfo {
        if let Some(&info) = self.bundles.get(&TypeId::of::<B>()) {
            return info;
        }
        let mut ids = [ComponentId(0); MAX_BUNDLE];
        B::register(&mut self.components, &mut ids);
        let mut sorted = ids[..B::COUNT].to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), B::COUNT, "a bundle repeats a component type");
        let archetype = self.archetype_for_sorted(&sorted);
        let arch = &self.archetypes[archetype as usize];
        let mut columns = [0u16; MAX_BUNDLE];
        for (column, id) in columns.iter_mut().zip(&ids[..B::COUNT]) {
            *column = arch.column_index(*id).unwrap_or_default() as u16;
        }
        let info = BundleInfo { archetype, columns };
        self.bundles.insert(TypeId::of::<B>(), info);
        info
    }

    #[inline]
    fn spawn_with<B: Bundle>(&mut self, info: BundleInfo, bundle: B, tick: u64) -> Entity {
        let entity = self.entities.alloc(Location {
            archetype: info.archetype,
            chunk: 0,
            row: 0,
        });
        let arch = &mut self.archetypes[info.archetype as usize];
        let (chunk, row) = arch.push_row(&mut self.pool, entity, tick);
        self.entities.set_location(
            entity.index(),
            Location {
                archetype: info.archetype,
                chunk,
                row,
            },
        );
        let mut dst = [core::ptr::null_mut(); MAX_BUNDLE];
        for (d, &column) in dst.iter_mut().zip(&info.columns[..B::COUNT]) {
            *d = arch.value_ptr(chunk as usize, column as usize, row as usize);
        }
        // SAFETY: each destination is uninitialized room for its tuple position's type.
        unsafe { bundle.write(&dst) };
        entity
    }

    /// Spawns an entity with the components of `bundle`.
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Entity {
        let info = self.bundle_info::<B>();
        let tick = self.bump_change_tick();
        self.spawn_with(info, bundle, tick)
    }

    /// Spawns one entity per bundle, in order.
    pub fn spawn_batch<B: Bundle>(&mut self, bundles: impl IntoIterator<Item = B>) {
        let info = self.bundle_info::<B>();
        let tick = self.bump_change_tick();
        let bundles = bundles.into_iter();
        self.entities.metas.reserve(bundles.size_hint().0);
        for bundle in bundles {
            self.spawn_with(info, bundle, tick);
        }
    }

    /// Whether `entity` is alive.
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.location(entity).is_some()
    }

    /// Despawns `entity`, dropping its components. False if it was not alive.
    pub fn despawn(&mut self, entity: Entity) -> bool {
        let Some(loc) = self.entities.location(entity) else {
            return false;
        };
        let tick = self.bump_change_tick();
        let arch = &mut self.archetypes[loc.archetype as usize];
        for column in 0..arch.columns.len() {
            if let Some(drop) = arch.columns[column].drop {
                // SAFETY: the row is initialized.
                unsafe { drop(arch.value_ptr(loc.chunk as usize, column, loc.row as usize)) };
            }
        }
        if let Some(moved) =
            arch.fill_hole(&mut self.pool, loc.chunk as usize, loc.row as usize, tick)
        {
            self.entities.set_location(moved.index(), loc);
        }
        self.entities.free(entity);
        true
    }

    /// Inserts (or replaces) the resource of type `T`.
    pub fn insert_resource<T: Send + Sync + 'static>(&mut self, value: T) {
        let id = self.resources.register::<T>();
        *self.resources.slots[id].value.get_mut() = Some(Box::new(value));
    }

    /// The resource of type `T`.
    pub fn resource<T: 'static>(&self) -> Option<&T> {
        let id = self.resources.id_of::<T>()?;
        // SAFETY: `&self` excludes `resource_mut`; the scheduler, the only other writer,
        // holds the world exclusively while systems run.
        unsafe { self.resources.get(id) }
    }

    /// The resource of type `T`, mutably.
    pub fn resource_mut<T: 'static>(&mut self) -> Option<&mut T> {
        let id = self.resources.id_of::<T>()?;
        // SAFETY: `&mut self` is exclusive.
        unsafe { self.resources.get_mut(id) }
    }

    /// Removes and returns the resource of type `T`.
    pub fn remove_resource<T: 'static>(&mut self) -> Option<T> {
        let id = self.resources.id_of::<T>()?;
        let value = self.resources.slots[id].value.get_mut().take()?;
        value.downcast::<T>().ok().map(|b| *b)
    }

    /// Despawns every entity, in index order.
    pub fn clear(&mut self) {
        for arch in &mut self.archetypes {
            arch.clear(&mut self.pool);
        }
        for index in 0..self.entities.metas.len() {
            let meta = self.entities.metas[index];
            if meta.location.archetype != crate::entity::DEAD {
                self.entities.free(Entity::from_bits(
                    (u64::from(meta.generation) << 32) | index as u64,
                ));
            }
        }
        self.bump_change_tick();
    }

    fn add_target(&mut self, source: u32, id: ComponentId) -> u32 {
        if let Some(&target) = self.archetypes[source as usize].add_edges.get(&id.0) {
            return target;
        }
        let mut ids = core::mem::take(&mut self.scratch);
        ids.clear();
        ids.extend_from_slice(&self.archetypes[source as usize].ids);
        let at = ids.partition_point(|&c| c < id);
        ids.insert(at, id);
        let target = self.archetype_for_sorted(&ids);
        self.scratch = ids;
        self.archetypes[source as usize]
            .add_edges
            .insert(id.0, target);
        target
    }

    fn remove_target(&mut self, source: u32, id: ComponentId) -> u32 {
        if let Some(&target) = self.archetypes[source as usize].remove_edges.get(&id.0) {
            return target;
        }
        let mut ids = core::mem::take(&mut self.scratch);
        ids.clear();
        ids.extend(
            self.archetypes[source as usize]
                .ids
                .iter()
                .copied()
                .filter(|&c| c != id),
        );
        let target = self.archetype_for_sorted(&ids);
        self.scratch = ids;
        self.archetypes[source as usize]
            .remove_edges
            .insert(id.0, target);
        target
    }

    /// Adds `value` to `entity`, or replaces the existing one. False (and `value`
    /// dropped) if the entity is not alive.
    pub fn insert<T: Component>(&mut self, entity: Entity, value: T) -> bool {
        let id = self.components.register::<T>();
        let Some(loc) = self.entities.location(entity) else {
            return false;
        };
        let tick = self.bump_change_tick();
        let arch = &mut self.archetypes[loc.archetype as usize];
        if let Some(column) = arch.column_index(id) {
            let ptr = arch
                .value_ptr(loc.chunk as usize, column, loc.row as usize)
                .cast::<T>();
            // SAFETY: the column holds an initialized `T` at this row.
            unsafe { *ptr = value };
            arch.mark_column(loc.chunk as usize, column, tick);
            return true;
        }
        let target = self.add_target(loc.archetype, id);
        let new = move_row(
            &mut self.archetypes,
            &mut self.pool,
            &mut self.entities,
            entity,
            loc,
            target,
            tick,
        );
        let arch = &self.archetypes[target as usize];
        let column = arch.column_index(id).unwrap_or_default();
        // SAFETY: `move_row` left this column's slot uninitialized for the new value.
        unsafe {
            arch.value_ptr(new.chunk as usize, column, new.row as usize)
                .cast::<T>()
                .write(value);
        }
        true
    }

    /// Removes `T` from `entity` and returns it.
    pub fn remove<T: Component>(&mut self, entity: Entity) -> Option<T> {
        let id = self.components.id_of::<T>()?;
        let loc = self.entities.location(entity)?;
        let column = self.archetypes[loc.archetype as usize].column_index(id)?;
        let tick = self.bump_change_tick();
        // SAFETY: the column holds an initialized `T`; `move_row` forgets the slot.
        let value = unsafe {
            self.archetypes[loc.archetype as usize]
                .value_ptr(loc.chunk as usize, column, loc.row as usize)
                .cast::<T>()
                .read()
        };
        let target = self.remove_target(loc.archetype, id);
        move_row(
            &mut self.archetypes,
            &mut self.pool,
            &mut self.entities,
            entity,
            loc,
            target,
            tick,
        );
        Some(value)
    }

    /// Shared reference to `entity`'s `T`.
    #[inline]
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        let id = self.components.id_of::<T>()?;
        let loc = self.entities.location(entity)?;
        let arch = &self.archetypes[loc.archetype as usize];
        let column = arch.column_index(id)?;
        // SAFETY: the column holds an initialized `T` at this row.
        Some(unsafe {
            &*arch
                .value_ptr(loc.chunk as usize, column, loc.row as usize)
                .cast::<T>()
        })
    }

    /// Mutable reference to `entity`'s `T`; stamps the chunk column as changed.
    #[inline]
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        let id = self.components.id_of::<T>()?;
        let loc = self.entities.location(entity)?;
        let column = self.archetypes[loc.archetype as usize].column_index(id)?;
        let tick = self.bump_change_tick();
        let arch = &mut self.archetypes[loc.archetype as usize];
        arch.mark_column(loc.chunk as usize, column, tick);
        // SAFETY: the column holds an initialized `T` at this row; `&mut self` makes the
        // reference unique.
        Some(unsafe {
            &mut *arch
                .value_ptr(loc.chunk as usize, column, loc.row as usize)
                .cast::<T>()
        })
    }

    /// Whether `entity` has `T`.
    pub fn has<T: Component>(&self, entity: Entity) -> bool {
        self.get::<T>(entity).is_some()
    }

    /// Adds or replaces a component by id, moving the value's bytes from `value`.
    /// False if the entity is not alive (the value is then not consumed).
    ///
    /// # Safety
    ///
    /// `value` points to a valid value of the component's type; on `true` the caller
    /// must not use or drop it again.
    pub unsafe fn insert_raw(&mut self, entity: Entity, id: ComponentId, value: *const u8) -> bool {
        let Some(loc) = self.entities.location(entity) else {
            return false;
        };
        let size = self.components.info(id).layout.size();
        let tick = self.bump_change_tick();
        let arch = &mut self.archetypes[loc.archetype as usize];
        if let Some(column) = arch.column_index(id) {
            let ptr = arch.value_ptr(loc.chunk as usize, column, loc.row as usize);
            // SAFETY: an initialized old value is replaced by the caller's value.
            unsafe {
                if let Some(drop) = arch.columns[column].drop {
                    drop(ptr);
                }
                core::ptr::copy_nonoverlapping(value, ptr, size);
            }
            arch.mark_column(loc.chunk as usize, column, tick);
            return true;
        }
        let target = self.add_target(loc.archetype, id);
        let new = move_row(
            &mut self.archetypes,
            &mut self.pool,
            &mut self.entities,
            entity,
            loc,
            target,
            tick,
        );
        let arch = &self.archetypes[target as usize];
        let column = arch.column_index(id).unwrap_or_default();
        // SAFETY: the slot is uninitialized room for the value.
        unsafe {
            core::ptr::copy_nonoverlapping(
                value,
                arch.value_ptr(new.chunk as usize, column, new.row as usize),
                size,
            );
        }
        true
    }

    /// Pointer to `entity`'s component `id`, valid until the next structural change.
    pub fn get_raw(&self, entity: Entity, id: ComponentId) -> Option<NonNull<u8>> {
        let loc = self.entities.location(entity)?;
        let arch = &self.archetypes[loc.archetype as usize];
        let column = arch.column_index(id)?;
        NonNull::new(arch.value_ptr(loc.chunk as usize, column, loc.row as usize))
    }

    /// Removes and drops component `id` from `entity`. False if absent.
    pub fn remove_raw(&mut self, entity: Entity, id: ComponentId) -> bool {
        let Some(loc) = self.entities.location(entity) else {
            return false;
        };
        let Some(column) = self.archetypes[loc.archetype as usize].column_index(id) else {
            return false;
        };
        let tick = self.bump_change_tick();
        let arch = &self.archetypes[loc.archetype as usize];
        if let Some(drop) = arch.columns[column].drop {
            // SAFETY: initialized; `move_row` forgets the slot.
            unsafe { drop(arch.value_ptr(loc.chunk as usize, column, loc.row as usize)) };
        }
        let target = self.remove_target(loc.archetype, id);
        move_row(
            &mut self.archetypes,
            &mut self.pool,
            &mut self.entities,
            entity,
            loc,
            target,
            tick,
        );
        true
    }

    /// Location of a live entity.
    #[allow(dead_code)]
    pub(crate) fn location(&self, entity: Entity) -> Option<Location> {
        self.entities.location(entity)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        for arch in &mut self.archetypes {
            arch.clear(&mut self.pool);
        }
    }
}
