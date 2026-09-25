//! Per-participant bump arena for `Scope::spawn` job records.
//!
//! Scopes nest strictly on a thread: a scope that starts while another one on the same
//! thread is waiting (for example inside a job the waiting thread picked up) finishes
//! before the outer scope resumes. So each scope records the arena top when it starts and
//! resets to it when it ends, like a stack. When the arena is full, `spawn` runs the job
//! inline instead: parallelism degrades, memory use does not grow.

use core::alloc::Layout;
use core::cell::Cell;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

pub(crate) struct SpawnArena {
    base: NonNull<MaybeUninit<u8>>,
    capacity: usize,
    top: Cell<usize>,
}

impl SpawnArena {
    pub(crate) fn new(capacity: usize) -> Self {
        let buffer: Box<[MaybeUninit<u8>]> = Box::new_uninit_slice(capacity);
        let base = NonNull::from(Box::leak(buffer)).cast::<MaybeUninit<u8>>();
        Self {
            base,
            capacity,
            top: Cell::new(0),
        }
    }

    #[inline]
    pub(crate) fn mark(&self) -> usize {
        self.top.get()
    }

    #[inline]
    pub(crate) fn reset(&self, mark: usize) {
        debug_assert!(mark <= self.top.get());
        self.top.set(mark);
    }

    /// Allocates `layout`, or returns `None` if it does not fit.
    #[inline]
    pub(crate) fn alloc(&self, layout: Layout) -> Option<NonNull<u8>> {
        let base = self.base.as_ptr() as usize;
        let current = base.checked_add(self.top.get())?;
        let start = current.checked_add(layout.align() - 1)? & !(layout.align() - 1);
        let end = start.checked_add(layout.size())?;
        if end > base + self.capacity {
            return None;
        }
        self.top.set(end - base);
        // SAFETY: `start - base` is within the buffer, so the pointer is in bounds and
        // non-null.
        Some(unsafe { NonNull::new_unchecked(self.base.as_ptr().cast::<u8>().add(start - base)) })
    }
}

impl Drop for SpawnArena {
    fn drop(&mut self) {
        let slice = core::ptr::slice_from_raw_parts_mut(self.base.as_ptr(), self.capacity);
        // SAFETY: `slice` is exactly the allocation leaked in `new`.
        drop(unsafe { Box::from_raw(slice) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_respects_alignment_and_capacity() {
        let arena = SpawnArena::new(256);
        let a = arena.alloc(Layout::from_size_align(3, 1).unwrap()).unwrap();
        let b = arena
            .alloc(Layout::from_size_align(8, 64).unwrap())
            .unwrap();
        assert_eq!(b.as_ptr() as usize % 64, 0);
        assert!(b.as_ptr() as usize > a.as_ptr() as usize);
        assert!(
            arena
                .alloc(Layout::from_size_align(512, 1).unwrap())
                .is_none()
        );
    }

    #[test]
    fn reset_rewinds_like_a_stack() {
        let arena = SpawnArena::new(64);
        let outer = arena.mark();
        arena.alloc(Layout::new::<[u8; 32]>()).unwrap();
        let inner = arena.mark();
        arena.alloc(Layout::new::<[u8; 16]>()).unwrap();
        arena.reset(inner);
        assert_eq!(arena.mark(), inner);
        arena.reset(outer);
        assert!(arena.alloc(Layout::new::<[u8; 64]>()).is_some());
    }
}
