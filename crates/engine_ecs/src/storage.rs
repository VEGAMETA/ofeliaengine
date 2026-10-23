//! Archetypes stored in chunks.
//!
//! A chunk holds rows of one archetype as structure of arrays: the entity handles
//! first, then one array per component, each aligned to at least 16 bytes so loops over
//! them vectorize. Rows are dense across an archetype's chunks: every chunk but the last
//! is full, and removal moves the archetype's last row into the hole.
//!
//! Chunk sizes grow with the archetype: its first chunk is 16 KiB, the next two 64 KiB,
//! the next four 256 KiB, the rest 1 MiB ([`CHUNK_CLASSES`]). Small archetypes stay small,
//! and large ones get long arrays per component: iterating a million entities streams
//! memory about as fast as whole columns would (on ref-1, 100-byte rows: whole columns
//! 458 µs, 1 MiB chunks 473 µs, 256 KiB chunks 591 µs, 16 KiB chunks 1656 µs, because each
//! short array restarts the hardware prefetcher).
//!
//! In chunks of 64 KiB and more, column arrays start at staggered offsets within a 4 KiB
//! page. Otherwise two same-size columns can start a near multiple of 4 KiB apart, and
//! loads from one falsely depend on stores to the other ("4K aliasing"): on ref-1 that
//! made `p += v` over 24-byte rows 30–60 % slower in large chunks.
//!
//! Chunks come from a per-world pool of 256 KiB slabs, so spawning and despawning in
//! steady state never reach the global allocator. Each chunk carries one change version
//! per component column (see [`World::change_tick`](crate::World::change_tick)).

use crate::component::{ComponentId, Components};
use crate::entity::Entity;
use crate::fxhash::FastMap;
use core::alloc::Layout;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

/// Chunk sizes in bytes: an archetype's first chunk uses the first, the next two the
/// second, the next four the third, the rest the fourth.
pub const CHUNK_CLASSES: [usize; CLASSES] = [16 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024];
const CLASSES: usize = 4;
/// Classes from this one up stagger their column arrays (see the module docs).
const STAGGERED_FROM: usize = 1;
const PAGE: usize = 4096;
const SLAB_BYTES: usize = 256 * 1024;
const CHUNK_ALIGN: usize = 64;
/// Minimum alignment of a column inside a chunk.
const COLUMN_ALIGN: usize = 16;

/// Size class of the chunk at `index` in its archetype.
#[inline(always)]
const fn class_of(index: usize) -> usize {
    match index {
        0 => 0,
        1 | 2 => 1,
        3..=6 => 2,
        _ => 3,
    }
}

/// Pool of chunks. Freed chunks are kept for reuse until the world is dropped.
#[derive(Default)]
pub(crate) struct ChunkPool {
    free: [Vec<NonNull<u8>>; CLASSES],
    slabs: Vec<NonNull<u8>>,
    /// Chunks larger than a slab, allocated one by one (all of them, free or in use).
    large: Vec<(NonNull<u8>, usize)>,
}

fn layout(bytes: usize) -> Layout {
    match Layout::from_size_align(bytes, CHUNK_ALIGN) {
        Ok(layout) => layout,
        Err(_) => std::alloc::handle_alloc_error(Layout::new::<[u8; CHUNK_ALIGN]>()),
    }
}

fn raw_alloc(bytes: usize) -> NonNull<u8> {
    let layout = layout(bytes);
    // SAFETY: non-zero size (chunks are at least 16 KiB).
    let ptr = unsafe { std::alloc::alloc(layout) };
    match NonNull::new(ptr) {
        Some(ptr) => ptr,
        None => std::alloc::handle_alloc_error(layout),
    }
}

impl ChunkPool {
    pub(crate) fn alloc(&mut self, bytes: usize) -> NonNull<u8> {
        let Some(class) = CHUNK_CLASSES.iter().position(|&b| b == bytes) else {
            return raw_alloc(bytes);
        };
        if let Some(chunk) = self.free[class].pop() {
            return chunk;
        }
        if bytes > SLAB_BYTES {
            let chunk = raw_alloc(bytes);
            self.large.push((chunk, bytes));
            return chunk;
        }
        let slab = raw_alloc(SLAB_BYTES);
        self.slabs.push(slab);
        // Hand out the first chunk, keep the rest; reversed so chunks are used in
        // address order.
        for i in (1..SLAB_BYTES / bytes).rev() {
            // SAFETY: inside the slab.
            self.free[class].push(unsafe { slab.add(i * bytes) });
        }
        slab
    }

    pub(crate) fn release(&mut self, chunk: NonNull<u8>, bytes: usize) {
        match CHUNK_CLASSES.iter().position(|&b| b == bytes) {
            Some(class) => self.free[class].push(chunk),
            // SAFETY: allocated by `raw_alloc` with the same size.
            None => unsafe { std::alloc::dealloc(chunk.as_ptr(), layout(bytes)) },
        }
    }
}

impl Drop for ChunkPool {
    fn drop(&mut self) {
        for &slab in &self.slabs {
            // SAFETY: allocated in `alloc` with this layout.
            unsafe { std::alloc::dealloc(slab.as_ptr(), layout(SLAB_BYTES)) };
        }
        for &(chunk, bytes) in &self.large {
            // SAFETY: allocated in `alloc` with this size.
            unsafe { std::alloc::dealloc(chunk.as_ptr(), layout(bytes)) };
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Column {
    pub(crate) id: ComponentId,
    pub(crate) size: u32,
    pub(crate) drop: Option<crate::component::DropGlue>,
    pub(crate) pod: bool,
}

/// All entities with exactly one set of components.
pub struct Archetype {
    /// Component ids, sorted.
    pub(crate) ids: Box<[ComponentId]>,
    /// One per id, same order.
    pub(crate) columns: Box<[Column]>,
    /// Sorted name keys of the components: the canonical, history-independent order of
    /// archetypes.
    pub(crate) key: Box<[u64]>,
    /// Column indices sorted by component name key (hash order).
    pub(crate) canonical_columns: Box<[u16]>,
    /// Rows per chunk, per size class.
    capacities: [u32; CLASSES],
    /// Bytes per chunk, per size class.
    chunk_bytes: [u32; CLASSES],
    /// `class * columns.len() + column` → byte offset of the column's array.
    offsets: Box<[u32]>,
    pub(crate) chunks: Vec<NonNull<u8>>,
    pub(crate) len: u32,
    /// Rows in all chunks but the last (which are full).
    full_rows: u32,
    /// `chunk * columns.len() + column` → change tick of the last write.
    /// Atomic so systems running in parallel can stamp different columns of one
    /// archetype through a shared reference (relaxed stores: plain moves on x86).
    pub(crate) versions: Vec<AtomicU64>,
    pub(crate) add_edges: FastMap<u32, u32>,
    pub(crate) remove_edges: FastMap<u32, u32>,
}

// SAFETY: chunks are owned by the archetype; component types are `Send + Sync` (the
// `Component` bound, or the `register_dynamic` contract).
unsafe impl Send for Archetype {}
// SAFETY: as above; shared access only reads.
unsafe impl Sync for Archetype {}

const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

impl Archetype {
    pub(crate) fn new(ids: Box<[ComponentId]>, components: &Components) -> Self {
        let infos: Vec<_> = ids.iter().map(|&id| components.info(id)).collect();
        let entity_bytes = core::mem::size_of::<Entity>();
        let row_bytes: usize = entity_bytes + infos.iter().map(|i| i.layout.size()).sum::<usize>();
        let mut capacities = [0u32; CLASSES];
        let mut chunk_bytes = [0u32; CLASSES];
        let mut offsets = vec![0u32; CLASSES * ids.len()];
        // Column c starts at an offset ≡ c · PAGE / n (mod PAGE) in staggered classes.
        let stagger_step = PAGE / ids.len().max(1).next_power_of_two();
        for class in 0..CLASSES {
            let staggered = class >= STAGGERED_FROM;
            let slack =
                (ids.len() + 1) * CHUNK_ALIGN + if staggered { ids.len() * PAGE } else { 0 };
            let (capacity, bytes) = if row_bytes + slack <= CHUNK_CLASSES[class] {
                (
                    (CHUNK_CLASSES[class] - slack) / row_bytes,
                    CHUNK_CLASSES[class],
                )
            } else {
                // A row larger than the class: one row per chunk, allocated separately.
                (1, align_up(row_bytes + slack, CHUNK_ALIGN))
            };
            let mut offset = align_up(entity_bytes * capacity, COLUMN_ALIGN);
            for (column, info) in infos.iter().enumerate() {
                offset = align_up(offset, info.layout.align().max(COLUMN_ALIGN));
                if staggered {
                    let want = (column * stagger_step) % PAGE;
                    offset += (want + PAGE - offset % PAGE) % PAGE;
                    offset = align_up(offset, info.layout.align().max(COLUMN_ALIGN));
                }
                offsets[class * ids.len() + column] = offset as u32;
                offset += info.layout.size() * capacity;
            }
            debug_assert!(offset <= bytes);
            capacities[class] = capacity as u32;
            chunk_bytes[class] = bytes as u32;
        }
        let columns: Box<[Column]> = ids
            .iter()
            .zip(&infos)
            .map(|(&id, info)| Column {
                id,
                size: info.layout.size() as u32,
                drop: info.drop,
                pod: info.pod,
            })
            .collect();
        let mut key: Vec<u64> = infos.iter().map(|i| i.key).collect();
        key.sort_unstable();
        let mut canonical: Vec<u16> = (0..ids.len() as u16).collect();
        canonical.sort_unstable_by_key(|&c| infos[c as usize].key);
        Self {
            ids,
            columns,
            key: key.into_boxed_slice(),
            canonical_columns: canonical.into_boxed_slice(),
            capacities,
            chunk_bytes,
            offsets: offsets.into_boxed_slice(),
            chunks: Vec::new(),
            len: 0,
            full_rows: 0,
            versions: Vec::new(),
            add_edges: FastMap::default(),
            remove_edges: FastMap::default(),
        }
    }

    /// Number of entities.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the archetype has no entities.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Component ids, sorted.
    pub fn components(&self) -> &[ComponentId] {
        &self.ids
    }

    /// Row capacity of the chunk at `index` (chunks grow in size classes).
    pub fn chunk_capacity(&self, index: usize) -> usize {
        self.capacities[class_of(index)] as usize
    }

    /// Number of chunks in use.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Change tick of the last write to `component` in `chunk`, if the archetype has it.
    pub fn chunk_version(&self, chunk: usize, component: ComponentId) -> Option<u64> {
        let column = self.column_index(component)?;
        self.versions
            .get(chunk * self.columns.len() + column)
            .map(|v| v.load(Ordering::Relaxed))
    }

    /// Index of `id`'s column in this archetype, if it has the component.
    #[inline]
    pub fn column_index(&self, id: ComponentId) -> Option<usize> {
        // Archetypes have few components: a linear scan beats binary search below ~16.
        if self.ids.len() <= 16 {
            self.ids.iter().position(|&c| c == id)
        } else {
            self.ids.binary_search(&id).ok()
        }
    }

    /// Rows in use in the chunk at `chunk` (every chunk but the last is full).
    #[inline]
    pub fn chunk_len(&self, chunk: usize) -> usize {
        if chunk + 1 < self.chunks.len() {
            self.capacities[class_of(chunk)] as usize
        } else {
            (self.len - self.full_rows) as usize
        }
    }

    /// The entities of the chunk at `chunk`, in row order.
    ///
    /// # Panics
    ///
    /// If `chunk` is not below [`Self::chunk_count`].
    #[inline]
    pub fn chunk_entities(&self, chunk: usize) -> &[Entity] {
        let len = self.chunk_len(chunk);
        // SAFETY: the entity array starts the chunk and its first `len` rows are
        // initialized; `&self` keeps the archetype from changing while borrowed.
        unsafe { core::slice::from_raw_parts(self.entity_ptr(chunk, 0), len) }
    }

    /// The bytes of a plain-data column in one chunk (`chunk_len × size` bytes, row
    /// order): for code that reads components by layout (replication, serialization).
    /// `None` if the column is not plain data.
    ///
    /// # Panics
    ///
    /// If `chunk` or `column` is out of range.
    #[inline]
    pub fn pod_column(&self, chunk: usize, column: usize) -> Option<&[u8]> {
        let c = self.columns[column];
        if !c.pod {
            return None;
        }
        let len = self.chunk_len(chunk) * c.size as usize;
        // SAFETY: the chunk's column holds `chunk_len` initialized plain-data values, any
        // byte of which may be read; `&self` keeps them from changing while borrowed.
        Some(unsafe { core::slice::from_raw_parts(self.column_ptr(chunk, column), len) })
    }

    /// The first value of a column in one chunk (`chunk_len` values, the component's
    /// size apart, row order): for code that reads values that are not plain data
    /// through their reflection description (saves). Valid while the archetype is
    /// borrowed; reading through it is the caller's `unsafe`.
    ///
    /// # Panics
    ///
    /// If `chunk` or `column` is out of range.
    #[inline]
    pub fn column_base(&self, chunk: usize, column: usize) -> *const u8 {
        assert!(chunk < self.chunks.len() && column < self.columns.len());
        self.column_ptr(chunk, column)
    }

    #[inline]
    pub(crate) fn chunk_ptr(&self, chunk: usize) -> *mut u8 {
        self.chunks[chunk].as_ptr()
    }

    #[inline]
    pub(crate) fn column_ptr(&self, chunk: usize, column: usize) -> *mut u8 {
        let offset = self.offsets[class_of(chunk) * self.columns.len() + column] as usize;
        // SAFETY: the offset is inside the chunk by construction.
        unsafe { self.chunk_ptr(chunk).add(offset) }
    }

    #[inline]
    pub(crate) fn value_ptr(&self, chunk: usize, column: usize, row: usize) -> *mut u8 {
        let size = self.columns[column].size as usize;
        // SAFETY: `row < capacity`, so the value is inside the column's array.
        unsafe { self.column_ptr(chunk, column).add(row * size) }
    }

    #[inline]
    pub(crate) fn entity_ptr(&self, chunk: usize, row: usize) -> *mut Entity {
        // SAFETY: the entity array starts the chunk; `row < capacity`.
        unsafe { self.chunk_ptr(chunk).cast::<Entity>().add(row) }
    }

    #[inline]
    pub(crate) fn mark_chunk(&self, chunk: usize, tick: u64) {
        let n = self.columns.len();
        for v in &self.versions[chunk * n..(chunk + 1) * n] {
            v.store(tick, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(crate) fn mark_column(&self, chunk: usize, column: usize, tick: u64) {
        let n = self.columns.len();
        self.versions[chunk * n + column].store(tick, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn column_version(&self, chunk: usize, column: usize) -> u64 {
        self.versions[chunk * self.columns.len() + column].load(Ordering::Relaxed)
    }

    /// Appends a chunk of the class its position calls for.
    fn grow(&mut self, pool: &mut ChunkPool, tick: u64) {
        if let Some(last) = self.chunks.len().checked_sub(1) {
            self.full_rows += self.capacities[class_of(last)];
        }
        let bytes = self.chunk_bytes[class_of(self.chunks.len())] as usize;
        self.chunks.push(pool.alloc(bytes));
        self.versions
            .extend((0..self.columns.len()).map(|_| AtomicU64::new(tick)));
    }

    /// Removes the last chunk, which must be empty.
    fn shrink(&mut self, pool: &mut ChunkPool) {
        let index = self.chunks.len() - 1;
        if let Some(chunk) = self.chunks.pop() {
            pool.release(chunk, self.chunk_bytes[class_of(index)] as usize);
        }
        self.versions
            .truncate(self.chunks.len() * self.columns.len());
        if let Some(last) = self.chunks.len().checked_sub(1) {
            self.full_rows -= self.capacities[class_of(last)];
        }
    }

    /// Appends an uninitialized row for `entity`; the caller writes every column.
    #[inline]
    pub(crate) fn push_row(
        &mut self,
        pool: &mut ChunkPool,
        entity: Entity,
        tick: u64,
    ) -> (u32, u32) {
        let full = match self.chunks.len().checked_sub(1) {
            Some(last) => self.len - self.full_rows == self.capacities[class_of(last)],
            None => true,
        };
        if full {
            self.grow(pool, tick);
        }
        let chunk = self.chunks.len() - 1;
        let row = (self.len - self.full_rows) as usize;
        // SAFETY: `row < capacity` in a live chunk.
        unsafe { self.entity_ptr(chunk, row).write(entity) };
        self.len += 1;
        self.mark_chunk(chunk, tick);
        (chunk as u32, row as u32)
    }

    /// Fills the hole at `(chunk, row)` with the last row and shrinks by one. The caller
    /// has already moved out or dropped the hole's values. Returns the entity that moved
    /// into the hole, if any.
    #[inline]
    pub(crate) fn fill_hole(
        &mut self,
        pool: &mut ChunkPool,
        chunk: usize,
        row: usize,
        tick: u64,
    ) -> Option<Entity> {
        let last_chunk = self.chunks.len() - 1;
        let last_row = (self.len - self.full_rows - 1) as usize;
        let moved = if (chunk, row) != (last_chunk, last_row) {
            for column in 0..self.columns.len() {
                let size = self.columns[column].size as usize;
                // SAFETY: both rows are inside live chunks; distinct rows do not overlap.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        self.value_ptr(last_chunk, column, last_row),
                        self.value_ptr(chunk, column, row),
                        size,
                    );
                }
            }
            // SAFETY: as above.
            let entity = unsafe { self.entity_ptr(last_chunk, last_row).read() };
            // SAFETY: as above.
            unsafe { self.entity_ptr(chunk, row).write(entity) };
            self.mark_chunk(chunk, tick);
            Some(entity)
        } else {
            None
        };
        self.len -= 1;
        if last_row == 0 {
            self.shrink(pool);
        }
        moved
    }

    /// Sets the row count for a restore: allocates or frees chunks so `len` rows fit
    /// with every chunk but the last full, and stamps all chunks with `tick`. Row
    /// contents are left for the caller.
    pub(crate) fn resize_for_restore(&mut self, pool: &mut ChunkPool, len: u32, tick: u64) {
        let mut chunks = 0;
        let mut rows = 0u32;
        while rows < len {
            rows += self.capacities[class_of(chunks)];
            chunks += 1;
        }
        while self.chunks.len() < chunks {
            self.grow(pool, tick);
        }
        while self.chunks.len() > chunks {
            self.shrink(pool);
        }
        for v in &self.versions {
            v.store(tick, Ordering::Relaxed);
        }
        self.len = len;
    }

    /// Drops every value and returns the chunks to the pool.
    pub(crate) fn clear(&mut self, pool: &mut ChunkPool) {
        for column in 0..self.columns.len() {
            if let Some(drop) = self.columns[column].drop {
                for chunk in 0..self.chunks.len() {
                    for row in 0..self.chunk_len(chunk) {
                        // SAFETY: rows below the chunk's length are initialized.
                        unsafe { drop.drop_value(self.value_ptr(chunk, column, row)) };
                    }
                }
            }
        }
        self.len = 0;
        while !self.chunks.is_empty() {
            self.shrink(pool);
        }
        debug_assert_eq!(self.full_rows, 0);
    }

    /// Whether every component is plain data.
    pub(crate) fn all_pod(&self) -> bool {
        self.columns.iter().all(|c| c.pod)
    }
}
