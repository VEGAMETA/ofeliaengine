//! Sleep accuracy on Windows: `std::thread::sleep` vs [`HighResTimer`] vs
//! [`PreciseSleeper`], for frame-pacing-sized waits. Prints Markdown (posted by the Bench
//! workflow). Error = actual wait − requested wait.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

#[cfg(not(windows))]
fn main() {
    println!("engine_platform timer benchmarks run on Windows only.");
}

#[cfg(windows)]
fn main() {
    use engine_platform::cpu::CpuTopology;
    use engine_platform::time::{HighResTimer, PreciseSleeper};
    use std::time::{Duration, Instant};

    const SAMPLES: usize = 200;
    let targets_us = [500u64, 1_000, 2_000, 4_167, 16_667];

    fn stats(mut errors_us: Vec<f64>) -> (f64, f64, f64) {
        errors_us.sort_by(|a, b| a.total_cmp(b));
        let n = errors_us.len();
        (errors_us[n / 2], errors_us[n * 99 / 100], errors_us[n - 1])
    }

    let topology = CpuTopology::query().unwrap();
    println!("### Sleep accuracy");
    println!();
    println!(
        "CPU: {}. {SAMPLES} samples per cell; error = actual − requested, in µs (median / p99 / max).",
        topology.summary()
    );
    println!();
    println!(
        "| Requested | std::thread::sleep | HighResTimer | PreciseSleeper | PreciseSleeper margin / spin median |"
    );
    println!("|---|---|---|---|---|");

    let timer = HighResTimer::new().unwrap();
    let mut precise = PreciseSleeper::new().unwrap();
    for &target in &targets_us {
        let d = Duration::from_micros(target);

        let mut std_err = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let start = Instant::now();
            #[allow(clippy::disallowed_methods)] // the baseline being compared against
            std::thread::sleep(d);
            std_err.push((start.elapsed().as_secs_f64() - d.as_secs_f64()) * 1e6);
        }

        let mut timer_err = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let start = Instant::now();
            timer.sleep(d).unwrap();
            timer_err.push((start.elapsed().as_secs_f64() - d.as_secs_f64()) * 1e6);
        }

        // Deadline-based, as a frame limiter uses it.
        let mut precise_err = Vec::with_capacity(SAMPLES);
        let mut spins = Vec::with_capacity(SAMPLES);
        let mut deadline = Instant::now();
        for _ in 0..SAMPLES {
            deadline += d;
            let margin = precise.margin();
            precise.sleep_until(deadline).unwrap();
            let now = Instant::now();
            precise_err.push((now.saturating_duration_since(deadline)).as_secs_f64() * 1e6);
            spins.push(margin.as_secs_f64() * 1e6);
            // Re-anchor if we fell behind (should not happen) so errors don't accumulate.
            if now > deadline + d {
                deadline = now;
            }
        }

        let (s50, s99, smax) = stats(std_err);
        let (t50, t99, tmax) = stats(timer_err);
        let (p50, p99, pmax) = stats(precise_err);
        let (m50, _, _) = stats(spins);
        println!(
            "| {:.3} ms | {s50:.1} / {s99:.1} / {smax:.1} | {t50:.1} / {t99:.1} / {tmax:.1} | {p50:.2} / {p99:.2} / {pmax:.2} | {m50:.0} µs |",
            target as f64 / 1000.0
        );
    }
    println!();
    println!(
        "PreciseSleeper spins for at most its margin per wait (learned from the timer's worst recent overshoot + 20 µs)."
    );
}
