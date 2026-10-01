//! The FLIP port against NVIDIA's own result: its test pair (`images/reference.png`,
//! `images/test.png` of github.com/NVlabs/flip, 4 and 5.5 MB, not committed) has a mean
//! LDR-FLIP of 0.159691 at the default viewing conditions (`src/tests/test.py`). Point
//! `ENGINE_FLIP_IMAGES` at a folder with the two files to run it.

#![allow(clippy::print_stdout)]

use engine_golden::flip::{DEFAULT_PPD, flip};
use engine_golden::golden::{read_png, to_linear};
use std::path::Path;

#[test]
fn matches_the_reference_implementation() {
    let Ok(dir) = std::env::var("ENGINE_FLIP_IMAGES") else {
        println!("skipped: set ENGINE_FLIP_IMAGES to a folder with NVlabs/flip's images");
        return;
    };
    let (w, h, reference) = read_png(&Path::new(&dir).join("reference.png")).unwrap();
    let (tw, th, test) = read_png(&Path::new(&dir).join("test.png")).unwrap();
    assert_eq!((w, h), (tw, th));
    let start = std::time::Instant::now();
    let result = flip(
        &to_linear(&reference),
        &to_linear(&test),
        w as usize,
        h as usize,
        DEFAULT_PPD,
    );
    let mean = result.mean();
    println!(
        "{w}×{h}: mean {mean:.6} in {:.0} ms",
        start.elapsed().as_secs_f64() * 1e3
    );
    assert!((mean - 0.159_691).abs() < 5e-6, "mean {mean}");
}
