//! Plays a small mix through the default output device and prints a Markdown report of the
//! device and the audio thread's timing, for three configurations: the default 5 ms
//! low-latency period, the driver's minimum period, and the standard (~10 ms) path.
//!
//! The mix: a looping music bed ducked by a sidechain compressor, beeps scheduled on the
//! mixer clock through a filtered bus with a delay send, a limiter on the master, plus 60
//! quiet voices so the render cost is not trivial.
//!
//! ```text
//! cargo run -p engine_audio --example tone --release -- [--seconds 4] [--mute]
//! ```
//!
//! `--mute` silences the output after mixing (the device, the thread and the mixer run
//! exactly as with sound; CI uses it).

#![allow(clippy::print_stdout, clippy::unwrap_used)]

#[cfg(not(windows))]
fn main() {
    println!("The output device layer is Windows-only; the mixer is tested by `cargo test`.");
}

#[cfg(windows)]
fn main() {
    use engine_audio::dsp::{Biquad, Block, Compressor, Delay, Effect, Limiter};
    use engine_audio::output::{Output, OutputConfig, OutputMode};
    use engine_audio::{Audio, AudioConfig, BusId, Clip, PlayParams, SendTap};
    use std::time::{Duration, Instant};

    struct Silence;
    impl Effect for Silence {
        fn prepare(&mut self, _: u32) {}
        fn set_param(&mut self, _: u32, _: f32) {}
        fn process(&mut self, block: Block<'_>) {
            block.left.fill(0.0);
            block.right.fill(0.0);
        }
    }

    let args: Vec<String> = std::env::args().collect();
    let seconds: f64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(4.0);
    let mute = args.iter().any(|a| a == "--mute");

    let rate = 48_000u32;
    let tone = |freq: f32, secs: f32, amp: f32| -> Vec<f32> {
        let n = (secs * rate as f32) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let env = (1.0 - t / secs).max(0.0);
                (t * freq * core::f32::consts::TAU).sin() * amp * env
            })
            .collect()
    };
    let chord: Vec<f32> = (0..rate as usize * 2)
        .flat_map(|i| {
            let t = i as f32 / rate as f32;
            let s = |f: f32| (t * f * core::f32::consts::TAU).sin();
            let l = (s(220.0) + s(277.18) + s(329.63)) * 0.08;
            let r = (s(220.0) + s(277.18) + s(329.63) * 0.9) * 0.08;
            [l, r]
        })
        .collect();
    let bed = Clip::from_interleaved(rate, 2, &chord).unwrap();
    let beep = Clip::from_interleaved(rate, 1, &tone(880.0, 0.12, 0.4)).unwrap();
    let hum = Clip::from_interleaved(rate, 1, &tone(110.0, 1.0, 0.002)).unwrap();

    let configs = [
        ("low latency, 5 ms requested", OutputConfig::default()),
        (
            "low latency, driver minimum",
            OutputConfig {
                period_ms: 0.0,
                ..OutputConfig::default()
            },
        ),
        (
            "standard (IAudioClient::Initialize)",
            OutputConfig {
                period_ms: 10.0,
                low_latency: false,
                ..OutputConfig::default()
            },
        ),
        (
            "standard, whole buffer queued",
            OutputConfig {
                period_ms: 10.0,
                low_latency: false,
                queue_margin_ms: None,
            },
        ),
    ];

    let mut device_rows = Vec::new();
    let mut timing_rows = Vec::new();
    for (name, config) in configs {
        let (mut audio, renderer) = Audio::new(&AudioConfig::default());
        let mut output = Output::start(renderer, config).unwrap();
        let info = output.info().clone();

        let music = audio.add_bus("music", BusId::MASTER).unwrap();
        let sfx = audio.add_bus("sfx", BusId::MASTER).unwrap();
        let echo = audio.add_bus("echo", BusId::MASTER).unwrap();
        let duck = audio
            .add_effect(music, Compressor::new(-40.0, 8.0))
            .unwrap();
        audio.set_sidechain(duck, Some(sfx)).unwrap();
        audio
            .add_effect(sfx, Biquad::low_pass(4_000.0, 0.8))
            .unwrap();
        audio.add_effect(echo, Delay::new(0.18, 0.35, 1.0)).unwrap();
        audio.add_send(sfx, echo, 0.3, SendTap::PostFader).unwrap();
        audio.add_effect(BusId::MASTER, Limiter::new(-1.0)).unwrap();
        if mute {
            audio.add_effect(BusId::MASTER, Silence).unwrap();
        }
        audio
            .play_with(
                &bed,
                &PlayParams {
                    bus: music,
                    looping: true,
                    fade_in: 0.2,
                    ..PlayParams::default()
                },
            )
            .unwrap();
        for i in 0..60 {
            audio
                .play_with(
                    &hum,
                    &PlayParams {
                        bus: sfx,
                        looping: true,
                        pitch: 1.0 + i as f32 * 0.01,
                        pan: (i % 9) as f32 / 4.0 - 1.0,
                        ..PlayParams::default()
                    },
                )
                .unwrap();
        }

        let start = Instant::now();
        let mut next_beep = audio.clock().frames_after(0.2);
        let mut peak_real = 0;
        let timer = engine_platform::time::HighResTimer::new().unwrap();
        while start.elapsed().as_secs_f64() < seconds {
            let clock = audio.clock();
            // Schedule beeps half a second ahead on the mixer clock: sample-accurate.
            while next_beep < clock.frames_after(0.5) {
                let _ = audio.play_with(
                    &beep,
                    &PlayParams {
                        bus: sfx,
                        start_at: Some(next_beep),
                        pan: if next_beep % 2 == 0 { -0.5 } else { 0.5 },
                        ..PlayParams::default()
                    },
                );
                next_beep += u64::from(clock.sample_rate.max(1)) * 3 / 8;
            }
            audio.update();
            peak_real = peak_real.max(audio.voice_counts().0);
            // A 60 Hz "game frame".
            timer.sleep(Duration::from_micros(16_667)).unwrap();
        }
        let stats = output.stats();
        let frames_per_ms = f64::from(info.sample_rate) / 1000.0;
        let ms = |frames: u32| f64::from(frames) / frames_per_ms;
        device_rows.push(format!(
            "| {name} | {} | {:?} | {} Hz, {} ch, {:?} | {} ({:.2} ms) | {} ({:.2} ms) | {} ({:.2} ms) | {} | {} |",
            if info.device.is_empty() {
                "—"
            } else {
                info.device.as_str()
            },
            info.mode,
            info.sample_rate,
            info.channels,
            info.format,
            info.period_frames,
            ms(info.period_frames),
            info.buffer_frames,
            ms(info.buffer_frames),
            info.queue_frames,
            ms(info.queue_frames),
            info.engine_periods.map_or(String::from("—"), |p| format!(
                "default {} / fundamental {} / min {} / max {}",
                p.default, p.fundamental, p.min, p.max
            )),
            info.note.as_deref().unwrap_or("—"),
        ));
        if info.mode == OutputMode::NoDevice {
            timing_rows.push(format!("| {name} | no device | | | | | | |"));
        } else {
            timing_rows.push(format!(
                "| {name} | {} | {:.2} / {:.2} / {:.2} | {} / {} / {} | {} / {} | {:.0} ({:.2} ms) | {} | {} |",
                stats.callbacks,
                f64::from(stats.interval.percentile_us(0.5)) / 1e3,
                f64::from(stats.interval.percentile_us(0.99)) / 1e3,
                f64::from(stats.interval.max_us()) / 1e3,
                stats.render.percentile_us(0.5),
                stats.render.percentile_us(0.99),
                stats.render.max_us(),
                stats.empty_wakes,
                stats.late_wakes,
                stats.average_padding_frames,
                stats.average_padding_frames / frames_per_ms,
                stats.reopens,
                peak_real,
            ));
        }
        drop(output);
        audio.update();
    }

    println!(
        "### Audio output (default render device{})\n",
        if mute { ", muted" } else { "" }
    );
    println!(
        "| configuration | device | mode | format | period (frames) | buffer (frames) | kept queued (frames) | engine periods (frames) | note |"
    );
    println!("|---|---|---|---|---:|---:|---:|---|---|");
    for row in &device_rows {
        println!("{row}");
    }
    println!("\n### Audio thread timing ({seconds} s each, 61 voices, 5 effects)\n");
    println!(
        "| configuration | wakes | interval p50 / p99 / max ms | render p50 / p99 / max µs | empty / late wakes | padding at wake | reopens | mixed voices |"
    );
    println!("|---|---:|---|---|---|---|---:|---:|");
    for row in &timing_rows {
        println!("{row}");
    }
}
