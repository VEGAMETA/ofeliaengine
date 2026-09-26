//! CPU cost of the 2D canvas: recording quads, and sorting + batching them (`prepare`)
//! for input already in order (one layer, no y-sorting) and for y-sorted input. The
//! y-sorted case is compared with the standard library's sorts on the same keys. Prints
//! Markdown.
//!
//! ```text
//! cargo bench -p engine_render2d --bench canvas
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::Vec2;
use engine_render2d::{Canvas, Order, Quad};
use std::hint::black_box;
use std::time::Instant;

/// Median time of `f` in nanoseconds over `runs` runs.
fn time(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut samples: Vec<f64> = (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1e9
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn position(i: usize) -> Vec2 {
    Vec2::new(
        (i as f32 * 0.618_034).fract() * 1920.0,
        (i as f32 * 0.414_214).fract() * 1080.0,
    )
}

fn record(canvas: &mut Canvas, count: usize, y_sorted: bool) {
    canvas.clear();
    for i in 0..count {
        let p = position(i);
        let order = if y_sorted {
            Order::y_sorted(0, p.y)
        } else {
            Order::layer(0)
        };
        canvas.draw(&Quad::sprite((i % 4) as u32, p, Vec2::splat(8.0)).order(order));
    }
}

/// The sort key `Order::y_sorted` produces, for the standard-library comparison.
fn key(y: f32) -> u64 {
    let bits = (-y).to_bits();
    let ordered = if bits & 0x8000_0000 != 0 {
        !bits
    } else {
        bits | 0x8000_0000
    };
    (0x8000u64 << 32) | u64::from(ordered)
}

fn main() {
    println!("### 2D canvas CPU cost\n");
    println!(
        "| quads | record ns/quad | prepare, in order ns/quad | prepare, y-sorted ns/quad | std sort_unstable ns/quad | std sort (stable, allocates) ns/quad | gather 64 B ns/quad | copy + order ns/quad |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|");
    let mut canvas = Canvas::new();
    for count in [1_000usize, 10_000, 100_000, 1_000_000] {
        let runs = (20_000_000 / count).clamp(5, 200);
        let per = |ns: f64| ns / count as f64;

        let record_ns = time(runs, || record(&mut canvas, count, false));
        record(&mut canvas, count, false);
        let in_order = time(runs, || canvas.prepare());
        assert!(canvas.stats().presorted);

        // `prepare` sorts in place: re-record before each run, subtract the recording.
        let y_sorted = time(runs, || {
            record(&mut canvas, count, true);
            canvas.prepare();
        }) - time(runs, || record(&mut canvas, count, true));
        assert!(!canvas.stats().presorted);

        let keys: Vec<(u64, u32)> = (0..count).map(|i| (key(position(i).y), i as u32)).collect();
        let mut scratch = keys.clone();
        let unstable = time(runs, || {
            scratch.copy_from_slice(&keys);
            scratch.sort_unstable();
            black_box(&scratch);
        });
        let stable = time(runs, || {
            scratch.copy_from_slice(&keys);
            scratch.sort_by_key(|e| e.0);
            black_box(&scratch);
        });
        // Writing sorted instances for the GPU: gathering 64-byte instances in draw order
        // (random reads) versus copying them in submission order plus a 4-byte draw-order
        // array (what the renderer does).
        scratch.copy_from_slice(&keys);
        scratch.sort_unstable();
        let order: Vec<u32> = scratch.iter().map(|e| e.1).collect();
        let instances: Vec<[u64; 8]> = (0..count as u64).map(|i| [i; 8]).collect();
        let mut out = vec![[0u64; 8]; count];
        let mut out_order = vec![0u32; count];
        let gather = time(runs, || {
            for (slot, &i) in out.iter_mut().zip(&order) {
                *slot = instances[i as usize];
            }
            black_box(&out);
        });
        let copy = time(runs, || {
            out.copy_from_slice(&instances);
            out_order.copy_from_slice(&order);
            black_box((&out, &out_order));
        });
        println!(
            "| {count} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
            per(record_ns),
            per(in_order),
            per(y_sorted),
            per(unstable),
            per(stable),
            per(gather),
            per(copy)
        );
    }
    println!();
}
