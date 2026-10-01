//! `flip <reference.png> <test.png> [error-map.png]`: the mean, 99.9th percentile and
//! largest LDR-FLIP error of `test` against `reference` (8-bit sRGB PNGs of one size),
//! and optionally the error map as an image.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use engine_golden::flip::{DEFAULT_PPD, flip};
use engine_golden::golden::{read_png, to_linear, write_png};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: flip <reference.png> <test.png> [error-map.png]");
        return ExitCode::from(2);
    }
    let read = |p: &str| {
        read_png(Path::new(p)).map_err(|e| {
            eprintln!("{p}: {e}");
        })
    };
    let (Ok((w, h, reference)), Ok((tw, th, test))) = (read(&args[0]), read(&args[1])) else {
        return ExitCode::FAILURE;
    };
    if (w, h) != (tw, th) {
        eprintln!("sizes differ: {w}×{h} and {tw}×{th}");
        return ExitCode::FAILURE;
    }
    let result = flip(
        &to_linear(&reference),
        &to_linear(&test),
        w as usize,
        h as usize,
        DEFAULT_PPD,
    );
    println!(
        "mean {:.6}, 99.9th percentile {:.4}, max {:.4}",
        result.mean(),
        result.percentile(0.999),
        result.max()
    );
    if let Some(out) = args.get(2)
        && let Err(e) = write_png(Path::new(out), w, h, &result.heatmap_rgb8())
    {
        eprintln!("{out}: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
