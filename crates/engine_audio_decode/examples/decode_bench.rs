//! Decoding speed of each format on the test files (0.5 s each): opening (headers) and
//! steady decoding, as multiples of real time. Seeking decodes from the start, so a seek
//! to position `t` costs `t / (multiple of real time)`. Prints Markdown.
//!
//! `cargo run -p engine_audio_decode --release --example decode_bench`

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use std::time::Instant;

fn main() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data");
    println!("| file | open µs | decode µs per second of audio | × real time |");
    println!("|---|---:|---:|---:|");
    for name in [
        "stereo_44100.ogg",
        "stereo_48000.opus",
        "stereo_44100_s16.flac",
        "stereo_48000_s24.flac",
    ] {
        let bytes: std::sync::Arc<[u8]> = std::fs::read(format!("{dir}/{name}")).unwrap().into();

        let mut opens: Vec<f64> = (0..200)
            .map(|_| {
                let bytes = bytes.clone();
                let start = Instant::now();
                let decoder = engine_audio_decode::open_bytes(bytes).unwrap();
                let us = start.elapsed().as_secs_f64() * 1e6;
                drop(decoder);
                us
            })
            .collect();
        opens.sort_by(f64::total_cmp);

        let mut decoder = engine_audio_decode::open_bytes(bytes.clone()).unwrap();
        let rate = f64::from(decoder.sample_rate());
        let mut buffer = vec![0.0f32; 4096 * usize::from(decoder.channels())];
        let (mut frames, start) = (0u64, Instant::now());
        while start.elapsed().as_secs_f64() < 1.0 {
            loop {
                let n = decoder.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                frames += n as u64;
            }
            decoder.seek(0).unwrap();
        }
        let seconds = start.elapsed().as_secs_f64();
        let audio = frames as f64 / rate;
        println!(
            "| {name} | {:.0} | {:.0} | {:.0} |",
            opens[opens.len() / 2],
            seconds / audio * 1e6,
            audio / seconds
        );
    }
}
