//! Cost of the logging hot path on the calling thread (ADR 0061).
//!
//! `cargo bench -p engine_log --bench log` prints Markdown. With
//! `--features engine_profiler/tracy` the Tracy message is included.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_log::LogConfig;
use std::hint::black_box;
use std::time::Instant;

const ITERATIONS: u32 = 1_000_000;

fn per_iteration_ns(mut f: impl FnMut(u32)) -> f64 {
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
    let _guard = engine_log::init(&LogConfig {
        filter: Some(String::from("info,engine_render=debug,bench::off=off")),
        // Sinks run on the writer thread; the numbers are the caller's cost.
        stderr: false,
        debugger: false,
        file: Some(std::env::temp_dir().join("engine_log_bench.log")),
        tracy: true,
        capacity: 4096,
    })
    .unwrap();
    let baseline = per_iteration_ns(|i| {
        black_box(i);
    });
    let disabled_level = per_iteration_ns(|i| {
        log::trace!("never {}", black_box(i));
    });
    let disabled_target = per_iteration_ns(|i| {
        log::info!(target: "bench::off", "never {}", black_box(i));
    });
    let literal = per_iteration_ns(|i| {
        black_box(i);
        log::info!("a short literal message");
    });
    let formatted = per_iteration_ns(|i| {
        log::info!(
            target: "engine_render::graph",
            "frame {i}: pass {} took {:.3} ms",
            "forward",
            f64::from(i) * 0.01
        );
    });
    let long = "z".repeat(900);
    let long_record = per_iteration_ns(|i| {
        log::debug!(target: "engine_render", "{i} {long}");
    });
    let threads = 4;
    let contended = {
        let start = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    for i in 0..ITERATIONS {
                        log::info!(
                            "frame {i}: pass {} took {:.3} ms",
                            "forward",
                            f64::from(i) * 0.01
                        );
                    }
                });
            }
        });
        start.elapsed().as_nanos() as f64 / f64::from(ITERATIONS)
    };
    engine_log::flush();
    let stats = engine_log::stats();
    println!("| call | ns per call |");
    println!("|---|---|");
    println!("| loop overhead | {baseline:.2} |");
    println!("| disabled level (`trace!` under `info`) | {disabled_level:.2} |");
    println!("| disabled target (`bench::off=off`) | {disabled_target:.2} |");
    println!("| literal message, 23 bytes | {literal:.2} |");
    println!("| formatted: integer, string, float (~50 bytes) | {formatted:.2} |");
    println!("| 900-byte record (4 ring entries) | {long_record:.2} |");
    println!(
        "| formatted, {threads} threads at once (wall time per call per thread) | {contended:.2} |"
    );
    println!(
        "\ntracy compiled in: {}; entries {}, lost {} (the file writer falls behind a flood)",
        engine_profiler::ENABLED,
        stats.entries,
        stats.lost
    );
}
