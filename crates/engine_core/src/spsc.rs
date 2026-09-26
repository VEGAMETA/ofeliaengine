//! Bounded single-producer single-consumer ring buffer.
//!
//! Hands values from one thread to another without locks or allocation after
//! construction: platform threads (window, raw input) to the main thread, the main
//! thread to the audio thread. Each side owns one index and only reads the other's, so a
//! push or a pop is one load, one move and one store. Values still in the ring when both
//! halves are gone are dropped by whichever half goes last.
//!
//! ```
//! use engine_core::spsc;
//!
//! let (mut tx, mut rx) = spsc::channel::<u32>(4);
//! std::thread::spawn(move || {
//!     for i in 0..100 {
//!         while tx.push(i).is_err() {
//!             std::hint::spin_loop();
//!         }
//!     }
//! });
//! let mut received = 0;
//! while received < 100 {
//!     if let Some(v) = rx.pop() {
//!         assert_eq!(v, received);
//!         received += 1;
//!     }
//! }
//! ```

use crate::atomic::{AtomicUsize, Ordering, UnsafeCell};
use crate::sync::CachePadded;
use core::mem::MaybeUninit;

#[cfg(not(loom))]
use std::sync::Arc;

#[cfg(loom)]
use loom::sync::Arc;

struct Ring<T> {
    /// Next slot to write; written only by the producer.
    head: CachePadded<AtomicUsize>,
    /// Next slot to read; written only by the consumer.
    tail: CachePadded<AtomicUsize>,
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
}

// SAFETY: a slot is written only by the producer before it publishes `head`, and read
// only by the consumer after acquiring `head`; `T: Send` moves between the two threads.
unsafe impl<T: Send> Sync for Ring<T> {}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        if !core::mem::needs_drop::<T>() {
            return;
        }
        // Both halves are gone, so nothing else touches the indices or the slots.
        let head = self.head.load(Ordering::Relaxed);
        let mut tail = self.tail.load(Ordering::Relaxed);
        while tail != head {
            // SAFETY: slots in `tail..head` were written by the producer and not yet taken
            // by the consumer; each is dropped once.
            self.slots[tail & self.mask].with_mut(|slot| unsafe { (*slot).assume_init_drop() });
            tail = tail.wrapping_add(1);
        }
    }
}

/// Sending half. Not `Clone`: exactly one producer.
pub struct Producer<T> {
    ring: Arc<Ring<T>>,
    /// Producer's cached copy of `tail`, refreshed only when the ring looks full.
    cached_tail: usize,
}

/// Receiving half. Not `Clone`: exactly one consumer.
pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
    /// Consumer's cached copy of `head`, refreshed only when the ring looks empty.
    cached_head: usize,
}

/// Creates a channel holding up to `capacity` values (rounded up to a power of two).
pub fn channel<T: Send>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let capacity = capacity.max(2).next_power_of_two();
    let slots = (0..capacity)
        .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
        .collect();
    let ring = Arc::new(Ring {
        head: CachePadded::new(AtomicUsize::new(0)),
        tail: CachePadded::new(AtomicUsize::new(0)),
        slots,
        mask: capacity - 1,
    });
    (
        Producer {
            ring: ring.clone(),
            cached_tail: 0,
        },
        Consumer {
            ring,
            cached_head: 0,
        },
    )
}

impl<T: Send> Producer<T> {
    /// Appends `value`, or returns it if the ring is full.
    #[inline]
    pub fn push(&mut self, value: T) -> Result<(), T> {
        let ring = &*self.ring;
        let head = ring.head.load(Ordering::Relaxed);
        if head.wrapping_sub(self.cached_tail) > ring.mask {
            self.cached_tail = ring.tail.load(Ordering::Acquire);
            if head.wrapping_sub(self.cached_tail) > ring.mask {
                return Err(value);
            }
        }
        // SAFETY: the slot at `head` is not visible to the consumer (it reads only below
        // `head`) and was released by it (checked against `tail` above).
        ring.slots[head & ring.mask].with_mut(|slot| unsafe { (*slot).write(value) });
        ring.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Capacity of the ring.
    pub fn capacity(&self) -> usize {
        self.ring.mask + 1
    }
}

impl<T: Send> Consumer<T> {
    /// Takes the oldest value, if any.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        let ring = &*self.ring;
        let tail = ring.tail.load(Ordering::Relaxed);
        if tail == self.cached_head {
            self.cached_head = ring.head.load(Ordering::Acquire);
            if tail == self.cached_head {
                return None;
            }
        }
        // SAFETY: `tail < head`, so the producer wrote this slot and published it with
        // the release store of `head` that we acquired.
        let value =
            ring.slots[tail & ring.mask].with_mut(|slot| unsafe { (*slot).assume_init_read() });
        ring.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(value)
    }

    /// Calls `f` for every value available now; returns how many there were.
    #[inline]
    pub fn drain(&mut self, mut f: impl FnMut(T)) -> usize {
        let mut n = 0;
        while let Some(v) = self.pop() {
            f(v);
            n += 1;
        }
        n
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use super::*;

    #[test]
    fn fifo_and_full() {
        let (mut tx, mut rx) = channel::<u8>(4);
        assert_eq!(tx.capacity(), 4);
        for i in 0..4 {
            tx.push(i).unwrap();
        }
        assert_eq!(tx.push(9), Err(9));
        assert_eq!(rx.pop(), Some(0));
        tx.push(4).unwrap();
        let mut got = Vec::new();
        assert_eq!(rx.drain(|v| got.push(v)), 4);
        assert_eq!(got, vec![1, 2, 3, 4]);
        assert_eq!(rx.pop(), None);
    }

    #[test]
    fn concurrent_in_order() {
        const N: u64 = 1_000_000;
        let (mut tx, mut rx) = channel::<u64>(64);
        let producer = std::thread::spawn(move || {
            for i in 0..N {
                let mut v = i;
                while let Err(back) = tx.push(v) {
                    v = back;
                    std::hint::spin_loop();
                }
            }
        });
        let mut expected = 0;
        while expected < N {
            if let Some(v) = rx.pop() {
                assert_eq!(v, expected);
                expected += 1;
            }
        }
        producer.join().unwrap();
    }

    #[test]
    fn owned_values_are_dropped_once() {
        let counter = std::sync::Arc::new(());
        let (mut tx, mut rx) = channel::<std::sync::Arc<()>>(8);
        for _ in 0..5 {
            tx.push(counter.clone()).unwrap();
        }
        drop(rx.pop());
        drop(rx.pop());
        assert_eq!(std::sync::Arc::strong_count(&counter), 4);
        drop(tx);
        assert_eq!(std::sync::Arc::strong_count(&counter), 4);
        drop(rx);
        assert_eq!(std::sync::Arc::strong_count(&counter), 1);
    }
}

#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
    use super::*;

    #[test]
    fn loom_spsc_in_order_across_wraparound() {
        loom::model(|| {
            let (mut tx, mut rx) = channel::<u32>(2);
            let producer = loom::thread::spawn(move || {
                for i in 0..3 {
                    let mut v = i;
                    while let Err(back) = tx.push(v) {
                        v = back;
                        loom::thread::yield_now();
                    }
                }
            });
            let mut expected = 0;
            while expected < 3 {
                match rx.pop() {
                    Some(v) => {
                        assert_eq!(v, expected);
                        expected += 1;
                    }
                    None => loom::thread::yield_now(),
                }
            }
            producer.join().unwrap();
        });
    }
}
