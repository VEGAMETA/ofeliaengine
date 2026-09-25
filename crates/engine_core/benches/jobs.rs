//! Job system benchmarks: `engine_core::jobs` against rayon.
//!
//! `cargo bench -p engine_core --bench jobs` prints a Markdown report (used verbatim by
//! the bench workflow as a commit comment). A tiny harness instead of criterion keeps the
//! dependency tree small and gives exactly the statistics we want: median and p90 over
//! samples, plus heap allocations per operation.

#![allow(missing_docs, clippy::unwrap_used, clippy::print_stdout)]

use engine_core::jobs::ThreadPool;
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// Allocation counting
// ---------------------------------------------------------------------------------------

struct CountingAllocator;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

// ---------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------

struct Stats {
    median: Duration,
    p90: Duration,
    allocs_per_op: f64,
}

fn stats(mut times: Vec<Duration>, allocs: usize) -> Stats {
    let samples = times.len();
    times.sort();
    Stats {
        median: times[samples / 2],
        p90: times[samples * 9 / 10],
        allocs_per_op: allocs as f64 / samples as f64,
    }
}

/// Times `samples` runs of `op` after `warmup` untimed runs. Returns per-run times and
/// the number of heap allocations (process-wide) during the timed runs.
fn time_runs(samples: usize, warmup: usize, op: &mut dyn FnMut()) -> (Vec<Duration>, usize) {
    for _ in 0..warmup {
        op();
    }
    let mut times = Vec::with_capacity(samples);
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    for _ in 0..samples {
        let start = Instant::now();
        op();
        times.push(start.elapsed());
    }
    (times, ALLOCATIONS.load(Ordering::SeqCst) - before)
}

/// Measures the engine and rayon versions of a case in alternating blocks, so that clock
/// drift and background load affect both sides alike. The rayon side runs inside
/// `rayon.install`, i.e. on a rayon worker, as the engine side runs on a participant.
fn compare(
    rayon: &rayon::ThreadPool,
    rounds: usize,
    per_round: usize,
    mut ours: impl FnMut(),
    mut theirs: impl FnMut() + Send,
) -> (Stats, Stats) {
    let (mut ours_times, mut theirs_times) = (Vec::new(), Vec::new());
    let (mut ours_allocs, mut theirs_allocs) = (0, 0);
    for round in 0..rounds {
        let warmup = if round == 0 { per_round } else { 1 };
        let (t, a) = time_runs(per_round, warmup, &mut ours);
        ours_times.extend(t);
        ours_allocs += a;
        let (t, a) = rayon.install(|| time_runs(per_round, warmup, &mut theirs));
        theirs_times.extend(t);
        theirs_allocs += a;
    }
    (
        stats(ours_times, ours_allocs),
        stats(theirs_times, theirs_allocs),
    )
}

/// Median of a sequential run, for the speed-up column.
fn sequential(samples: usize, mut op: impl FnMut()) -> Duration {
    let (times, _) = time_runs(samples, 1, &mut op);
    stats(times, 0).median
}

fn fmt_time(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 10_000 {
        format!("{ns} ns")
    } else if ns < 10_000_000 {
        format!("{:.1} µs", ns as f64 / 1e3)
    } else {
        format!("{:.2} ms", ns as f64 / 1e6)
    }
}

struct Row {
    case: &'static str,
    ours: Stats,
    rayon: Stats,
    sequential: Option<Duration>,
}

fn print_report(threads: usize, rows: &[Row]) {
    println!("### Job system benchmarks");
    println!();
    println!(
        "`engine_core::jobs` vs rayon 1.x, {threads} threads each (engine: {} workers + registered caller). Median / p90 per operation; allocations per operation.",
        threads - 1
    );
    println!();
    println!(
        "| Case | engine median | engine p90 | engine allocs | rayon median | rayon p90 | rayon allocs | rayon / engine | sequential |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for r in rows {
        let ratio = r.rayon.median.as_secs_f64() / r.ours.median.as_secs_f64();
        println!(
            "| {} | {} | {} | {:.2} | {} | {} | {:.2} | {:.2}x | {} |",
            r.case,
            fmt_time(r.ours.median),
            fmt_time(r.ours.p90),
            r.ours.allocs_per_op,
            fmt_time(r.rayon.median),
            fmt_time(r.rayon.p90),
            r.rayon.allocs_per_op,
            ratio,
            r.sequential.map_or_else(|| "–".to_string(), fmt_time),
        );
    }
    println!();
    println!("`rayon / engine` > 1 means the engine is faster.");
}

// ---------------------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------------------

/// Work the optimizer cannot fold away: `iterations` rounds of an LCG.
#[inline(never)]
fn busy(seed: u64, iterations: u32) -> u64 {
    let mut x = black_box(seed);
    for _ in 0..iterations {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
    }
    x
}

fn fib_seq(n: u32) -> u64 {
    if n < 2 {
        n as u64
    } else {
        fib_seq(n - 1) + fib_seq(n - 2)
    }
}

fn fib_engine(pool: &ThreadPool, n: u32) -> u64 {
    if n < 12 {
        return fib_seq(n);
    }
    let (a, b) = pool.join(|| fib_engine(pool, n - 1), || fib_engine(pool, n - 2));
    a + b
}

fn fib_rayon(n: u32) -> u64 {
    if n < 12 {
        return fib_seq(n);
    }
    let (a, b) = rayon::join(|| fib_rayon(n - 1), || fib_rayon(n - 2));
    a + b
}

/// Spin (not sleep) so that pool workers go idle and park, independent of OS timer
/// granularity.
fn idle_for(d: Duration) {
    let start = Instant::now();
    while start.elapsed() < d {
        std::hint::spin_loop();
    }
}

fn main() {
    // `cargo bench` passes `--bench`; ignore arguments.
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .max(2);
    let engine = ThreadPool::builder().workers(threads - 1).build().unwrap();
    engine.register_current_thread().unwrap();
    let rayon = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();

    let mut rows = Vec::new();

    // 1. Fine-grained recursive join: overhead per fork.
    {
        let n = 30;
        let seq = sequential(10, || {
            black_box(fib_seq(black_box(n)));
        });
        let (ours, theirs) = compare(
            &rayon,
            10,
            5,
            || {
                black_box(fib_engine(&engine, black_box(n)));
            },
            || {
                black_box(fib_rayon(black_box(n)));
            },
        );
        rows.push(Row {
            case: "join: fib(30), sequential below 12",
            ours,
            rayon: theirs,
            sequential: Some(seq),
        });
    }

    // 2. Data-parallel loop, even work.
    {
        let len = 1 << 20;
        let grain = 4096;
        let chunks = len / grain;
        let partial: Vec<AtomicU64> = (0..chunks).map(|_| AtomicU64::new(0)).collect();
        let work = |chunk: usize, start: usize, end: usize| {
            let mut acc = 0u64;
            for i in start..end {
                acc = acc.wrapping_add(busy(i as u64, 8));
            }
            partial[chunk].store(acc, Ordering::Relaxed);
        };
        let seq = sequential(5, || {
            for c in 0..chunks {
                work(c, c * grain, (c + 1) * grain);
            }
        });
        let (ours, theirs) = compare(
            &rayon,
            10,
            5,
            || engine.parallel_for(len, grain, |c, r| work(c, r.start, r.end)),
            || {
                use rayon::prelude::*;
                (0..chunks)
                    .into_par_iter()
                    .for_each(|c| work(c, c * grain, (c + 1) * grain));
            },
        );
        rows.push(Row {
            case: "parallel_for: 1M items, 256 chunks, even",
            ours,
            rayon: theirs,
            sequential: Some(seq),
        });
    }

    // 3. Data-parallel loop, uneven work (triangular cost).
    {
        let chunks = 256;
        let work = |c: usize| black_box(busy(c as u64, (c as u32 + 1) * 400));
        let seq = sequential(5, || {
            for c in 0..chunks {
                work(c);
            }
        });
        let (ours, theirs) = compare(
            &rayon,
            10,
            5,
            || {
                engine.parallel_for(chunks, 1, |c, _| {
                    work(c);
                })
            },
            || {
                use rayon::prelude::*;
                (0..chunks).into_par_iter().for_each(|c| {
                    work(c);
                });
            },
        );
        rows.push(Row {
            case: "parallel_for: 256 chunks, triangular cost",
            ours,
            rayon: theirs,
            sequential: Some(seq),
        });
    }

    // 4. Many tiny spawns in a scope.
    {
        let spawns = 256;
        let counter = AtomicUsize::new(0);
        let (ours, theirs) = compare(
            &rayon,
            20,
            20,
            || {
                engine.scope(|s| {
                    for i in 0..spawns {
                        let counter = &counter;
                        s.spawn(move || {
                            counter.fetch_add(busy(i, 50) as usize & 1, Ordering::Relaxed);
                        });
                    }
                })
            },
            || {
                rayon::scope(|s| {
                    for i in 0..spawns {
                        let counter = &counter;
                        s.spawn(move |_| {
                            counter.fetch_add(busy(i, 50) as usize & 1, Ordering::Relaxed);
                        });
                    }
                })
            },
        );
        rows.push(Row {
            case: "scope: 256 tiny spawns (~50 ns each)",
            ours,
            rayon: theirs,
            sequential: None,
        });
    }

    // 5. Wake-up latency: pool idle (workers parked) for 3 ms, then a small burst.
    //    Only the burst is timed.
    {
        let chunks = threads * 2;
        let idle = Duration::from_millis(3);
        let ours_times = std::cell::RefCell::new(Vec::new());
        let theirs_times = std::sync::Mutex::new(Vec::new());
        let (_, _) = compare(
            &rayon,
            10,
            10,
            || {
                idle_for(idle);
                let start = Instant::now();
                engine.parallel_for(chunks, 1, |c, _| {
                    black_box(busy(c as u64, 2_000));
                });
                ours_times.borrow_mut().push(start.elapsed());
            },
            || {
                use rayon::prelude::*;
                idle_for(idle);
                let start = Instant::now();
                (0..chunks).into_par_iter().for_each(|c| {
                    black_box(busy(c as u64, 2_000));
                });
                theirs_times.lock().unwrap().push(start.elapsed());
            },
        );
        rows.push(Row {
            case: "cold burst: 3 ms idle, then 2 chunks/thread of ~2 µs",
            ours: stats(ours_times.into_inner(), 0),
            rayon: stats(theirs_times.into_inner().unwrap(), 0),
            sequential: None,
        });
    }

    print_report(threads, &rows);
}
