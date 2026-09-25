//! Primitives for data shared between threads.

use core::fmt;
use core::ops::{Deref, DerefMut};

/// Size, in bytes, of the unit of false sharing on the target CPU.
///
/// A cache line on x86-64 is 64 bytes, but Intel's spatial prefetcher fetches
/// cache lines in adjacent 64-byte pairs (Intel 64 and IA-32 Architectures
/// Optimization Reference Manual, "Data Prefetching"). Two hot values in the same
/// 128-byte aligned pair can still interfere, so we pad to 128. AMD Zen does not
/// pair lines, and there the extra 64 bytes only cost memory.
pub const FALSE_SHARING_SIZE: usize = 128;

/// Pads and aligns `T` to [`FALSE_SHARING_SIZE`] so that it never shares a cache line
/// (or an adjacent-line prefetch pair) with other data.
///
/// Use it for per-thread counters, queue heads/tails, and any other value that one
/// thread writes while other threads read or write neighbours (CLAUDE.md §5).
///
/// # Examples
///
/// ```
/// use core::sync::atomic::{AtomicU64, Ordering};
/// use engine_core::sync::CachePadded;
///
/// // Two counters updated by different threads: each gets its own cache lines.
/// let counters = [CachePadded::new(AtomicU64::new(0)), CachePadded::new(AtomicU64::new(0))];
/// counters[0].fetch_add(1, Ordering::Relaxed);
///
/// assert_eq!(core::mem::align_of_val(&counters[0]), 128);
/// assert_eq!(core::mem::size_of_val(&counters), 256);
/// ```
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(C, align(128))]
pub struct CachePadded<T> {
    value: T,
}

// `repr(align)` needs a literal; keep it in sync with the public constant.
const _: () = assert!(core::mem::align_of::<CachePadded<u8>>() == FALSE_SHARING_SIZE);

impl<T> CachePadded<T> {
    /// Wraps `value`.
    #[inline(always)]
    pub const fn new(value: T) -> Self {
        Self { value }
    }

    /// Returns the wrapped value.
    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for CachePadded<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T> From<T> for CachePadded<T> {
    #[inline(always)]
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: fmt::Debug> fmt::Debug for CachePadded<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CachePadded").field(&self.value).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};
    use core::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn layout_small_value_takes_one_unit() {
        assert_eq!(align_of::<CachePadded<u8>>(), FALSE_SHARING_SIZE);
        assert_eq!(size_of::<CachePadded<u8>>(), FALSE_SHARING_SIZE);
        assert_eq!(size_of::<CachePadded<AtomicU64>>(), FALSE_SHARING_SIZE);
    }

    #[test]
    fn layout_large_value_rounds_up() {
        assert_eq!(size_of::<CachePadded<[u8; 129]>>(), 2 * FALSE_SHARING_SIZE);
    }

    #[test]
    fn array_elements_do_not_share_units() {
        let values = [CachePadded::new(0u32), CachePadded::new(1u32)];
        let a = &*values[0] as *const u32 as usize;
        let b = &*values[1] as *const u32 as usize;
        assert_eq!(a % FALSE_SHARING_SIZE, 0);
        assert_eq!(b - a, FALSE_SHARING_SIZE);
    }

    #[test]
    fn deref_reads_and_writes_inner_value() {
        let mut padded = CachePadded::new(5u32);
        *padded += 1;
        assert_eq!(*padded, 6);
        assert_eq!(padded.into_inner(), 6);

        let counter = CachePadded::new(AtomicU64::new(0));
        counter.fetch_add(3, Ordering::Relaxed);
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }
}
