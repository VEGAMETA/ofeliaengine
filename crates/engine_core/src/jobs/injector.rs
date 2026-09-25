//! Bounded multi-producer multi-consumer queue for jobs submitted by threads that are not
//! participants of the pool.
//!
//! Dmitry Vyukov's bounded MPMC queue (1024cores.net, "Bounded MPMC queue"): every cell
//! carries a sequence number that tells producers and consumers whose turn it is, so a
//! push or pop is one CAS on a position counter plus one store. Values are `AtomicPtr` so
//! that no access is a data race even under contention.

use super::atomic::{AtomicPtr, AtomicUsize, Ordering};
use crate::sync::CachePadded;
use core::ptr::{self, NonNull};

struct Cell<T> {
    sequence: AtomicUsize,
    value: AtomicPtr<T>,
}

pub(crate) struct Injector<T> {
    enqueue: CachePadded<AtomicUsize>,
    dequeue: CachePadded<AtomicUsize>,
    cells: Box<[Cell<T>]>,
    mask: usize,
}

impl<T> Injector<T> {
    /// Creates a queue holding up to `capacity` items (rounded up to a power of two).
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two();
        let cells = (0..capacity)
            .map(|i| Cell {
                sequence: AtomicUsize::new(i),
                value: AtomicPtr::new(ptr::null_mut()),
            })
            .collect();
        Self {
            enqueue: CachePadded::new(AtomicUsize::new(0)),
            dequeue: CachePadded::new(AtomicUsize::new(0)),
            cells,
            mask: capacity - 1,
        }
    }

    /// Pushes an item. Returns it back if the queue is full.
    pub(crate) fn push(&self, item: NonNull<T>) -> Result<(), NonNull<T>> {
        let mut pos = self.enqueue.load(Ordering::Relaxed);
        let cell = loop {
            let cell = &self.cells[pos & self.mask];
            let seq = cell.sequence.load(Ordering::Acquire);
            let diff = seq as isize - pos as isize;
            if diff == 0 {
                match self.enqueue.compare_exchange_weak(
                    pos,
                    pos + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break cell,
                    Err(current) => pos = current,
                }
            } else if diff < 0 {
                return Err(item);
            } else {
                pos = self.enqueue.load(Ordering::Relaxed);
            }
        };
        cell.value.store(item.as_ptr(), Ordering::Relaxed);
        cell.sequence.store(pos + 1, Ordering::Release);
        Ok(())
    }

    /// Pops the oldest item.
    pub(crate) fn pop(&self) -> Option<NonNull<T>> {
        let mut pos = self.dequeue.load(Ordering::Relaxed);
        let cell = loop {
            let cell = &self.cells[pos & self.mask];
            let seq = cell.sequence.load(Ordering::Acquire);
            let diff = seq as isize - (pos + 1) as isize;
            if diff == 0 {
                match self.dequeue.compare_exchange_weak(
                    pos,
                    pos + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break cell,
                    Err(current) => pos = current,
                }
            } else if diff < 0 {
                return None;
            } else {
                pos = self.dequeue.load(Ordering::Relaxed);
            }
        };
        let item = cell.value.load(Ordering::Relaxed);
        cell.sequence.store(pos + self.mask + 1, Ordering::Release);
        NonNull::new(item)
    }

    /// Whether the queue looks empty. Racy; used as a hint before going to sleep.
    pub(crate) fn is_empty(&self) -> bool {
        let d = self.dequeue.load(Ordering::SeqCst);
        let e = self.enqueue.load(Ordering::SeqCst);
        e == d
    }
}

// SAFETY: stores pointers only; ownership of pointees moves through push/pop.
unsafe impl<T> Send for Injector<T> {}
// SAFETY: all shared state is atomic.
unsafe impl<T> Sync for Injector<T> {}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn item(v: usize) -> NonNull<u8> {
        NonNull::new((v * 8 + 8) as *mut u8).unwrap()
    }

    fn value(p: NonNull<u8>) -> usize {
        (p.as_ptr() as usize - 8) / 8
    }

    #[test]
    fn fifo_and_full() {
        let q = Injector::with_capacity(4);
        for v in 0..4 {
            q.push(item(v)).unwrap();
        }
        assert_eq!(q.push(item(9)), Err(item(9)));
        for v in 0..4 {
            assert_eq!(q.pop().map(value), Some(v));
        }
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn concurrent_exactly_once() {
        const PER_PRODUCER: usize = 50_000;
        const PRODUCERS: usize = 3;
        let q = Arc::new(Injector::<u8>::with_capacity(64));
        let seen: Arc<Vec<std::sync::atomic::AtomicUsize>> = Arc::new(
            (0..PER_PRODUCER * PRODUCERS)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
        );
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|p| {
                let q = q.clone();
                std::thread::spawn(move || {
                    for i in 0..PER_PRODUCER {
                        let mut it = item(p * PER_PRODUCER + i);
                        while let Err(back) = q.push(it) {
                            it = back;
                            std::hint::spin_loop();
                        }
                    }
                })
            })
            .collect();
        let consumers: Vec<_> = (0..2)
            .map(|_| {
                let (q, seen) = (q.clone(), seen.clone());
                std::thread::spawn(move || {
                    let mut n = 0;
                    while n < PER_PRODUCER * PRODUCERS / 2 {
                        if let Some(p) = q.pop() {
                            seen[value(p)].fetch_add(1, Ordering::Relaxed);
                            n += 1;
                        }
                    }
                })
            })
            .collect();
        for t in producers.into_iter().chain(consumers) {
            t.join().unwrap();
        }
        assert!(seen.iter().all(|c| c.load(Ordering::Relaxed) == 1));
    }
}

#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    fn item(v: usize) -> NonNull<u8> {
        NonNull::new((v * 8 + 8) as *mut u8).unwrap()
    }

    #[test]
    fn loom_two_producers_one_consumer() {
        loom::model(|| {
            let q = Arc::new(Injector::<u8>::with_capacity(2));
            let producers: Vec<_> = (1..=2)
                .map(|v| {
                    let q = q.clone();
                    thread::spawn(move || q.push(item(v)).is_ok())
                })
                .collect();
            let mut got = Vec::new();
            for _ in 0..2 {
                if let Some(p) = q.pop() {
                    got.push(p);
                }
            }
            for p in producers {
                assert!(p.join().unwrap());
            }
            while let Some(p) = q.pop() {
                got.push(p);
            }
            got.sort();
            assert_eq!(got, vec![item(1), item(2)]);
        });
    }
}
