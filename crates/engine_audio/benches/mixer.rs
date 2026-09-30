//! CPU cost of the mixer per 256-frame block (5.33 ms at 48 kHz): voices at the output
//! rate (straight copy), resampled voices, stereo 44.1 kHz clips, virtual voices, the
//! built-in effects, and a game-like graph. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_audio --bench mixer
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_audio::dsp::{Biquad, Block, Compressor, Delay, Effect, Limiter, Reverb};
use engine_audio::{
    Audio, AudioConfig, BusId, Clip, DecodeError, Decoder, Emitter, PlayParams, Renderer, SendTap,
};
use engine_core::math::Vec3;
use std::time::Instant;

const BLOCK: usize = 256;
const BUDGET_US: f64 = BLOCK as f64 / 48_000.0 * 1e6;

/// Median microseconds per `render` of one block, over `runs` blocks after a warm-up.
fn time_render(renderer: &mut Renderer, runs: usize) -> f64 {
    let mut out = vec![0.0f32; BLOCK * 2];
    for _ in 0..20 {
        renderer.render(&mut out, 2);
    }
    let mut samples: Vec<f64> = (0..runs)
        .map(|_| {
            let start = Instant::now();
            renderer.render(&mut out, 2);
            start.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn noise(frames: usize, channels: usize) -> Vec<f32> {
    (0..frames * channels)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 8) as f32 / 16_777_216.0 - 0.5)
        .collect()
}

fn voices(count: u32, clip: &Clip, pitch: f32, max_real: u32) -> Renderer {
    let (mut audio, renderer) = Audio::new(&AudioConfig {
        max_voices: count.max(1),
        max_real_voices: max_real,
        ..AudioConfig::default()
    });
    for i in 0..count {
        audio
            .play_with(
                clip,
                &PlayParams {
                    pitch,
                    looping: true,
                    gain: 0.01,
                    pan: (i % 17) as f32 / 8.0 - 1.0,
                    ..PlayParams::default()
                },
            )
            .unwrap();
    }
    // The game side must outlive nothing here: the renderer owns everything it needs.
    std::mem::forget(audio);
    renderer
}

/// `count` looping spatial voices on a ring 5..80 m around the listener, each moving (so
/// every one is resampled for the Doppler effect, and the far ones low-passed).
fn spatial_voices(count: u32, clip: &Clip, max_real: u32) -> Renderer {
    let (mut audio, renderer) = Audio::new(&AudioConfig {
        max_voices: count.max(1),
        max_real_voices: max_real,
        ..AudioConfig::default()
    });
    for i in 0..count {
        let angle = i as f32 * 2.399;
        let distance = 5.0 + 75.0 * (i as f32 / count.max(1) as f32);
        let position = Vec3::new(angle.cos(), 0.0, angle.sin()) * distance;
        audio
            .play_with(
                clip,
                &PlayParams {
                    looping: true,
                    gain: 0.05,
                    spatial: Some(Emitter {
                        velocity: Vec3::new(-angle.sin(), 0.0, angle.cos()) * 10.0
                            + position.normalize() * 3.0,
                        ..Emitter::at(position)
                    }),
                    ..PlayParams::default()
                },
            )
            .unwrap();
    }
    std::mem::forget(audio);
    renderer
}

/// A stereo decoder of noise, `frames` long: the stream path without codec cost.
struct NoiseDecoder {
    frames: u64,
    pos: u64,
}

impl Decoder for NoiseDecoder {
    fn sample_rate(&self) -> u32 {
        48_000
    }

    fn channels(&self) -> u16 {
        2
    }

    fn frames(&self) -> Option<u64> {
        Some(self.frames)
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
        let n = ((out.len() / 2) as u64).min(self.frames - self.pos) as usize;
        let noise = noise(n, 2);
        out[..n * 2].copy_from_slice(&noise);
        self.pos += n as u64;
        Ok(n)
    }

    fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        self.pos = frame.min(self.frames);
        Ok(())
    }
}

/// `count` stereo streams, decoded ahead in full (a 5 s buffer, 2 s streams), so the
/// timing is the ring reading of the audio thread alone.
fn streams(count: u32) -> Renderer {
    let (mut audio, renderer) = Audio::new(&AudioConfig {
        max_voices: count.max(1),
        max_real_voices: count,
        stream_buffer_ms: 5_000,
        ..AudioConfig::default()
    });
    for i in 0..count {
        audio
            .play_stream(
                Box::new(NoiseDecoder {
                    frames: 96_000,
                    pos: 0,
                }),
                &PlayParams {
                    gain: 0.01,
                    pan: (i % 17) as f32 / 8.0 - 1.0,
                    ..PlayParams::default()
                },
            )
            .unwrap();
    }
    std::mem::forget(audio);
    renderer
}

fn row(name: &str, us: f64) {
    println!("| {name} | {us:.1} | {:.2} % |", us / BUDGET_US * 100.0);
}

fn stereo48() -> Clip {
    Clip::from_interleaved(48_000, 2, &noise(96_000, 2)).unwrap()
}

fn main() {
    let mono = Clip::from_interleaved(48_000, 1, &noise(48_000, 1)).unwrap();
    let stereo = Clip::from_interleaved(44_100, 2, &noise(44_100, 2)).unwrap();

    println!("### Mixer CPU cost per 256-frame block ({BUDGET_US:.0} µs of audio at 48 kHz)\n");
    println!("| case | µs per block (median) | share of real time |");
    println!("|---|---:|---:|");
    row(
        "empty (master only)",
        time_render(&mut voices(0, &mono, 1.0, 128), 2000),
    );
    for count in [32u32, 128, 512] {
        row(
            &format!("{count} mono voices, output rate (copy)"),
            time_render(&mut voices(count, &mono, 1.0, count), 500),
        );
        row(
            &format!("{count} mono voices, pitch 1.37 (linear interpolation)"),
            time_render(&mut voices(count, &mono, 1.37, count), 500),
        );
        row(
            &format!("{count} stereo 44.1 kHz voices (resampled)"),
            time_render(&mut voices(count, &stereo, 1.0, count), 500),
        );
    }
    row(
        "32 stereo streams, output rate (ring reads)",
        time_render(&mut streams(32), 300),
    );
    row(
        "32 stereo clips, output rate (copy), for comparison",
        time_render(&mut voices(32, &stereo48(), 1.0, 32), 300),
    );
    row(
        "128 spatial mono voices (distance, pan, Doppler, low-pass)",
        time_render(&mut spatial_voices(128, &mono, 128), 500),
    );
    row(
        "1024 spatial voices, 64 real + 960 virtual",
        time_render(&mut spatial_voices(1024, &mono, 64), 500),
    );
    row(
        "1024 voices, 64 real + 960 virtual",
        time_render(&mut voices(1024, &mono, 1.0, 64), 500),
    );

    // A game-like graph: 8 category buses with EQ and compression, a reverb send,
    // a ducking sidechain, a master limiter; 128 voices spread over the categories.
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let reverb = audio.add_bus("reverb", BusId::MASTER).unwrap();
    audio.add_effect(reverb, Reverb::new(1.8, 0.6)).unwrap();
    let mut categories = Vec::new();
    for i in 0..8 {
        let bus = audio
            .add_bus(&format!("category {i}"), BusId::MASTER)
            .unwrap();
        audio
            .add_effect(bus, Biquad::peak(1_000.0, 1.0, 3.0))
            .unwrap();
        audio.add_effect(bus, Compressor::new(-18.0, 3.0)).unwrap();
        audio
            .add_send(bus, reverb, 0.2, SendTap::PostFader)
            .unwrap();
        categories.push(bus);
    }
    let duck = audio
        .add_effect(categories[0], Compressor::new(-30.0, 6.0))
        .unwrap();
    audio.set_sidechain(duck, Some(categories[1])).unwrap();
    audio.add_effect(BusId::MASTER, Limiter::new(-1.0)).unwrap();
    for i in 0..128usize {
        let clip = if i % 4 == 0 { &stereo } else { &mono };
        audio
            .play_with(
                clip,
                &PlayParams {
                    bus: categories[i % 8],
                    pitch: 1.0 + (i % 5) as f32 * 0.05,
                    looping: true,
                    gain: 0.01,
                    ..PlayParams::default()
                },
            )
            .unwrap();
    }
    row(
        "game graph: 128 voices, 9 buses, 26 effects, 8 sends, sidechain, limiter",
        time_render(&mut renderer, 500),
    );

    println!("\n### Effects, µs per 256-frame stereo block (median)\n");
    println!("| effect | µs | ns per frame |");
    println!("|---|---:|---:|");
    let effects: Vec<(&str, Box<dyn Effect>)> = vec![
        (
            "Biquad (low-pass)",
            Box::new(Biquad::low_pass(2_000.0, 0.7)),
        ),
        ("Compressor", Box::new(Compressor::new(-20.0, 4.0))),
        ("Limiter (1.5 ms lookahead)", Box::new(Limiter::new(-1.0))),
        (
            "Delay (fractional, feedback)",
            Box::new(Delay::new(0.25, 0.4, 0.3)),
        ),
        ("Reverb (FDN, 8 lines)", Box::new(Reverb::new(2.0, 0.6))),
    ];
    let input = noise(BLOCK, 2);
    for (name, mut effect) in effects {
        effect.prepare(48_000);
        let mut l = input[..BLOCK].to_vec();
        let mut r = input[BLOCK..].to_vec();
        let mut samples: Vec<f64> = (0..2000)
            .map(|_| {
                l.copy_from_slice(&input[..BLOCK]);
                r.copy_from_slice(&input[BLOCK..]);
                let start = Instant::now();
                effect.process(Block {
                    left: &mut l,
                    right: &mut r,
                    sidechain: None,
                });
                start.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        samples.sort_by(f64::total_cmp);
        let us = samples[samples.len() / 2];
        println!("| {name} | {us:.2} | {:.1} |", us * 1e3 / BLOCK as f64);
    }
}
