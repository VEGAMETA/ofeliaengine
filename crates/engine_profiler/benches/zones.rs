//! Cost of instrumentation with Tracy compiled in (no profiler connected, so events are
//! queued in the client, as during start-up before the profiler attaches).
//!
//! `cargo bench -p engine_profiler --features tracy --bench zones` prints Markdown.

#![allow(missing_docs, clippy::print_stdout)]

use std::hint::black_box;
use std::time::Instant;

const ITERATIONS: u32 = 1_000_000;

fn per_iteration_ns(mut f: impl FnMut(u32)) -> f64 {
    // Warm-up also creates the per-call-site source locations (one allocation each).
    for i in 0..10_000 {
        f(i);
    }
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for i in 0..ITERATIONS {
            f(i);
        }
        best = best.min(start.elapsed().as_nanos() as f64 / f64::from(ITERATIONS));
    }
    best
}

fn main() {
    let baseline = per_iteration_ns(|i| {
        black_box(i);
    });
    let zone = per_iteration_ns(|i| {
        engine_profiler::zone!("bench::zone");
        black_box(i);
    });
    let nested = per_iteration_ns(|i| {
        engine_profiler::zone!("bench::outer");
        {
            engine_profiler::zone!("bench::inner");
            black_box(i);
        }
    });
    let plot = per_iteration_ns(|i| {
        engine_profiler::plot!("bench::plot", i);
    });

    println!("### Profiler instrumentation cost");
    println!();
    let mode = if cfg!(feature = "tracy-ondemand") {
        "on-demand mode (events dropped while no profiler is connected)"
    } else {
        "capture-from-start mode (events buffered in memory while no profiler is connected)"
    };
    let connected = if engine_profiler::is_connected() {
        "connected"
    } else {
        "not connected"
    };
    println!(
        "Tracy 0.14.1 client, {mode}, profiler {connected}. Best of 5 × {ITERATIONS} iterations, baseline subtracted."
    );
    println!();
    println!("| Operation | ns per call |");
    println!("|---|---|");
    println!("| empty loop (baseline) | {baseline:.2} |");
    println!("| `zone!` | {:.2} |", zone - baseline);
    println!("| two nested `zone!` | {:.2} |", nested - baseline);
    println!("| `plot!` | {:.2} |", plot - baseline);
}
