//! Cached queries: typed access to the components of every matching entity.
//!
//! A [`QueryState`] remembers which archetypes match and keeps them in canonical order
//! (sorted by component names), so iteration order does not depend on the order in
//! which archetypes were created. Iteration walks chunks: each chunk gives contiguous
//! arrays per component, which loops vectorize. [`QueryState::for_each_chunk`] hands out
//! the arrays themselves.
//!
//! Writes are tracked per chunk and component: a query with `&mut T` stamps every chunk
//! it visits with a new change tick, and [`Changed<T>`] skips chunks not written since the
//! query's previous run. Gameplay code pays one store per chunk, not one per entity.

use crate::component::{Component, ComponentId};
use crate::entity::Entity;
use crate::storage::Archetype;
use crate::world::World;
use core::marker::PhantomData;
use core::ptr::NonNull;
use engine_core::jobs::ThreadPool;

/// What a query fetches per entity: `&T`, `&mut T`, [`Entity`], `Option<F>`, or tuples
/// of up to 12 of these.
///
/// # Safety
///
/// - `register` must record every component the fetch reads or writes, with `true` for
///   writes, so aliasing is caught when the query is created.
/// - `state` may only succeed for archetypes whose columns hold the fetched types.
/// - Items and slices must be safe to create on any thread (component types are
///   `Send + Sync`).
pub unsafe trait Fetch {
    /// Per-archetype state (column indices).
    type State: Copy + Send + Sync + 'static;
    /// Pointers into one chunk.
    type Ptr: Copy;
    /// What one entity yields.
    type Item<'w>;
    /// What one chunk yields: slices of equal length.
    type Slice<'w>;
    /// Whether any component is fetched mutably.
    const WRITES: bool;

    /// Registers the component types and records access.
    fn register(world: &mut World, access: &mut Vec<(ComponentId, bool)>);

    /// The per-archetype state, or `None` if the archetype does not match.
    fn state(world: &World, archetype: &Archetype) -> Option<Self::State>;

    /// Stamps the chunk columns this fetch writes.
    fn mark(state: Self::State, archetype: &mut Archetype, chunk: usize, tick: u64);

    /// Pointers into `chunk`.
    ///
    /// # Safety
    ///
    /// `state` comes from `state` for this archetype and `chunk` is in use.
    unsafe fn ptr(state: Self::State, archetype: &Archetype, chunk: usize) -> Self::Ptr;

    /// The item at `row`.
    ///
    /// # Safety
    ///
    /// `row` is below the chunk's length; items for the same row are not alive twice
    /// when the fetch writes; the world outlives `'w` without structural changes.
    unsafe fn item<'w>(ptr: Self::Ptr, row: usize) -> Self::Item<'w>;

    /// The chunk's arrays, `len` rows long.
    ///
    /// # Safety
    ///
    /// As for [`Fetch::item`], for all rows below `len`.
    unsafe fn slice<'w>(ptr: Self::Ptr, len: usize) -> Self::Slice<'w>;
}

/// Fetches that only read, so they can run on a shared `&World`.
///
/// # Safety
///
/// `WRITES` is false and `mark` does nothing.
pub unsafe trait ReadOnlyFetch: Fetch {}

// SAFETY: records a read of `T`; the column holds `T`s.
unsafe impl<T: Component> Fetch for &T {
    type State = usize;
    type Ptr = *const T;
    type Item<'w> = &'w T;
    type Slice<'w> = &'w [T];
    const WRITES: bool = false;

    fn register(world: &mut World, access: &mut Vec<(ComponentId, bool)>) {
        access.push((world.register::<T>(), false));
    }

    #[inline]
    fn state(world: &World, archetype: &Archetype) -> Option<usize> {
        archetype.column_index(world.components.id_of::<T>()?)
    }

    #[inline]
    fn mark(_: usize, _: &mut Archetype, _: usize, _: u64) {}

    #[inline]
    unsafe fn ptr(state: usize, archetype: &Archetype, chunk: usize) -> *const T {
        archetype.column_ptr(chunk, state).cast()
    }

    #[inline(always)]
    unsafe fn item<'w>(ptr: *const T, row: usize) -> &'w T {
        // SAFETY: guaranteed by the caller.
        unsafe { &*ptr.add(row) }
    }

    #[inline]
    unsafe fn slice<'w>(ptr: *const T, len: usize) -> &'w [T] {
        // SAFETY: guaranteed by the caller.
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }
}

// SAFETY: reads only.
unsafe impl<T: Component> ReadOnlyFetch for &T {}

// SAFETY: records a write of `T`; the column holds `T`s.
unsafe impl<T: Component> Fetch for &mut T {
    type State = usize;
    type Ptr = *mut T;
    type Item<'w> = &'w mut T;
    type Slice<'w> = &'w mut [T];
    const WRITES: bool = true;

    fn register(world: &mut World, access: &mut Vec<(ComponentId, bool)>) {
        access.push((world.register::<T>(), true));
    }

    #[inline]
    fn state(world: &World, archetype: &Archetype) -> Option<usize> {
        archetype.column_index(world.components.id_of::<T>()?)
    }

    #[inline]
    fn mark(state: usize, archetype: &mut Archetype, chunk: usize, tick: u64) {
        archetype.mark_column(chunk, state, tick);
    }

    #[inline]
    unsafe fn ptr(state: usize, archetype: &Archetype, chunk: usize) -> *mut T {
        archetype.column_ptr(chunk, state).cast()
    }

    #[inline(always)]
    unsafe fn item<'w>(ptr: *mut T, row: usize) -> &'w mut T {
        // SAFETY: guaranteed by the caller.
        unsafe { &mut *ptr.add(row) }
    }

    #[inline]
    unsafe fn slice<'w>(ptr: *mut T, len: usize) -> &'w mut [T] {
        // SAFETY: guaranteed by the caller.
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }
}

// SAFETY: the entity array is read only.
unsafe impl Fetch for Entity {
    type State = ();
    type Ptr = *const Entity;
    type Item<'w> = Entity;
    type Slice<'w> = &'w [Entity];
    const WRITES: bool = false;

    fn register(_: &mut World, _: &mut Vec<(ComponentId, bool)>) {}

    #[inline]
    fn state(_: &World, _: &Archetype) -> Option<()> {
        Some(())
    }

    #[inline]
    fn mark(_: (), _: &mut Archetype, _: usize, _: u64) {}

    #[inline]
    unsafe fn ptr(_: (), archetype: &Archetype, chunk: usize) -> *const Entity {
        archetype.entity_ptr(chunk, 0)
    }

    #[inline(always)]
    unsafe fn item<'w>(ptr: *const Entity, row: usize) -> Self::Item<'w> {
        // SAFETY: guaranteed by the caller.
        unsafe { *ptr.add(row) }
    }

    #[inline]
    unsafe fn slice<'w>(ptr: *const Entity, len: usize) -> &'w [Entity] {
        // SAFETY: guaranteed by the caller.
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }
}

// SAFETY: reads only.
unsafe impl ReadOnlyFetch for Entity {}

// SAFETY: delegates to `F` where the archetype has it.
unsafe impl<F: Fetch> Fetch for Option<F> {
    type State = Option<F::State>;
    type Ptr = Option<F::Ptr>;
    type Item<'w> = Option<F::Item<'w>>;
    type Slice<'w> = Option<F::Slice<'w>>;
    const WRITES: bool = F::WRITES;

    fn register(world: &mut World, access: &mut Vec<(ComponentId, bool)>) {
        F::register(world, access);
    }

    #[inline]
    fn state(world: &World, archetype: &Archetype) -> Option<Self::State> {
        Some(F::state(world, archetype))
    }

    #[inline]
    fn mark(state: Self::State, archetype: &mut Archetype, chunk: usize, tick: u64) {
        if let Some(state) = state {
            F::mark(state, archetype, chunk, tick);
        }
    }

    #[inline]
    unsafe fn ptr(state: Self::State, archetype: &Archetype, chunk: usize) -> Self::Ptr {
        // SAFETY: guaranteed by the caller.
        state.map(|s| unsafe { F::ptr(s, archetype, chunk) })
    }

    #[inline(always)]
    unsafe fn item<'w>(ptr: Self::Ptr, row: usize) -> Self::Item<'w> {
        // SAFETY: guaranteed by the caller.
        ptr.map(|p| unsafe { F::item(p, row) })
    }

    #[inline]
    unsafe fn slice<'w>(ptr: Self::Ptr, len: usize) -> Self::Slice<'w> {
        // SAFETY: guaranteed by the caller.
        ptr.map(|p| unsafe { F::slice(p, len) })
    }
}

// SAFETY: reads only when `F` does.
unsafe impl<F: ReadOnlyFetch> ReadOnlyFetch for Option<F> {}

macro_rules! fetch_tuple {
    ($($f:ident $i:tt),+) => {
        // SAFETY: each element upholds the contract; the tuple records all accesses.
        unsafe impl<$($f: Fetch),+> Fetch for ($($f,)+) {
            type State = ($($f::State,)+);
            type Ptr = ($($f::Ptr,)+);
            type Item<'w> = ($($f::Item<'w>,)+);
            type Slice<'w> = ($($f::Slice<'w>,)+);
            const WRITES: bool = $($f::WRITES)||+;

            fn register(world: &mut World, access: &mut Vec<(ComponentId, bool)>) {
                $($f::register(world, access);)+
            }

            #[inline]
            fn state(world: &World, archetype: &Archetype) -> Option<Self::State> {
                Some(($($f::state(world, archetype)?,)+))
            }

            #[inline]
            fn mark(state: Self::State, archetype: &mut Archetype, chunk: usize, tick: u64) {
                $($f::mark(state.$i, archetype, chunk, tick);)+
            }

            #[inline]
            unsafe fn ptr(state: Self::State, archetype: &Archetype, chunk: usize) -> Self::Ptr {
                // SAFETY: guaranteed by the caller.
                unsafe { ($($f::ptr(state.$i, archetype, chunk),)+) }
            }

            #[inline(always)]
            unsafe fn item<'w>(ptr: Self::Ptr, row: usize) -> Self::Item<'w> {
                // SAFETY: guaranteed by the caller.
                unsafe { ($($f::item(ptr.$i, row),)+) }
            }

            #[inline]
            unsafe fn slice<'w>(ptr: Self::Ptr, len: usize) -> Self::Slice<'w> {
                // SAFETY: guaranteed by the caller.
                unsafe { ($($f::slice(ptr.$i, len),)+) }
            }
        }

        // SAFETY: reads only when every element does.
        unsafe impl<$($f: ReadOnlyFetch),+> ReadOnlyFetch for ($($f,)+) {}
    };
}

fetch_tuple!(A 0);
fetch_tuple!(A 0, B 1);
fetch_tuple!(A 0, B 1, C 2);
fetch_tuple!(A 0, B 1, C 2, D 3);
fetch_tuple!(A 0, B 1, C 2, D 3, E 4);
fetch_tuple!(A 0, B 1, C 2, D 3, E 4, F 5);
fetch_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6);
fetch_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
fetch_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
fetch_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
fetch_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
fetch_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);

/// Which archetypes and chunks a query visits, beyond what it fetches.
pub trait Filter {
    /// Per-archetype state.
    type State: Copy + Send + Sync + 'static;
    /// Whether [`Filter::chunk`] must be evaluated.
    const CHUNK: bool;

    /// Registers the component types.
    fn register(world: &mut World);

    /// The per-archetype state, or `None` to skip the archetype.
    fn state(world: &World, archetype: &Archetype) -> Option<Self::State>;

    /// Whether to visit `chunk`; `last_run` is the query's previous change tick.
    fn chunk(state: Self::State, archetype: &Archetype, chunk: usize, last_run: u64) -> bool;
}

impl Filter for () {
    type State = ();
    const CHUNK: bool = false;

    fn register(_: &mut World) {}

    #[inline]
    fn state(_: &World, _: &Archetype) -> Option<()> {
        Some(())
    }

    #[inline]
    fn chunk(_: (), _: &Archetype, _: usize, _: u64) -> bool {
        true
    }
}

/// Only entities that have `T` (without fetching it).
pub struct With<T>(PhantomData<fn() -> T>);

impl<T: Component> Filter for With<T> {
    type State = ();
    const CHUNK: bool = false;

    fn register(world: &mut World) {
        world.register::<T>();
    }

    #[inline]
    fn state(world: &World, archetype: &Archetype) -> Option<()> {
        archetype
            .column_index(world.components.id_of::<T>()?)
            .map(|_| ())
    }

    #[inline]
    fn chunk(_: (), _: &Archetype, _: usize, _: u64) -> bool {
        true
    }
}

/// Only entities that do not have `T`.
pub struct Without<T>(PhantomData<fn() -> T>);

impl<T: Component> Filter for Without<T> {
    type State = ();
    const CHUNK: bool = false;

    fn register(world: &mut World) {
        world.register::<T>();
    }

    #[inline]
    fn state(world: &World, archetype: &Archetype) -> Option<()> {
        match world.components.id_of::<T>() {
            Some(id) if archetype.column_index(id).is_some() => None,
            _ => Some(()),
        }
    }

    #[inline]
    fn chunk(_: (), _: &Archetype, _: usize, _: u64) -> bool {
        true
    }
}

/// Only chunks whose `T` was written (or added) since the query's previous run.
///
/// Granularity is the chunk: unchanged entities that share a chunk with a changed one
/// are visited too. Use it for caching, replication, rendering sync and editors; game
/// logic that must stay deterministic across rollbacks should not depend on it, because
/// change ticks are not part of snapshots.
pub struct Changed<T>(PhantomData<fn() -> T>);

impl<T: Component> Filter for Changed<T> {
    type State = usize;
    const CHUNK: bool = true;

    fn register(world: &mut World) {
        world.register::<T>();
    }

    #[inline]
    fn state(world: &World, archetype: &Archetype) -> Option<usize> {
        archetype.column_index(world.components.id_of::<T>()?)
    }

    #[inline]
    fn chunk(state: usize, archetype: &Archetype, chunk: usize, last_run: u64) -> bool {
        archetype.versions[chunk * archetype.columns.len() + state] > last_run
    }
}

macro_rules! filter_tuple {
    ($($f:ident $i:tt),+) => {
        impl<$($f: Filter),+> Filter for ($($f,)+) {
            type State = ($($f::State,)+);
            const CHUNK: bool = $($f::CHUNK)||+;

            fn register(world: &mut World) {
                $($f::register(world);)+
            }

            #[inline]
            fn state(world: &World, archetype: &Archetype) -> Option<Self::State> {
                Some(($($f::state(world, archetype)?,)+))
            }

            #[inline]
            fn chunk(state: Self::State, archetype: &Archetype, chunk: usize, last_run: u64) -> bool {
                $($f::chunk(state.$i, archetype, chunk, last_run))&&+
            }
        }
    };
}

filter_tuple!(A 0);
filter_tuple!(A 0, B 1);
filter_tuple!(A 0, B 1, C 2);
filter_tuple!(A 0, B 1, C 2, D 3);

#[derive(Clone, Copy)]
struct Matched<S, FS> {
    archetype: u32,
    state: S,
    filter: FS,
}

/// A cached query: fetch `Q` from every entity that matches `Q` and filter `F`.
///
/// Create it once and keep it; iteration allocates nothing unless new archetypes
/// appeared since the last call.
///
/// ```
/// use engine_ecs::{bytemuck, pod_component, Changed, Entity, QueryState, World};
///
/// #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
/// #[repr(C)]
/// struct Health(i32);
/// pod_component!(Health);
///
/// let mut world = World::new();
/// let a = world.spawn((Health(10),));
/// let mut damaged = QueryState::<(Entity, &Health), Changed<Health>>::new(&mut world);
/// let mut seen = 0;
/// damaged.for_each(&mut world, |_| seen += 1);
/// assert_eq!(seen, 1); // the spawn counts as a change
/// seen = 0;
/// damaged.for_each(&mut world, |_| seen += 1);
/// assert_eq!(seen, 0);
/// world.get_mut::<Health>(a).unwrap().0 -= 3;
/// damaged.for_each(&mut world, |(e, h)| {
///     assert_eq!((e, h.0), (a, 7));
///     seen += 1;
/// });
/// assert_eq!(seen, 1);
/// ```
pub struct QueryState<Q: Fetch, F: Filter = ()> {
    world_id: u64,
    seen: usize,
    matched: Vec<Matched<Q::State, F::State>>,
    /// Archetype index → index into `matched`, `u32::MAX` when not matched.
    by_archetype: Vec<u32>,
    last_run: u64,
    par: Vec<(u32, u32)>,
    _marker: PhantomData<fn() -> (Q, F)>,
}

impl<Q: Fetch, F: Filter> core::fmt::Debug for QueryState<Q, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("QueryState")
            .field("archetypes", &self.matched.len())
            .field("last_run", &self.last_run)
            .finish()
    }
}

impl<Q: Fetch, F: Filter> QueryState<Q, F> {
    /// Creates the query for `world`, registering its component types.
    ///
    /// # Panics
    ///
    /// If `Q` fetches a component mutably together with any other access to it (a
    /// programming error, caught once here rather than on every iteration).
    pub fn new(world: &mut World) -> Self {
        let mut access = Vec::new();
        Q::register(world, &mut access);
        F::register(world);
        for (i, &(id, write)) in access.iter().enumerate() {
            for &(other, other_write) in &access[i + 1..] {
                assert!(
                    id != other || !(write || other_write),
                    "query {} accesses `{}` mutably more than once",
                    core::any::type_name::<Q>(),
                    world.components.name(id).unwrap_or("?"),
                );
            }
        }
        let mut state = Self {
            world_id: world.id(),
            seen: 0,
            matched: Vec::new(),
            by_archetype: Vec::new(),
            last_run: 0,
            par: Vec::new(),
            _marker: PhantomData,
        };
        state.update(world);
        state
    }

    fn update(&mut self, world: &World) {
        // A state from another world would index the wrong archetypes and columns.
        assert_eq!(
            self.world_id,
            world.id(),
            "query state used with another world"
        );
        if self.seen == world.archetypes.len() {
            return;
        }
        for index in self.seen..world.archetypes.len() {
            let archetype = &world.archetypes[index];
            if let (Some(state), Some(filter)) =
                (Q::state(world, archetype), F::state(world, archetype))
            {
                let at = self.matched.partition_point(|m| {
                    world.archetypes[m.archetype as usize].key < archetype.key
                });
                self.matched.insert(
                    at,
                    Matched {
                        archetype: index as u32,
                        state,
                        filter,
                    },
                );
            }
        }
        self.seen = world.archetypes.len();
        self.by_archetype.clear();
        self.by_archetype.resize(self.seen, u32::MAX);
        for (i, m) in self.matched.iter().enumerate() {
            self.by_archetype[m.archetype as usize] = i as u32;
        }
    }

    /// Number of matching archetypes.
    pub fn archetype_count(&mut self, world: &World) -> usize {
        self.update(world);
        self.matched.len()
    }

    /// Number of entities in matching archetypes (chunk filters are not applied).
    pub fn entity_count(&mut self, world: &World) -> usize {
        self.update(world);
        self.matched
            .iter()
            .map(|m| world.archetypes[m.archetype as usize].len())
            .sum()
    }

    #[inline]
    fn begin(&mut self, world: &mut World) -> (u64, u64) {
        self.update(world);
        let tick = if Q::WRITES {
            world.bump_change_tick()
        } else {
            world.change_tick
        };
        let last_run = self.last_run;
        self.last_run = tick;
        (tick, last_run)
    }

    /// Calls `f` for every matching entity, in canonical order.
    #[inline]
    pub fn for_each<'w>(&mut self, world: &'w mut World, mut f: impl FnMut(Q::Item<'w>)) {
        let (tick, last_run) = self.begin(world);
        for m in &self.matched {
            let archetype = &mut world.archetypes[m.archetype as usize];
            for chunk in 0..archetype.chunks.len() {
                if F::CHUNK && !F::chunk(m.filter, archetype, chunk, last_run) {
                    continue;
                }
                Q::mark(m.state, archetype, chunk, tick);
                let len = archetype.chunk_len(chunk);
                // SAFETY: the state belongs to this archetype; rows below `len` are
                // initialized; `&mut World` makes access exclusive, and the access check
                // in `new` rules out aliasing within an item; each row is visited once.
                unsafe {
                    let ptr = Q::ptr(m.state, archetype, chunk);
                    for row in 0..len {
                        f(Q::item(ptr, row));
                    }
                }
            }
        }
    }

    /// Calls `f` with each matching chunk's component arrays, in canonical order.
    #[inline]
    pub fn for_each_chunk<'w>(&mut self, world: &'w mut World, mut f: impl FnMut(Q::Slice<'w>)) {
        let (tick, last_run) = self.begin(world);
        for m in &self.matched {
            let archetype = &mut world.archetypes[m.archetype as usize];
            for chunk in 0..archetype.chunks.len() {
                if F::CHUNK && !F::chunk(m.filter, archetype, chunk, last_run) {
                    continue;
                }
                Q::mark(m.state, archetype, chunk, tick);
                let len = archetype.chunk_len(chunk);
                // SAFETY: as in `for_each`; each chunk is handed out once.
                unsafe { f(Q::slice(Q::ptr(m.state, archetype, chunk), len)) };
            }
        }
    }

    /// Like [`QueryState::for_each`], on a shared world, for read-only queries.
    #[inline]
    pub fn for_each_readonly<'w>(&mut self, world: &'w World, mut f: impl FnMut(Q::Item<'w>))
    where
        Q: ReadOnlyFetch,
    {
        self.update(world);
        let last_run = self.last_run;
        self.last_run = world.change_tick;
        for m in &self.matched {
            let archetype = &world.archetypes[m.archetype as usize];
            for chunk in 0..archetype.chunks.len() {
                if F::CHUNK && !F::chunk(m.filter, archetype, chunk, last_run) {
                    continue;
                }
                let len = archetype.chunk_len(chunk);
                // SAFETY: as in `for_each`; the fetch only reads.
                unsafe {
                    let ptr = Q::ptr(m.state, archetype, chunk);
                    for row in 0..len {
                        f(Q::item(ptr, row));
                    }
                }
            }
        }
    }

    /// Like [`QueryState::for_each_chunk`], on a shared world, for read-only queries.
    #[inline]
    pub fn for_each_chunk_readonly<'w>(&mut self, world: &'w World, mut f: impl FnMut(Q::Slice<'w>))
    where
        Q: ReadOnlyFetch,
    {
        self.update(world);
        let last_run = self.last_run;
        self.last_run = world.change_tick;
        for m in &self.matched {
            let archetype = &world.archetypes[m.archetype as usize];
            for chunk in 0..archetype.chunks.len() {
                if F::CHUNK && !F::chunk(m.filter, archetype, chunk, last_run) {
                    continue;
                }
                let len = archetype.chunk_len(chunk);
                // SAFETY: as in `for_each`; the fetch only reads.
                unsafe { f(Q::slice(Q::ptr(m.state, archetype, chunk), len)) };
            }
        }
    }

    /// Iterator over every matching entity, in canonical order.
    pub fn iter<'w, 's>(&'s mut self, world: &'w mut World) -> QueryIter<'w, 's, Q, F> {
        let (tick, last_run) = self.begin(world);
        QueryIter::new(&self.matched, &mut world.archetypes, tick, last_run)
    }

    /// Like [`QueryState::iter`], on a shared world, for read-only queries.
    pub fn iter_readonly<'w, 's>(&'s mut self, world: &'w World) -> QueryIter<'w, 's, Q, F>
    where
        Q: ReadOnlyFetch,
    {
        self.update(world);
        let last_run = self.last_run;
        self.last_run = world.change_tick;
        // Read-only fetches never write through the pointer (`mark` is not called
        // because `WRITES` is false).
        QueryIter {
            matched: &self.matched,
            archetypes: NonNull::from(&world.archetypes[..]).cast(),
            next: 0,
            current: None,
            chunk: 0,
            chunk_count: 0,
            row: 0,
            len: 0,
            ptr: None,
            tick: world.change_tick,
            last_run,
            _marker: PhantomData,
        }
    }

    /// The item of one entity, if it matches (chunk filters are not applied).
    pub fn get<'w>(&mut self, world: &'w mut World, entity: Entity) -> Option<Q::Item<'w>> {
        self.update(world);
        let loc = world.entities.location(entity)?;
        let index = *self.by_archetype.get(loc.archetype as usize)?;
        let m = *self.matched.get(index as usize)?;
        let tick = if Q::WRITES {
            world.bump_change_tick()
        } else {
            world.change_tick
        };
        let archetype = &mut world.archetypes[loc.archetype as usize];
        Q::mark(m.state, archetype, loc.chunk as usize, tick);
        // SAFETY: a live row of a matching archetype; `&mut World` makes it exclusive.
        Some(unsafe {
            Q::item(
                Q::ptr(m.state, archetype, loc.chunk as usize),
                loc.row as usize,
            )
        })
    }

    /// Like [`QueryState::get`], on a shared world, for read-only queries.
    pub fn get_readonly<'w>(&mut self, world: &'w World, entity: Entity) -> Option<Q::Item<'w>>
    where
        Q: ReadOnlyFetch,
    {
        self.update(world);
        let loc = world.entities.location(entity)?;
        let index = *self.by_archetype.get(loc.archetype as usize)?;
        let m = *self.matched.get(index as usize)?;
        let archetype = &world.archetypes[loc.archetype as usize];
        // SAFETY: a live row of a matching archetype; the fetch only reads.
        Some(unsafe {
            Q::item(
                Q::ptr(m.state, archetype, loc.chunk as usize),
                loc.row as usize,
            )
        })
    }

    fn collect_chunks(&mut self, world: &mut World) {
        let (tick, last_run) = self.begin(world);
        self.par.clear();
        for (index, m) in self.matched.iter().enumerate() {
            let archetype = &mut world.archetypes[m.archetype as usize];
            for chunk in 0..archetype.chunks.len() {
                if F::CHUNK && !F::chunk(m.filter, archetype, chunk, last_run) {
                    continue;
                }
                Q::mark(m.state, archetype, chunk, tick);
                self.par.push((index as u32, chunk as u32));
            }
        }
    }

    /// Calls `f` for every matching entity, chunks spread over `pool`'s workers.
    ///
    /// The result is deterministic when `f` only touches its own item (the usual
    /// case): each chunk is processed by exactly one worker.
    pub fn par_for_each(
        &mut self,
        world: &mut World,
        pool: &ThreadPool,
        f: impl for<'a> Fn(Q::Item<'a>) + Sync,
    ) {
        self.collect_chunks(world);
        let archetypes = &world.archetypes;
        let matched = &self.matched;
        let par = &self.par;
        pool.parallel_for(par.len(), 1, |_, range| {
            for &(index, chunk) in &par[range] {
                let m = matched[index as usize];
                let archetype = &archetypes[m.archetype as usize];
                let len = archetype.chunk_len(chunk as usize);
                // SAFETY: as in `for_each`; every chunk is handed to one job only, and
                // components are `Send + Sync`.
                unsafe {
                    let ptr = Q::ptr(m.state, archetype, chunk as usize);
                    for row in 0..len {
                        f(Q::item(ptr, row));
                    }
                }
            }
        });
    }

    /// Calls `f` with each matching chunk's arrays, spread over `pool`'s workers. `f`
    /// also receives the chunk's position in canonical order, for per-chunk outputs that
    /// are combined deterministically afterwards.
    pub fn par_for_each_chunk(
        &mut self,
        world: &mut World,
        pool: &ThreadPool,
        f: impl for<'a> Fn(usize, Q::Slice<'a>) + Sync,
    ) {
        self.collect_chunks(world);
        let archetypes = &world.archetypes;
        let matched = &self.matched;
        let par = &self.par;
        pool.parallel_for(par.len(), 1, |_, range| {
            for (position, &(index, chunk)) in range.clone().zip(&par[range]) {
                let m = matched[index as usize];
                let archetype = &archetypes[m.archetype as usize];
                let len = archetype.chunk_len(chunk as usize);
                // SAFETY: as in `par_for_each`.
                unsafe {
                    f(
                        position,
                        Q::slice(Q::ptr(m.state, archetype, chunk as usize), len),
                    )
                };
            }
        });
    }

    /// Number of chunks the last parallel call visited (the length of the position
    /// range `par_for_each_chunk` passes).
    pub fn last_parallel_chunks(&self) -> usize {
        self.par.len()
    }
}

/// Iterator returned by [`QueryState::iter`].
pub struct QueryIter<'w, 's, Q: Fetch, F: Filter> {
    matched: &'s [Matched<Q::State, F::State>],
    archetypes: NonNull<Archetype>,
    next: usize,
    current: Option<Matched<Q::State, F::State>>,
    chunk: usize,
    chunk_count: usize,
    row: usize,
    len: usize,
    ptr: Option<Q::Ptr>,
    tick: u64,
    last_run: u64,
    _marker: PhantomData<&'w mut World>,
}

impl<'w, 's, Q: Fetch, F: Filter> QueryIter<'w, 's, Q, F> {
    fn new(
        matched: &'s [Matched<Q::State, F::State>],
        archetypes: &'w mut [Archetype],
        tick: u64,
        last_run: u64,
    ) -> Self {
        Self {
            matched,
            archetypes: NonNull::from(archetypes).cast(),
            next: 0,
            current: None,
            chunk: 0,
            chunk_count: 0,
            row: 0,
            len: 0,
            ptr: None,
            tick,
            last_run,
            _marker: PhantomData,
        }
    }
}

impl<'w, Q: Fetch, F: Filter> Iterator for QueryIter<'w, '_, Q, F> {
    type Item = Q::Item<'w>;

    #[inline]
    fn next(&mut self) -> Option<Q::Item<'w>> {
        loop {
            if let Some(ptr) = self.ptr
                && self.row < self.len
            {
                let row = self.row;
                self.row += 1;
                // SAFETY: as in `QueryState::for_each`; each row is yielded once.
                return Some(unsafe { Q::item(ptr, row) });
            }
            self.ptr = None;
            let m = match self.current {
                Some(m) if self.chunk < self.chunk_count => m,
                _ => {
                    let m = *self.matched.get(self.next)?;
                    self.next += 1;
                    self.current = Some(m);
                    self.chunk = 0;
                    // SAFETY: matched archetype indices are in bounds; the world is
                    // borrowed for 'w.
                    self.chunk_count =
                        unsafe { &*self.archetypes.as_ptr().add(m.archetype as usize) }
                            .chunks
                            .len();
                    continue;
                }
            };
            let chunk = self.chunk;
            self.chunk += 1;
            // SAFETY: as above.
            let archetype = unsafe { &*self.archetypes.as_ptr().add(m.archetype as usize) };
            if F::CHUNK && !F::chunk(m.filter, archetype, chunk, self.last_run) {
                continue;
            }
            if Q::WRITES {
                // SAFETY: writing fetches only come from `iter`, which borrows the world
                // mutably for 'w; no other reference to the archetype is alive here.
                let archetype = unsafe { &mut *self.archetypes.as_ptr().add(m.archetype as usize) };
                Q::mark(m.state, archetype, chunk, self.tick);
            }
            self.len = archetype.chunk_len(chunk);
            self.row = 0;
            // SAFETY: the state belongs to this archetype and the chunk is in use.
            self.ptr = Some(unsafe { Q::ptr(m.state, archetype, chunk) });
        }
    }
}
