//! Frame arena: a bump allocator reset once per frame (CLAUDE.md §5: per-frame data lives
//! in arenas reset once per frame).
//!
//! Allocation is a pointer bump in the current chunk. When a chunk is full, the next one
//! is used, and a new one is created only if the arena has never needed that many
//! before. [`Arena::reset`] rewinds to the first chunk and keeps them all, so once the
//! arena has seen a frame's peak usage, later frames touch no heap.
//!
//! Values are never dropped by the arena. Owners of values with destructors (for example
//! closures that capture owned data) drop them in place before resetting.

use core::alloc::Layout;
use core::cell::{Cell, UnsafeCell};
use core::ptr::NonNull;

/// Alignment of every chunk; larger alignments are handled by padding inside a chunk.
const CHUNK_ALIGN: usize = 64;

struct Chunk {
    base: NonNull<u8>,
    size: usize,
}

/// A bump allocator. Allocating needs only `&self`; resetting needs `&mut self`, so no
/// reference into the arena can outlive a reset.
pub struct Arena {
    chunks: UnsafeCell<Vec<Chunk>>,
    current: Cell<usize>,
    offset: Cell<usize>,
    chunk_size: usize,
}

impl core::fmt::Debug for Arena {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Arena")
            .field("chunk_size", &self.chunk_size)
            .field("capacity", &self.capacity())
            .finish()
    }
}

// SAFETY: the arena owns its chunks; moving it to another thread moves that ownership.
// It is not `Sync`: `&self` allocation mutates through cells.
unsafe impl Send for Arena {}

impl Arena {
    /// An empty arena that grows in chunks of `chunk_size` bytes (larger requests get a
    /// chunk of their own size).
    pub fn new(chunk_size: usize) -> Self {
        Self {
            chunks: UnsafeCell::new(Vec::new()),
            current: Cell::new(0),
            offset: Cell::new(0),
            chunk_size: chunk_size.max(CHUNK_ALIGN),
        }
    }

    /// Moves `value` into the arena. It lives until the next [`Self::reset`] and is not
    /// dropped by the arena.
    #[allow(clippy::mut_from_ref)] // Each call returns a distinct, fresh allocation.
    pub fn alloc<T>(&self, value: T) -> &mut T {
        let ptr = self.alloc_layout(Layout::new::<T>()).cast::<T>();
        // SAFETY: fresh, properly aligned memory for a `T`, not aliased by any other
        // reference; it stays valid until `reset`, which needs `&mut self`.
        unsafe {
            ptr.as_ptr().write(value);
            &mut *ptr.as_ptr()
        }
    }

    /// Raw memory for `layout`, valid until the next [`Self::reset`].
    pub fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
        // SAFETY: no reference into the chunk list escapes this function, and the arena
        // is not `Sync`, so nothing else touches the list during the call.
        let chunks = unsafe { &mut *self.chunks.get() };
        loop {
            let index = self.current.get();
            if let Some(chunk) = chunks.get(index) {
                let start = (chunk.base.as_ptr() as usize + self.offset.get())
                    .next_multiple_of(layout.align());
                let end = start + layout.size();
                if end <= chunk.base.as_ptr() as usize + chunk.size {
                    self.offset.set(end - chunk.base.as_ptr() as usize);
                    // SAFETY: `start` lies inside the chunk (checked above) and is non-null.
                    return unsafe { NonNull::new_unchecked(start as *mut u8) };
                }
                // Chunks after the current one are unused this frame: take the first that
                // is large enough (moving it next in line), or make one.
                let needed = layout.size() + layout.align();
                if let Some(found) = (index + 1..chunks.len()).find(|&j| chunks[j].size >= needed) {
                    chunks.swap(index + 1, found);
                    self.current.set(index + 1);
                    self.offset.set(0);
                    continue;
                }
            }
            let size = self.chunk_size.max(layout.size() + layout.align());
            let chunk_layout = Layout::from_size_align(size, CHUNK_ALIGN)
                .unwrap_or_else(|_| Layout::new::<[u8; CHUNK_ALIGN]>());
            // SAFETY: non-zero size.
            let base = unsafe { std::alloc::alloc(chunk_layout) };
            let Some(base) = NonNull::new(base) else {
                std::alloc::handle_alloc_error(chunk_layout);
            };
            // Insert after the current chunk so that the chunk order stays the order of
            // use; a large one-off chunk is reused by later frames like any other.
            let insert_at = if chunks.is_empty() { 0 } else { index + 1 };
            chunks.insert(insert_at, Chunk { base, size });
            self.current.set(insert_at);
            self.offset.set(0);
        }
    }

    /// Rewinds to the start, keeping every chunk for reuse.
    pub fn reset(&mut self) {
        self.current.set(0);
        self.offset.set(0);
    }

    /// Bytes reserved in chunks.
    pub fn capacity(&self) -> usize {
        // SAFETY: shared read; `alloc_layout` does not hold references across calls.
        unsafe { &*self.chunks.get() }.iter().map(|c| c.size).sum()
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        for chunk in self.chunks.get_mut().drain(..) {
            // SAFETY: allocated in `alloc_layout` with this size and alignment.
            unsafe {
                std::alloc::dealloc(
                    chunk.base.as_ptr(),
                    Layout::from_size_align_unchecked(chunk.size, CHUNK_ALIGN),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_are_aligned_distinct_and_survive_chunk_growth() {
        let mut arena = Arena::new(256);
        let mut pointers = Vec::new();
        for i in 0..1000u64 {
            let value = arena.alloc(i);
            assert_eq!(*value, i);
            pointers.push(value as *const u64 as usize);
            let wide = arena.alloc([7u8; 100]);
            assert_eq!(wide[99], 7);
            #[repr(align(128))]
            struct Aligned(#[allow(dead_code)] u8);
            let aligned = arena.alloc(Aligned(1));
            assert_eq!(aligned as *const Aligned as usize % 128, 0);
        }
        pointers.sort_unstable();
        pointers.dedup();
        assert_eq!(pointers.len(), 1000);
        let capacity = arena.capacity();
        arena.reset();
        // The same workload fits in the kept chunks.
        for i in 0..1000u64 {
            arena.alloc(i);
            arena.alloc([7u8; 100]);
        }
        assert_eq!(arena.capacity(), capacity);
    }

    #[test]
    fn oversized_requests_get_their_own_chunk_and_are_reused() {
        let mut arena = Arena::new(128);
        let big = arena.alloc([1u8; 10_000]);
        assert_eq!(big[9_999], 1);
        let small = arena.alloc(5u32);
        assert_eq!(*small, 5);
        let capacity = arena.capacity();
        // Frames repeat their allocation pattern: the next one fits in the kept chunks.
        for _ in 0..3 {
            arena.reset();
            arena.alloc([1u8; 10_000]);
            arena.alloc(5u32);
            assert_eq!(arena.capacity(), capacity);
        }
    }

    #[test]
    fn trait_objects_can_live_in_the_arena() {
        let mut arena = Arena::new(1024);
        let mut total = 0;
        {
            let captured = String::from("abc");
            let f: &mut dyn FnMut() -> usize = arena.alloc(move || captured.len());
            total += f();
            // SAFETY: `f` is not used afterwards; its captured Vec is freed here.
            unsafe { core::ptr::drop_in_place(f as *mut dyn FnMut() -> usize) };
        }
        assert_eq!(total, 3);
        arena.reset();
    }
}
