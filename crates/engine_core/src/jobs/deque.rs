//! Fixed-capacity Chase–Lev work-stealing deque.
//!
//! The owner pushes and pops at the bottom (LIFO); other threads steal from the top
//! (FIFO). Memory orderings follow the C11 version proven correct by Lê, Pop, Cohen and
//! Zappa Nardelli, "Correct and Efficient Work-Stealing for Weak Memory Models"
//! (PPoPP 2013), itself based on Chase and Lev, "Dynamic Circular Work-Stealing Deque"
//! (SPAA 2005).
//!
//! Differences from the paper and from `crossbeam-deque`:
//!
//! - **The buffer never grows or shrinks.** A full deque rejects the push and the caller
//!   runs the job inline. This removes all allocation after construction (CLAUDE.md §5:
//!   zero allocations per frame) and removes the need for epoch-based reclamation of old
//!   buffers.
//! - **Slots are `AtomicPtr`.** A thief may read a slot that the owner is overwriting;
//!   its CAS on `top` then fails and the value is discarded. With atomic slots that read
//!   is a plain atomic load, not a data race.

use super::atomic::{AtomicIsize, AtomicPtr, Ordering, fence};
use crate::sync::CachePadded;
use core::ptr::{self, NonNull};

/// Result of [`Deque::steal`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Steal<T> {
    /// The deque was empty.
    Empty,
    /// Lost a race with another thief or with the owner; the deque may still have items.
    Retry,
    /// Took an item.
    Success(T),
}

pub(crate) struct Deque<T> {
    /// Index of the oldest item. Only ever increases (by thieves and by the owner's pop
    /// of the last item).
    top: CachePadded<AtomicIsize>,
    /// Index one past the newest item. Written only by the owner.
    bottom: CachePadded<AtomicIsize>,
    slots: Box<[AtomicPtr<T>]>,
    mask: usize,
}

impl<T> Deque<T> {
    /// Creates a deque holding up to `capacity` items (rounded up to a power of two).
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two();
        let slots = (0..capacity)
            .map(|_| AtomicPtr::new(ptr::null_mut()))
            .collect();
        Self {
            top: CachePadded::new(AtomicIsize::new(0)),
            bottom: CachePadded::new(AtomicIsize::new(0)),
            slots,
            mask: capacity - 1,
        }
    }

    #[inline(always)]
    fn slot(&self, index: isize) -> &AtomicPtr<T> {
        // Indices are never negative: they start at 0 and only grow.
        &self.slots[index as usize & self.mask]
    }

    /// Pushes an item at the bottom. Returns it back if the deque is full.
    ///
    /// # Safety
    ///
    /// Must only be called by the owner thread of this deque.
    #[inline]
    pub(crate) unsafe fn push(&self, item: NonNull<T>) -> Result<(), NonNull<T>> {
        let b = self.bottom.load(Ordering::Relaxed);
        let t = self.top.load(Ordering::Acquire);
        if b - t > self.mask as isize {
            return Err(item);
        }
        self.slot(b).store(item.as_ptr(), Ordering::Relaxed);
        // Publishes the slot (and everything written before the push) to thieves that
        // acquire `bottom`.
        fence(Ordering::Release);
        self.bottom.store(b + 1, Ordering::Relaxed);
        Ok(())
    }

    /// Pops the newest item.
    ///
    /// # Safety
    ///
    /// Must only be called by the owner thread of this deque.
    #[inline]
    pub(crate) unsafe fn pop(&self) -> Option<NonNull<T>> {
        let b = self.bottom.load(Ordering::Relaxed) - 1;
        self.bottom.store(b, Ordering::Relaxed);
        // Orders the `bottom` store before the `top` load, against the thieves' fence.
        fence(Ordering::SeqCst);
        let t = self.top.load(Ordering::Relaxed);
        if t > b {
            // Empty.
            self.bottom.store(b + 1, Ordering::Relaxed);
            return None;
        }
        let item = self.slot(b).load(Ordering::Relaxed);
        if t == b {
            // Last item: race the thieves for it.
            let won = self
                .top
                .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok();
            self.bottom.store(b + 1, Ordering::Relaxed);
            return if won { NonNull::new(item) } else { None };
        }
        NonNull::new(item)
    }

    /// Steals the oldest item. May be called from any thread.
    #[inline]
    pub(crate) fn steal(&self) -> Steal<NonNull<T>> {
        let t = self.top.load(Ordering::Acquire);
        fence(Ordering::SeqCst);
        let b = self.bottom.load(Ordering::Acquire);
        if t >= b {
            return Steal::Empty;
        }
        // May observe a newer item if the slot was reused; the CAS below then fails.
        let item = self.slot(t).load(Ordering::Relaxed);
        if self
            .top
            .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
            .is_err()
        {
            return Steal::Retry;
        }
        match NonNull::new(item) {
            Some(item) => Steal::Success(item),
            // Unreachable for items pushed through `push` (they are non-null).
            None => Steal::Retry,
        }
    }

    /// Whether the deque looks empty. Racy; used as a hint before going to sleep. The
    /// `SeqCst` loads pair with the `SeqCst` fence the pusher issues before it checks
    /// for sleeping workers.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        let b = self.bottom.load(Ordering::SeqCst);
        let t = self.top.load(Ordering::SeqCst);
        t >= b
    }

    #[cfg(all(test, not(loom)))]
    pub(crate) fn capacity(&self) -> usize {
        self.mask + 1
    }
}

// SAFETY: the deque only stores pointers; ownership of the pointees is transferred by the
// push/steal protocol, and callers only put `Send` jobs in it.
unsafe impl<T> Send for Deque<T> {}
// SAFETY: all shared state is atomic; owner-only operations are `unsafe fn`.
unsafe impl<T> Sync for Deque<T> {}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize as StdAtomicUsize};

    fn item(v: usize) -> NonNull<u8> {
        // Tests only compare addresses, never dereference.
        NonNull::new((v * 8 + 8) as *mut u8).unwrap()
    }

    fn value(p: NonNull<u8>) -> usize {
        (p.as_ptr() as usize - 8) / 8
    }

    #[test]
    fn owner_lifo_thief_fifo() {
        let d = Deque::with_capacity(8);
        // SAFETY: single-threaded test; this thread is the owner.
        unsafe {
            for v in 0..4 {
                d.push(item(v)).unwrap();
            }
            assert_eq!(d.steal(), Steal::Success(item(0)));
            assert_eq!(d.pop().map(value), Some(3));
            assert_eq!(d.pop().map(value), Some(2));
            assert_eq!(d.steal(), Steal::Success(item(1)));
            assert_eq!(d.pop(), None);
            assert_eq!(d.steal(), Steal::Empty);
            assert!(d.is_empty());
        }
    }

    #[test]
    fn full_deque_rejects_push_and_wraps() {
        let d = Deque::with_capacity(4);
        assert_eq!(d.capacity(), 4);
        // SAFETY: single-threaded test; this thread is the owner.
        unsafe {
            for round in 0..10 {
                for v in 0..4 {
                    d.push(item(round * 4 + v)).unwrap();
                }
                assert_eq!(d.push(item(999)), Err(item(999)));
                for v in 0..4 {
                    assert_eq!(d.steal(), Steal::Success(item(round * 4 + v)));
                }
            }
        }
    }

    /// One owner pushing/popping, three thieves; every item is consumed exactly once.
    #[test]
    fn concurrent_exactly_once() {
        const N: usize = 200_000;
        let d = Arc::new(Deque::<u8>::with_capacity(64));
        let seen: Arc<Vec<StdAtomicUsize>> =
            Arc::new((0..N).map(|_| StdAtomicUsize::new(0)).collect());
        let done = Arc::new(AtomicBool::new(false));

        let thieves: Vec<_> = (0..3)
            .map(|_| {
                let (d, seen, done) = (d.clone(), seen.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut count = 0usize;
                    loop {
                        match d.steal() {
                            Steal::Success(p) => {
                                seen[value(p)].fetch_add(1, Ordering::Relaxed);
                                count += 1;
                            }
                            Steal::Retry => {}
                            Steal::Empty => {
                                if done.load(Ordering::Acquire) {
                                    break;
                                }
                                std::hint::spin_loop();
                            }
                        }
                    }
                    count
                })
            })
            .collect();

        let mut owner_count = 0;
        let mut next = 0;
        while next < N {
            // SAFETY: this thread is the only owner.
            match unsafe { d.push(item(next)) } {
                Ok(()) => next += 1,
                Err(_) => {
                    // SAFETY: owner thread.
                    if let Some(p) = unsafe { d.pop() } {
                        seen[value(p)].fetch_add(1, Ordering::Relaxed);
                        owner_count += 1;
                    }
                }
            }
            if next % 3 == 0 {
                // SAFETY: owner thread.
                if let Some(p) = unsafe { d.pop() } {
                    seen[value(p)].fetch_add(1, Ordering::Relaxed);
                    owner_count += 1;
                }
            }
        }
        // SAFETY: owner thread.
        while let Some(p) = unsafe { d.pop() } {
            seen[value(p)].fetch_add(1, Ordering::Relaxed);
            owner_count += 1;
        }
        done.store(true, Ordering::Release);
        let stolen: usize = thieves.into_iter().map(|t| t.join().unwrap()).sum();

        assert_eq!(owner_count + stolen, N);
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

    /// Owner pushes two items and pops; a thief steals concurrently. Each item is taken
    /// exactly once in every interleaving.
    #[test]
    fn loom_push_pop_steal() {
        loom::model(|| {
            let d = Arc::new(Deque::<u8>::with_capacity(2));
            let thief = {
                let d = d.clone();
                thread::spawn(move || match d.steal() {
                    Steal::Success(p) => Some(p),
                    _ => None,
                })
            };
            // SAFETY: this thread is the owner.
            let mut got = Vec::new();
            unsafe {
                d.push(item(1)).unwrap();
                d.push(item(2)).unwrap();
                while let Some(p) = d.pop() {
                    got.push(p);
                }
            }
            if let Some(p) = thief.join().unwrap() {
                got.push(p);
            }
            got.sort();
            assert_eq!(got, vec![item(1), item(2)]);
        });
    }

    /// Last-item race between the owner's pop and a thief.
    #[test]
    fn loom_last_item_race() {
        loom::model(|| {
            let d = Arc::new(Deque::<u8>::with_capacity(2));
            // SAFETY: this thread is the owner.
            unsafe { d.push(item(7)).unwrap() };
            let thief = {
                let d = d.clone();
                thread::spawn(move || matches!(d.steal(), Steal::Success(_)))
            };
            // SAFETY: owner.
            let owner_got = unsafe { d.pop() }.is_some();
            let thief_got = thief.join().unwrap();
            assert!(owner_got ^ thief_got);
        });
    }

    /// Slot reuse after wrap-around while a thief is mid-steal.
    #[test]
    fn loom_wraparound() {
        loom::model(|| {
            let d = Arc::new(Deque::<u8>::with_capacity(2));
            let thief = {
                let d = d.clone();
                thread::spawn(move || {
                    let mut got = Vec::new();
                    for _ in 0..2 {
                        if let Steal::Success(p) = d.steal() {
                            got.push(p);
                        }
                    }
                    got
                })
            };
            let mut got = Vec::new();
            // SAFETY: owner.
            unsafe {
                for v in 1..=3 {
                    if d.push(item(v)).is_err() {
                        if let Some(p) = d.pop() {
                            got.push(p);
                        }
                        d.push(item(v)).unwrap();
                    }
                }
                while let Some(p) = d.pop() {
                    got.push(p);
                }
            }
            got.extend(thief.join().unwrap());
            got.sort();
            assert_eq!(got, vec![item(1), item(2), item(3)]);
        });
    }
}
