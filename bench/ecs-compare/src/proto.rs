//! A minimal archetype ECS, written to measure the storage model the engine would own
//! (ADR 0017) against the candidates. Not engine code: no scheduling, change detection,
//! observers or reflection; queries are closures over typed column slices.
//!
//! - Entities: generational indices into a dense metadata array.
//! - Archetypes: one per sorted component set, one type-erased column per component
//!   (structure of arrays), rows swap-removed.
//! - Structural changes follow cached archetype-graph edges (add / remove one component).

use std::alloc::{Layout, alloc, dealloc, realloc};
use std::any::TypeId;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::ptr::NonNull;

/// Multiply-rotate hashing (FxHash's scheme) for small integer and TypeId keys; the
/// standard SipHash is DoS-resistant and several times slower, which ECS maps don't need.
#[derive(Default)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.write_u64(u64::from(v));
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Entity {
    index: u32,
    generation: u32,
}

#[derive(Clone, Copy)]
struct ComponentInfo {
    layout: Layout,
    drop: Option<unsafe fn(*mut u8)>,
}

unsafe fn drop_as<T>(ptr: *mut u8) {
    // SAFETY: the caller passes a valid, initialized `T`.
    unsafe { std::ptr::drop_in_place(ptr.cast::<T>()) }
}

/// A type-erased growable array; its length is the archetype's row count.
struct Column {
    data: NonNull<u8>,
    info: ComponentInfo,
    capacity: usize,
}

impl Column {
    fn new(info: ComponentInfo) -> Self {
        Self {
            data: NonNull::new(info.layout.align() as *mut u8).unwrap(),
            info,
            capacity: if info.layout.size() == 0 {
                usize::MAX
            } else {
                0
            },
        }
    }

    fn reserve(&mut self, len: usize, additional: usize) {
        let needed = len + additional;
        if needed <= self.capacity {
            return;
        }
        let new_capacity = needed.max(self.capacity * 2).max(64);
        let size = self.info.layout.size();
        let new_layout =
            Layout::from_size_align(size * new_capacity, self.info.layout.align()).unwrap();
        // SAFETY: non-zero size (zero-sized columns never grow); the old allocation, if
        // any, was made with the same alignment and `size * capacity` bytes.
        let ptr = unsafe {
            if self.capacity == 0 {
                alloc(new_layout)
            } else {
                let old =
                    Layout::from_size_align(size * self.capacity, new_layout.align()).unwrap();
                realloc(self.data.as_ptr(), old, new_layout.size())
            }
        };
        self.data = NonNull::new(ptr).expect("out of memory");
        self.capacity = new_capacity;
    }

    fn ptr(&self, row: usize) -> *mut u8 {
        // SAFETY: callers pass rows within capacity.
        unsafe { self.data.as_ptr().add(row * self.info.layout.size()) }
    }

    /// Moves `row` out to `dst` (if any) and moves the last row (`len - 1`) into its place.
    unsafe fn swap_remove(&mut self, row: usize, len: usize, dst: Option<*mut u8>, drop_it: bool) {
        let size = self.info.layout.size();
        let at = self.ptr(row);
        // SAFETY: `row < len`; the caller guarantees initialized rows `..len`.
        unsafe {
            if let Some(dst) = dst {
                std::ptr::copy_nonoverlapping(at, dst, size);
            } else if drop_it && let Some(drop) = self.info.drop {
                drop(at);
            }
            let last = len - 1;
            if row != last {
                std::ptr::copy_nonoverlapping(self.ptr(last), at, size);
            }
        }
    }

    unsafe fn drop_all(&mut self, len: usize) {
        if let Some(drop) = self.info.drop {
            for row in 0..len {
                // SAFETY: rows `..len` are initialized.
                unsafe { drop(self.ptr(row)) };
            }
        }
        if self.info.layout.size() > 0 && self.capacity > 0 {
            let layout = Layout::from_size_align(
                self.info.layout.size() * self.capacity,
                self.info.layout.align(),
            )
            .unwrap();
            // SAFETY: allocated with this layout in `reserve`.
            unsafe { dealloc(self.data.as_ptr(), layout) };
        }
    }
}

struct Archetype {
    ids: Box<[u32]>,
    columns: Vec<Column>,
    entities: Vec<Entity>,
    add_edges: FastMap<u32, u32>,
    remove_edges: FastMap<u32, u32>,
}

impl Archetype {
    fn column_of(&self, id: u32) -> Option<usize> {
        self.ids.binary_search(&id).ok()
    }
}

#[derive(Clone, Copy)]
struct Meta {
    generation: u32,
    archetype: u32,
    row: u32,
}

pub struct World {
    infos: Vec<ComponentInfo>,
    type_ids: FastMap<TypeId, u32>,
    bundles: FastMap<TypeId, (u32, [usize; 8])>,
    archetypes: Vec<Archetype>,
    by_ids: FastMap<Box<[u32]>, u32>,
    metas: Vec<Meta>,
    free: Vec<u32>,
    scratch: Vec<u32>,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    pub fn new() -> Self {
        let mut world = Self {
            infos: Vec::new(),
            type_ids: FastMap::default(),
            bundles: FastMap::default(),
            archetypes: Vec::new(),
            by_ids: FastMap::default(),
            metas: Vec::new(),
            free: Vec::new(),
            scratch: Vec::new(),
        };
        world.archetype_for(Box::new([]));
        world
    }

    pub fn component_id<T: 'static>(&mut self) -> u32 {
        let next = self.infos.len() as u32;
        let id = *self.type_ids.entry(TypeId::of::<T>()).or_insert(next);
        if id == next {
            self.infos.push(ComponentInfo {
                layout: Layout::new::<T>(),
                drop: std::mem::needs_drop::<T>().then_some(drop_as::<T> as unsafe fn(*mut u8)),
            });
        }
        id
    }

    fn existing_id<T: 'static>(&self) -> Option<u32> {
        self.type_ids.get(&TypeId::of::<T>()).copied()
    }

    fn archetype_for(&mut self, ids: Box<[u32]>) -> u32 {
        if let Some(&a) = self.by_ids.get(&ids) {
            return a;
        }
        let columns = ids
            .iter()
            .map(|&id| Column::new(self.infos[id as usize]))
            .collect();
        let index = self.archetypes.len() as u32;
        self.archetypes.push(Archetype {
            ids: ids.clone(),
            columns,
            entities: Vec::new(),
            add_edges: FastMap::default(),
            remove_edges: FastMap::default(),
        });
        self.by_ids.insert(ids, index);
        index
    }

    fn alloc_entity(&mut self, archetype: u32, row: u32) -> Entity {
        if let Some(index) = self.free.pop() {
            let meta = &mut self.metas[index as usize];
            meta.archetype = archetype;
            meta.row = row;
            Entity {
                index,
                generation: meta.generation,
            }
        } else {
            self.metas.push(Meta {
                generation: 0,
                archetype,
                row,
            });
            Entity {
                index: (self.metas.len() - 1) as u32,
                generation: 0,
            }
        }
    }

    fn meta(&self, entity: Entity) -> Option<Meta> {
        let meta = *self.metas.get(entity.index as usize)?;
        (meta.generation == entity.generation).then_some(meta)
    }

    /// The archetype of bundle `B` and, per tuple position, its column there (cached per
    /// bundle type).
    fn bundle_layout<B: Bundle + 'static>(&mut self) -> (u32, [usize; 8]) {
        if let Some(&layout) = self.bundles.get(&TypeId::of::<B>()) {
            return layout;
        }
        let mut ids = [0u32; 8];
        B::ids(self, &mut ids);
        let mut sorted: Vec<u32> = ids[..B::COUNT].to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), B::COUNT, "a bundle repeats a component type");
        let archetype = self.archetype_for(sorted.into_boxed_slice());
        let arch = &self.archetypes[archetype as usize];
        let mut columns = [0usize; 8];
        for (column, id) in columns.iter_mut().zip(&ids[..B::COUNT]) {
            *column = arch.column_of(*id).unwrap();
        }
        self.bundles.insert(TypeId::of::<B>(), (archetype, columns));
        (archetype, columns)
    }

    pub fn spawn<B: Bundle + 'static>(&mut self, bundle: B) -> Entity {
        let (archetype, columns) = self.bundle_layout::<B>();
        self.spawn_in(archetype, &columns, bundle)
    }

    pub fn spawn_batch<B: Bundle + 'static>(&mut self, bundles: impl ExactSizeIterator<Item = B>) {
        let (archetype, columns) = self.bundle_layout::<B>();
        let arch = &mut self.archetypes[archetype as usize];
        let len = arch.entities.len();
        for column in &mut arch.columns {
            column.reserve(len, bundles.len());
        }
        arch.entities.reserve(bundles.len());
        self.metas.reserve(bundles.len());
        for bundle in bundles {
            self.spawn_in(archetype, &columns, bundle);
        }
    }

    fn spawn_in<B: Bundle>(&mut self, archetype: u32, columns: &[usize; 8], bundle: B) -> Entity {
        let row = self.archetypes[archetype as usize].entities.len();
        let entity = self.alloc_entity(archetype, row as u32);
        let arch = &mut self.archetypes[archetype as usize];
        let mut dst = [std::ptr::null_mut(); 8];
        for (i, &c) in columns[..B::COUNT].iter().enumerate() {
            arch.columns[c].reserve(row, 1);
            dst[i] = arch.columns[c].ptr(row);
        }
        // SAFETY: each destination is room for its tuple position's type in this row.
        unsafe { bundle.write(&dst) };
        arch.entities.push(entity);
        entity
    }

    pub fn despawn(&mut self, entity: Entity) -> bool {
        let Some(meta) = self.meta(entity) else {
            return false;
        };
        let arch = &mut self.archetypes[meta.archetype as usize];
        let len = arch.entities.len();
        for column in &mut arch.columns {
            // SAFETY: `meta.row < len`, rows initialized.
            unsafe { column.swap_remove(meta.row as usize, len, None, true) };
        }
        arch.entities.swap_remove(meta.row as usize);
        if let Some(&moved) = arch.entities.get(meta.row as usize) {
            self.metas[moved.index as usize].row = meta.row;
        }
        let m = &mut self.metas[entity.index as usize];
        m.generation = m.generation.wrapping_add(1);
        self.free.push(entity.index);
        true
    }

    pub fn get<T: 'static>(&self, entity: Entity) -> Option<&T> {
        let meta = self.meta(entity)?;
        let id = self.existing_id::<T>()?;
        let arch = &self.archetypes[meta.archetype as usize];
        let column = arch.column_of(id)?;
        // SAFETY: the column holds `T`s; the row is initialized.
        Some(unsafe { &*arch.columns[column].ptr(meta.row as usize).cast::<T>() })
    }

    pub fn insert<T: 'static>(&mut self, entity: Entity, value: T) -> bool {
        let Some(meta) = self.meta(entity) else {
            return false;
        };
        let id = self.component_id::<T>();
        let source = meta.archetype;
        if let Some(column) = self.archetypes[source as usize].column_of(id) {
            let arch = &mut self.archetypes[source as usize];
            // SAFETY: the column holds `T`s; the row is initialized.
            unsafe { *arch.columns[column].ptr(meta.row as usize).cast::<T>() = value };
            return true;
        }
        let target = match self.archetypes[source as usize].add_edges.get(&id) {
            Some(&t) => t,
            None => {
                self.scratch.clear();
                self.scratch
                    .extend_from_slice(&self.archetypes[source as usize].ids);
                let at = self.scratch.binary_search(&id).unwrap_err();
                self.scratch.insert(at, id);
                let t = self.archetype_for(self.scratch.clone().into_boxed_slice());
                self.archetypes[source as usize].add_edges.insert(id, t);
                t
            }
        };
        let new_row = self.move_row(entity, meta, target, Some(id));
        let arch = &mut self.archetypes[target as usize];
        let column = arch.column_of(id).unwrap();
        // SAFETY: `move_row` reserved the row and left this column's slot for the value.
        unsafe { arch.columns[column].ptr(new_row).cast::<T>().write(value) };
        true
    }

    pub fn remove<T: 'static>(&mut self, entity: Entity) -> Option<T> {
        let meta = self.meta(entity)?;
        let id = self.existing_id::<T>()?;
        let source = meta.archetype;
        let column = self.archetypes[source as usize].column_of(id)?;
        // SAFETY: the column holds `T`s; the row is initialized; `move_row` forgets it.
        let value = unsafe {
            self.archetypes[source as usize].columns[column]
                .ptr(meta.row as usize)
                .cast::<T>()
                .read()
        };
        let target = match self.archetypes[source as usize].remove_edges.get(&id) {
            Some(&t) => t,
            None => {
                self.scratch.clear();
                self.scratch.extend(
                    self.archetypes[source as usize]
                        .ids
                        .iter()
                        .copied()
                        .filter(|&c| c != id),
                );
                let t = self.archetype_for(self.scratch.clone().into_boxed_slice());
                self.archetypes[source as usize].remove_edges.insert(id, t);
                t
            }
        };
        self.move_row(entity, meta, target, None);
        Some(value)
    }

    /// Moves `entity`'s row to `target`. Components missing in `target` are forgotten
    /// (already read out by `remove`); `skip` is a target column left for the caller.
    fn move_row(&mut self, entity: Entity, meta: Meta, target: u32, skip: Option<u32>) -> usize {
        let (src, dst) = if meta.archetype < target {
            let (a, b) = self.archetypes.split_at_mut(target as usize);
            (&mut a[meta.archetype as usize], &mut b[0])
        } else {
            let (a, b) = self.archetypes.split_at_mut(meta.archetype as usize);
            (&mut b[0], &mut a[target as usize])
        };
        let new_row = dst.entities.len();
        let src_len = src.entities.len();
        for (column, &id) in dst.columns.iter_mut().zip(dst.ids.iter()) {
            column.reserve(new_row, 1);
            if Some(id) == skip {
                continue;
            }
            let from = src.column_of(id).unwrap();
            // SAFETY: row initialized in the source, room reserved in the target.
            unsafe {
                src.columns[from].swap_remove(
                    meta.row as usize,
                    src_len,
                    Some(column.ptr(new_row)),
                    false,
                );
            }
        }
        // Source columns not in the target: the value was taken by `remove`; close the gap.
        for (column, &id) in src.columns.iter_mut().zip(src.ids.iter()) {
            if dst.column_of(id).is_none() {
                // SAFETY: as above; nothing to drop (moved out already).
                unsafe { column.swap_remove(meta.row as usize, src_len, None, false) };
            }
        }
        src.entities.swap_remove(meta.row as usize);
        let moved = src.entities.get(meta.row as usize).copied();
        dst.entities.push(entity);
        if let Some(moved) = moved {
            self.metas[moved.index as usize].row = meta.row;
        }
        let m = &mut self.metas[entity.index as usize];
        m.archetype = target;
        m.row = new_row as u32;
        new_row
    }

    /// Calls `f` for every entity with `A` (mutable).
    pub fn each_mut<A: 'static>(&mut self, mut f: impl FnMut(&mut A)) {
        let Some(a) = self.existing_id::<A>() else {
            return;
        };
        for arch in &mut self.archetypes {
            let Some(ca) = arch.column_of(a) else {
                continue;
            };
            let len = arch.entities.len();
            // SAFETY: the column holds `len` initialized `A`s.
            let sa =
                unsafe { std::slice::from_raw_parts_mut(arch.columns[ca].ptr(0).cast::<A>(), len) };
            for x in sa {
                f(x);
            }
        }
    }

    /// Calls `f` for every entity with `A` (mutable) and `B`.
    pub fn each_mut2<A: 'static, B: 'static>(&mut self, mut f: impl FnMut(&mut A, &B)) {
        assert_ne!(TypeId::of::<A>(), TypeId::of::<B>());
        let (Some(a), Some(b)) = (self.existing_id::<A>(), self.existing_id::<B>()) else {
            return;
        };
        for arch in &mut self.archetypes {
            let (Some(ca), Some(cb)) = (arch.column_of(a), arch.column_of(b)) else {
                continue;
            };
            let len = arch.entities.len();
            // SAFETY: distinct columns (A ≠ B) of `len` initialized values.
            let (sa, sb) = unsafe {
                (
                    std::slice::from_raw_parts_mut(arch.columns[ca].ptr(0).cast::<A>(), len),
                    std::slice::from_raw_parts(arch.columns[cb].ptr(0).cast::<B>(), len),
                )
            };
            for (x, y) in sa.iter_mut().zip(sb) {
                f(x, y);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.archetypes.iter().map(|a| a.entities.len()).sum()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        for arch in &mut self.archetypes {
            let len = arch.entities.len();
            for column in &mut arch.columns {
                // SAFETY: rows `..len` are initialized; the world is going away.
                unsafe { column.drop_all(len) };
            }
        }
    }
}

/// A tuple of up to eight components spawned together.
///
/// # Safety
///
/// `write` must initialize exactly one value at each destination, of the type whose id
/// `ids` put at the same position.
pub unsafe trait Bundle {
    const COUNT: usize;
    /// Component ids in tuple order.
    fn ids(world: &mut World, out: &mut [u32; 8]);
    /// # Safety
    ///
    /// `dst[i]` points to uninitialized room for the tuple's `i`-th component.
    unsafe fn write(self, dst: &[*mut u8; 8]);
}

macro_rules! bundle {
    ($count:expr; $($t:ident $v:ident $i:tt),+) => {
        // SAFETY: writes each component to the destination of its tuple position.
        unsafe impl<$($t: 'static),+> Bundle for ($($t,)+) {
            const COUNT: usize = $count;
            fn ids(world: &mut World, out: &mut [u32; 8]) {
                $(out[$i] = world.component_id::<$t>();)+
            }
            unsafe fn write(self, dst: &[*mut u8; 8]) {
                let ($($v,)+) = self;
                // SAFETY: guaranteed by the caller.
                unsafe { $(dst[$i].cast::<$t>().write($v);)+ }
            }
        }
    };
}

bundle!(1; A a 0);
bundle!(2; A a 0, B b 1);
bundle!(3; A a 0, B b 1, C c 2);
bundle!(4; A a 0, B b 1, C c 2, D d 3);
bundle!(5; A a 0, B b 1, C c 2, D d 3, E e 4);

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[derive(Debug, PartialEq)]
    struct A(u32);
    #[derive(Debug, PartialEq)]
    struct B(f32);
    struct Tracked(#[allow(dead_code)] Rc<()>);

    #[test]
    fn spawn_insert_remove_despawn() {
        let mut w = World::new();
        let es: Vec<Entity> = (0..100).map(|i| w.spawn((A(i), B(i as f32)))).collect();
        for (i, &e) in es.iter().enumerate() {
            assert_eq!(w.get::<A>(e), Some(&A(i as u32)));
        }
        let rc = Rc::new(());
        for &e in es.iter().step_by(3) {
            assert!(w.insert(e, Tracked(rc.clone())));
        }
        assert_eq!(Rc::strong_count(&rc), 35);
        for &e in es.iter().step_by(6) {
            assert!(w.remove::<Tracked>(e).is_some());
        }
        assert_eq!(Rc::strong_count(&rc), 18);
        for (i, &e) in es.iter().enumerate() {
            assert_eq!(w.get::<A>(e), Some(&A(i as u32)), "{i}");
            assert_eq!(w.get::<B>(e), Some(&B(i as f32)));
        }
        let mut sum = 0;
        w.each_mut2::<A, B>(|a, _| sum += a.0);
        assert_eq!(sum, (0..100).sum::<u32>());
        for &e in &es[..50] {
            assert!(w.despawn(e));
            assert!(!w.despawn(e));
        }
        assert_eq!(w.len(), 50);
        for (i, &e) in es.iter().enumerate().skip(50) {
            assert_eq!(w.get::<A>(e), Some(&A(i as u32)));
        }
        drop(w);
        assert_eq!(Rc::strong_count(&rc), 1);
    }
}
