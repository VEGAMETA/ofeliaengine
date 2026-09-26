use super::*;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

fn pool(workers: usize) -> ThreadPool {
    ThreadPool::builder().workers(workers).build().unwrap()
}

/// Work the optimizer cannot fold away.
fn busy_work(seed: u64, iterations: u32) -> u64 {
    let mut x = std::hint::black_box(seed);
    for _ in 0..iterations {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
    }
    x
}

fn fib(pool: &ThreadPool, n: u64) -> u64 {
    if n < 12 {
        return (1..=n).fold((0u64, 1u64), |(a, b), _| (b, a + b)).0;
    }
    let (a, b) = pool.join(|| fib(pool, n - 1), || fib(pool, n - 2));
    a + b
}

#[test]
fn join_from_external_thread() {
    let pool = pool(3);
    assert!(!pool.is_participant());
    assert_eq!(fib(&pool, 24), 46368);
}

#[test]
fn join_from_registered_thread() {
    let pool = pool(3);
    pool.register_current_thread().unwrap();
    assert!(pool.is_participant());
    for _ in 0..20 {
        assert_eq!(fib(&pool, 22), 17711);
    }
}

#[test]
fn install_runs_on_a_worker() {
    let pool = pool(2);
    let on_participant = pool.install(|| pool.is_participant());
    assert!(on_participant);
    assert_eq!(pool.install(|| 7), 7);
}

#[test]
fn parallel_for_uses_multiple_threads() {
    let pool = pool(3);
    pool.register_current_thread().unwrap();
    let ids = std::sync::Mutex::new(std::collections::HashSet::new());
    // Whoever runs chunk 0 waits until another thread has run a chunk, so the result does
    // not depend on how soon the OS schedules a woken worker. Timing alone was not enough:
    // Windows Server's long quantum on an oversubscribed 4-vCPU runner let the calling
    // thread finish ~100 ms of chunks before any worker ran.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    pool.parallel_for(256 * 20, 1, |chunk, _| {
        ids.lock().unwrap().insert(std::thread::current().id());
        if chunk == 0 {
            while ids.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
        }
        std::hint::black_box(busy_work(chunk as u64, 2_000));
    });
    assert!(ids.lock().unwrap().len() > 1);
}

#[test]
fn scope_spawns_borrowing_jobs() {
    let pool = pool(3);
    pool.register_current_thread().unwrap();
    let mut data = vec![0u64; 4096];
    pool.scope(|s| {
        for (i, chunk) in data.chunks_mut(64).enumerate() {
            s.spawn(move || {
                for (j, v) in chunk.iter_mut().enumerate() {
                    *v = (i * 64 + j) as u64;
                }
            });
        }
    });
    assert!(data.iter().enumerate().all(|(i, &v)| v == i as u64));
}

#[test]
fn scope_arena_overflow_runs_inline() {
    // A tiny arena: most spawns do not fit and must run inline, still correctly.
    let pool = ThreadPool::builder()
        .workers(2)
        .spawn_arena_bytes(128)
        .build()
        .unwrap();
    pool.register_current_thread().unwrap();
    let counter = AtomicUsize::new(0);
    pool.scope(|s| {
        for _ in 0..1000 {
            s.spawn(|| {
                counter.fetch_add(1, Ordering::Relaxed);
            });
        }
    });
    assert_eq!(counter.load(Ordering::Relaxed), 1000);
}

#[test]
fn deque_overflow_runs_inline() {
    let pool = ThreadPool::builder()
        .workers(2)
        .deque_capacity(4)
        .build()
        .unwrap();
    pool.register_current_thread().unwrap();
    let counter = AtomicUsize::new(0);
    pool.scope(|s| {
        for _ in 0..500 {
            s.spawn(|| {
                counter.fetch_add(1, Ordering::Relaxed);
            });
        }
    });
    assert_eq!(counter.load(Ordering::Relaxed), 500);
    assert_eq!(fib(&pool, 20), 6765);
}

#[test]
fn nested_scopes_and_joins() {
    let pool = pool(3);
    pool.register_current_thread().unwrap();
    let total = AtomicUsize::new(0);
    pool.scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                pool.scope(|inner| {
                    for _ in 0..8 {
                        inner.spawn(|| {
                            let (a, b) = pool.join(|| 1, || 2);
                            total.fetch_add(a + b, Ordering::Relaxed);
                        });
                    }
                });
            });
        }
    });
    assert_eq!(total.load(Ordering::Relaxed), 8 * 8 * 3);
}

#[test]
fn parallel_for_visits_every_index_once() {
    let pool = pool(3);
    pool.register_current_thread().unwrap();
    for (len, grain) in [
        (0, 1),
        (1, 1),
        (7, 3),
        (1000, 1),
        (1000, 7),
        (100_000, 1000),
        (5, 100),
    ] {
        let seen: Vec<AtomicU32> = (0..len).map(|_| AtomicU32::new(0)).collect();
        pool.parallel_for(len, grain, |chunk, range| {
            assert_eq!(range.start, chunk * grain.max(1));
            for i in range {
                seen[i].fetch_add(1, Ordering::Relaxed);
            }
        });
        assert!(
            seen.iter().all(|c| c.load(Ordering::Relaxed) == 1),
            "len {len} grain {grain}"
        );
    }
}

#[test]
fn parallel_for_chunks_are_deterministic() {
    // Same chunk boundaries regardless of worker count.
    let record = |workers: usize| {
        let pool = pool(workers);
        let ranges: Vec<std::sync::Mutex<(usize, usize)>> =
            (0..100).map(|_| std::sync::Mutex::new((0, 0))).collect();
        pool.parallel_for(997, 10, |chunk, range| {
            *ranges[chunk].lock().unwrap() = (range.start, range.end);
        });
        ranges
            .into_iter()
            .map(|m| m.into_inner().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(record(1), record(3));
}

#[test]
fn panic_in_join_a_propagates_after_b() {
    let pool = pool(2);
    pool.register_current_thread().unwrap();
    let b_done = AtomicBool::new(false);
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        pool.join(
            || panic!("a failed"),
            || {
                std::hint::black_box(busy_work(1, 10_000));
                b_done.store(true, Ordering::SeqCst);
            },
        )
    }));
    assert!(result.is_err());
    // The pool is still usable.
    assert_eq!(fib(&pool, 18), 2584);
}

#[test]
fn panic_in_join_b_propagates() {
    let pool = pool(2);
    for registered in [false, true] {
        if registered {
            pool.register_current_thread().unwrap();
        }
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            pool.join(|| 1, || -> u32 { panic!("b failed") })
        }));
        assert!(result.is_err());
    }
}

#[test]
fn panic_in_scope_job_propagates_after_all_jobs() {
    let pool = pool(3);
    pool.register_current_thread().unwrap();
    let finished = AtomicUsize::new(0);
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        pool.scope(|s| {
            for i in 0..64 {
                let finished = &finished;
                s.spawn(move || {
                    if i == 13 {
                        panic!("job 13 failed");
                    }
                    finished.fetch_add(1, Ordering::Relaxed);
                });
            }
        })
    }));
    assert!(result.is_err());
    assert_eq!(finished.load(Ordering::Relaxed), 63);
}

#[test]
fn panic_in_parallel_for_propagates() {
    let pool = pool(3);
    pool.register_current_thread().unwrap();
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        pool.parallel_for(1000, 1, |chunk, _| {
            if chunk == 500 {
                panic!("chunk failed");
            }
        })
    }));
    assert!(result.is_err());
    pool.parallel_for(100, 1, |_, _| {});
}

#[test]
fn many_external_threads_share_the_pool() {
    let pool = Arc::new(pool(3));
    let threads: Vec<_> = (0..6)
        .map(|_| {
            let pool = pool.clone();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    assert_eq!(fib(&pool, 16), 987);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

#[test]
fn registration_is_limited_to_external_slots() {
    let pool = Arc::new(
        ThreadPool::builder()
            .workers(1)
            .external_participants(1)
            .build()
            .unwrap(),
    );
    pool.register_current_thread().unwrap();
    // Registering again from the same thread is a no-op.
    pool.register_current_thread().unwrap();
    let other = {
        let pool = pool.clone();
        std::thread::spawn(move || pool.register_current_thread())
    };
    assert_eq!(other.join().unwrap(), Err(RegisterError));
}

#[test]
fn too_many_participants_is_an_error() {
    let result = ThreadPool::builder()
        .workers(60)
        .external_participants(10)
        .build();
    assert!(matches!(
        result,
        Err(BuildError::TooManyParticipants {
            requested: 70,
            max: 64
        })
    ));
}

#[test]
fn idle_pool_wakes_up_repeatedly() {
    // Workers go to sleep between bursts; every burst must still complete.
    let pool = ThreadPool::builder()
        .workers(3)
        .spin_rounds(0)
        .build()
        .unwrap();
    pool.register_current_thread().unwrap();
    for _ in 0..200 {
        let count = AtomicUsize::new(0);
        pool.parallel_for(64, 1, |_, _| {
            count.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(count.load(Ordering::Relaxed), 64);
        for _ in 0..1000 {
            std::hint::spin_loop();
        }
    }
}

#[test]
fn on_thread_start_runs_for_each_worker() {
    let started = Arc::new(AtomicUsize::new(0));
    let hook = started.clone();
    let pool = ThreadPool::builder()
        .workers(3)
        .on_thread_start(move |_| {
            hook.fetch_add(1, Ordering::SeqCst);
        })
        .build()
        .unwrap();
    // Make sure every worker has started by giving them all something to do.
    pool.parallel_for(3000, 1, |_, _| {});
    drop(pool);
    assert_eq!(started.load(Ordering::SeqCst), 3);
}

#[test]
fn drop_with_sleeping_workers_terminates() {
    for _ in 0..20 {
        let pool = ThreadPool::builder()
            .workers(4)
            .spin_rounds(0)
            .build()
            .unwrap();
        assert_eq!(pool.install(|| 1), 1);
        drop(pool);
    }
}
