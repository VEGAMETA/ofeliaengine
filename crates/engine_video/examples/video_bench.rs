//! Decoding speed of AV1 WebM files (ADR 0100): the headers parsed by `engine_video`'s
//! parser (µs per temporal unit) and rav1d's decoding with several thread settings, as
//! pictures per second and multiples of real time, with the slowest unit's time when one
//! frame is decoded at a time (the latency a player sees). Prints Markdown.
//!
//! `cargo run -p engine_video --release --example video_bench -- [files] [--threads 1,4,0]
//! [--frame-delay 1,0] [--runs 3]` (default: the test clips; 0 threads: one per logical
//! core; frame delay 0: rav1d's choice, 1: one frame at a time). Each setting decodes the
//! whole file `--runs` times and keeps the fastest run.

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::time::Instant;

use engine_video::av1::{Parser, TemporalUnit};
use engine_video::software::{Settings, SoftwareDecoder};
use engine_video::webm::{TrackKind, Webm};

fn list(value: &str) -> Vec<u32> {
    value
        .split(',')
        .map(|v| v.trim().parse().expect("a list of numbers"))
        .collect()
}

fn main() {
    let mut files = Vec::new();
    let (mut threads, mut delays, mut runs) = (vec![1, 4, 0], vec![1, 0], 3);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--threads" => threads = list(&args.next().expect("--threads 1,4,0")),
            "--frame-delay" => delays = list(&args.next().expect("--frame-delay 1,0")),
            "--runs" => runs = args.next().expect("--runs 3").parse().expect("a number"),
            _ => files.push(arg),
        }
    }
    if files.is_empty() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/clips");
        for name in [
            "svt_8bit_320x180.webm",
            "aom_10bit_tiles_320x180.webm",
            "svt_grain_tiles_640x360.webm",
            "rav1e_8bit_320x180.webm",
        ] {
            files.push(format!("{dir}/{name}"));
        }
    }
    println!(
        "| file | size | Mbit/s | parse µs/unit | threads | frame delay | pictures/s | × real time | slowest unit ms |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|");
    for file in &files {
        let data = std::fs::read(file).unwrap_or_else(|error| panic!("{file}: {error}"));
        let webm = Webm::read(&data).unwrap();
        let video = webm.track(TrackKind::Video).expect("a video track");
        let units: Vec<(&[u8], i64)> = webm
            .blocks()
            .map(Result::unwrap)
            .filter(|b| b.track == video.number)
            .map(|b| (b.data(), b.time_ns))
            .collect();
        let bytes: usize = units.iter().map(|(d, _)| d.len()).sum();
        // The stream's length: its first to its last unit, plus one frame.
        let deltas: Vec<i64> = units.windows(2).map(|w| w[1].1 - w[0].1).collect();
        let frame_ns = if video.default_duration_ns > 0 {
            video.default_duration_ns as f64
        } else {
            let mut sorted = deltas.clone();
            sorted.sort_unstable();
            sorted.get(sorted.len() / 2).copied().unwrap_or(33_333_333) as f64
        };
        let seconds = ((units.last().unwrap().1 - units[0].1) as f64 + frame_ns) / 1e9;

        // The headers: every unit parsed, the fastest of several passes.
        let mut unit = TemporalUnit::default();
        let mut parse_us = f64::MAX;
        for _ in 0..runs.max(5) {
            let mut parser = Parser::with_codec_private(&video.private).unwrap();
            let start = Instant::now();
            for (data, _) in &units {
                parser.parse(data, &mut unit).unwrap();
            }
            parse_us = parse_us.min(start.elapsed().as_secs_f64() * 1e6 / units.len() as f64);
        }

        let name = std::path::Path::new(file)
            .file_name()
            .unwrap()
            .to_string_lossy();
        let size = format!("{}×{}", video.width, video.height);
        for &thread_count in &threads {
            for &delay in &delays {
                if thread_count == 1 && delay != 1 {
                    continue; // one thread decodes one frame at a time anyway
                }
                let settings = Settings {
                    threads: thread_count,
                    max_frame_delay: delay,
                    apply_grain: true,
                    output_invisible_frames: false,
                };
                let (mut best, mut slowest, mut pictures) = (f64::MAX, 0.0f64, 0);
                for _ in 0..runs {
                    let mut decoder = SoftwareDecoder::new(&settings).unwrap();
                    let (mut count, mut worst) = (0usize, 0.0f64);
                    let start = Instant::now();
                    for &(data, time) in &units {
                        let unit_start = Instant::now();
                        decoder.decode(data, time, |_| count += 1).unwrap();
                        worst = worst.max(unit_start.elapsed().as_secs_f64());
                    }
                    decoder.finish(|_| count += 1).unwrap();
                    let total = start.elapsed().as_secs_f64();
                    if total < best {
                        (best, slowest, pictures) = (total, worst, count);
                    }
                }
                let per_second = pictures as f64 / best;
                println!(
                    "| {name} | {size} | {:.1} | {parse_us:.1} | {} | {} | {per_second:.0} | {:.1} | {} |",
                    bytes as f64 * 8.0 / seconds / 1e6,
                    if thread_count == 0 {
                        "all".to_owned()
                    } else {
                        thread_count.to_string()
                    },
                    if delay == 0 {
                        "auto".to_owned()
                    } else {
                        delay.to_string()
                    },
                    seconds / best,
                    if delay == 1 {
                        format!("{:.2}", slowest * 1e3)
                    } else {
                        "–".to_owned()
                    },
                );
            }
        }
    }
}
