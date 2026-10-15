//! Playing videos (ADR 0100) on the GPU `ENGINE_ADAPTER` picks, under the validation
//! layer with synchronization validation: every picture shown once and in order, seeks
//! that land on their picture, the sound's position as the clock, pause, loops and speed,
//! and every test clip played to its end (the GPU handing over to rav1d where it refuses
//! a frame).
//!
//! Frames are simulated at 60 Hz; without a GPU the tests print why and pass, unless
//! `ENGINE_REQUIRE_GPU=1`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::undocumented_unsafe_blocks,
    // Sleeping lets the decoding and audio threads keep up with simulated frames.
    clippy::disallowed_methods
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Bed, codes_of, compare, gpu, reference};
use engine_audio::{Audio, AudioConfig, Renderer};
use engine_video::convert::{Conversion, Planes};
use engine_video::player::{Tick, VideoDesc, VideoEvent, VideoPlayer};
use engine_video::software::{Settings, SoftwareDecoder};
use engine_video::webm::{TrackKind, Webm};

const FRAME: f64 = 1.0 / 60.0;
/// Audio frames mixed per simulated frame.
const MIXED: usize = 800;

fn clip(name: &str) -> Arc<[u8]> {
    let path = format!("{}/tests/clips/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap().into()
}

/// A video and the frames run so far.
struct Run {
    video: VideoPlayer,
    frame: u64,
    shown: Vec<f64>,
    ended: bool,
    looped: usize,
}

impl Run {
    fn open(bed: &mut Bed, name: &str, desc: &VideoDesc) -> Self {
        let video = VideoPlayer::open(
            &bed.device,
            &mut bed.allocator,
            &bed.bindless,
            clip(name),
            desc,
            1,
        )
        .unwrap();
        Self {
            video,
            frame: 0,
            shown: Vec::new(),
            ended: false,
            looped: 0,
        }
    }

    /// One frame of `dt` seconds; mixes `MIXED` frames first when there is sound.
    fn frame(
        &mut self,
        bed: &mut Bed,
        sound: Option<(&mut Audio, &mut Renderer)>,
        dt: f64,
        read: bool,
    ) -> Option<Vec<[u8; 4]>> {
        self.frame += 1;
        let audio = sound.map(|(audio, renderer)| {
            let mut out = vec![0.0f32; MIXED * 2];
            renderer.render(&mut out, 2);
            audio.update();
            audio
        });
        self.video
            .update(
                &bed.device,
                &mut bed.allocator,
                &bed.bindless,
                Tick {
                    frame: self.frame,
                    in_flight: 1,
                    dt,
                    audio,
                    latency: 0.0,
                },
            )
            .unwrap();
        let texels = bed.show(&mut self.video, self.frame, read);
        for event in self.video.events() {
            match event {
                VideoEvent::Shown(time) => self.shown.push(time),
                VideoEvent::Ended => self.ended = true,
                VideoEvent::Looped => self.looped += 1,
            }
        }
        texels
    }

    /// Frames until the end (at most `limit`), sleeping `pause` after each so a software
    /// decoder keeps up with the simulated clock.
    fn play_to_end(&mut self, bed: &mut Bed, limit: usize, pause: Duration) {
        for _ in 0..limit {
            self.frame(bed, None, FRAME, false);
            if self.ended {
                return;
            }
            std::thread::sleep(pause);
        }
    }

    fn destroy(self, bed: &mut Bed, audio: Option<&mut Audio>) {
        unsafe {
            self.video.destroy(
                &bed.device,
                &mut bed.allocator,
                &bed.bindless,
                audio,
                self.frame,
            )
        };
    }
}

fn increasing(times: &[f64]) -> bool {
    times.windows(2).all(|w| w[0] < w[1])
}

#[test]
fn every_picture_shows_once_in_order() {
    let Some((instance, device)) = gpu(true) else {
        return;
    };
    let mut bed = Bed::new(device);
    for on_gpu in [true, false] {
        let desc = VideoDesc {
            sound: false,
            gpu: on_gpu,
            ..VideoDesc::default()
        };
        let mut run = Run::open(&mut bed, "svt_8bit_320x180.webm", &desc);
        let gpu = run.video.decodes_on_gpu();
        let pause = if gpu {
            Duration::ZERO
        } else {
            Duration::from_millis(4)
        };
        run.play_to_end(&mut bed, 400, pause);
        println!(
            "{}: {} pictures shown, ended at {:.3} s",
            if gpu { "GPU" } else { "rav1d" },
            run.shown.len(),
            run.video.time()
        );
        assert!(run.ended, "the video did not end");
        assert!(increasing(&run.shown), "{:?}", run.shown);
        assert_eq!(run.shown.first().copied(), Some(0.0));
        if gpu {
            // 60 pictures at 30 a second, frames at 60: each once.
            assert_eq!(run.shown.len(), 60);
        } else {
            // The thread keeps up on a quiet machine; under load it may skip a few.
            assert!(run.shown.len() >= 50, "{} pictures", run.shown.len());
        }
        assert!(run.video.time() >= run.video.duration());
        run.destroy(&mut bed, None);
    }
    assert_eq!(bed.finish(&instance), 0, "validation errors");
}

#[test]
fn a_seek_shows_the_picture_at_its_time() {
    let Some((instance, device)) = gpu(true) else {
        return;
    };
    let name = "svt_8bit_320x180.webm";
    // rav1d's pictures, for the picture the texture must hold.
    let data = clip(name);
    let webm = Webm::read(&data).unwrap();
    let track = webm.track(TrackKind::Video).unwrap();
    let mut software = SoftwareDecoder::new(&Settings {
        threads: 1,
        max_frame_delay: 1,
        apply_grain: true,
        output_invisible_frames: false,
    })
    .unwrap();
    let mut pictures = Vec::new();
    for block in webm.blocks().map(Result::unwrap) {
        if block.track == track.number {
            software
                .decode(block.data(), block.time_ns, |p| pictures.push(p))
                .unwrap();
        }
    }
    let mut bed = Bed::new(device);
    for on_gpu in [true, false] {
        let desc = VideoDesc {
            sound: false,
            autoplay: false,
            gpu: on_gpu,
            ..VideoDesc::default()
        };
        let mut run = Run::open(&mut bed, name, &desc);
        let sequence = *run.video.sequence();
        let size = run.video.texture().size();
        let conversion = Conversion::new(&sequence.color, size[0], size[1]);
        // Paused, it shows its first picture.
        for _ in 0..10 {
            run.frame(&mut bed, None, FRAME, false);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(run.shown, [0.0]);
        for (target, index) in [(1.0, 30), (0.5, 15), (1.9, 57)] {
            run.shown.clear();
            run.video.seek(target);
            let mut texels = None;
            for _ in 0..40 {
                texels = run.frame(&mut bed, None, FRAME, true);
                if !run.shown.is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(run.shown.len(), 1, "seek to {target}: {:?}", run.shown);
            let shown = run.shown[0];
            let expected = pictures[index].timestamp() as f64 * 1e-9;
            assert!(
                (shown - expected).abs() < 1e-9,
                "seek to {target}: shown {shown}, picture {index} at {expected}"
            );
            let theirs = reference(
                &codes_of(&Planes::from(&pictures[index])),
                &conversion,
                size,
            );
            if let Err(e) = compare(&texels.unwrap(), &theirs, size[0]) {
                panic!("seek to {target}: the texture is not picture {index}: {e}");
            }
            // Paused: the clock and the picture stay.
            for _ in 0..5 {
                run.frame(&mut bed, None, FRAME, false);
            }
            assert_eq!(run.shown.len(), 1);
            assert!((run.video.time() - shown).abs() < 1e-9);
        }
        println!(
            "{}: seeks land on their pictures",
            if run.video.decodes_on_gpu() {
                "GPU"
            } else {
                "rav1d"
            }
        );
        run.destroy(&mut bed, None);
    }
    assert_eq!(bed.finish(&instance), 0, "validation errors");
}

#[test]
fn the_sound_runs_the_clock() {
    let Some((instance, device)) = gpu(true) else {
        return;
    };
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let mut bed = Bed::new(device);
    let mut run = Run::open(&mut bed, "svt_8bit_320x180.webm", &VideoDesc::default());
    let mut mixed = 0usize;
    let frames =
        |run: &mut Run, bed: &mut Bed, audio: &mut Audio, renderer: &mut Renderer, n: usize| {
            for _ in 0..n {
                run.frame(bed, Some((audio, renderer)), FRAME, false);
                // The stream thread decodes ahead of the mix.
                std::thread::sleep(Duration::from_millis(2));
            }
        };
    // The clock waits for the first picture, then follows the mix.
    frames(&mut run, &mut bed, &mut audio, &mut renderer, 3);
    let start = run.video.time();
    frames(&mut run, &mut bed, &mut audio, &mut renderer, 45);
    mixed += 45 * MIXED;
    let heard = mixed as f64 / 48_000.0;
    let moved = run.video.time() - start;
    println!("after {heard:.3} s mixed the clock moved {moved:.3} s");
    assert!(
        (moved - heard).abs() < 0.05,
        "the clock moved {moved} s while {heard} s were mixed"
    );
    // The picture shown is the one due.
    let last = *run.shown.last().unwrap();
    assert!(last <= run.video.time() && run.video.time() - last < 0.034 + 0.02);
    assert_eq!(
        audio.stream_underruns(),
        0,
        "the stream fell behind the mix"
    );

    // Paused, the clock stands while the mix goes on.
    run.video.pause();
    frames(&mut run, &mut bed, &mut audio, &mut renderer, 2);
    let paused = run.video.time();
    let shown = run.shown.len();
    frames(&mut run, &mut bed, &mut audio, &mut renderer, 20);
    assert_eq!(run.video.time(), paused);
    assert_eq!(run.shown.len(), shown);
    // Playing again, it goes on from there.
    run.video.play();
    frames(&mut run, &mut bed, &mut audio, &mut renderer, 30);
    let moved = run.video.time() - paused;
    let heard = 29.0 * MIXED as f64 / 48_000.0;
    assert!(
        (moved - heard).abs() < 0.05,
        "after the pause the clock moved {moved} s while {heard} s were mixed"
    );
    run.destroy(&mut bed, Some(&mut audio));
    assert_eq!(bed.finish(&instance), 0, "validation errors");
}

#[test]
fn loops_and_speed() {
    let Some((instance, device)) = gpu(true) else {
        return;
    };
    let mut bed = Bed::new(device);
    // Looping: back to the start at the end, never ended.
    let mut run = Run::open(
        &mut bed,
        "svt_8bit_320x180.webm",
        &VideoDesc {
            sound: false,
            looping: true,
            ..VideoDesc::default()
        },
    );
    for _ in 0..200 {
        run.frame(&mut bed, None, FRAME, false);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!run.ended);
    assert_eq!(run.looped, 1);
    let back = run.shown.windows(2).filter(|w| w[1] < w[0]).count();
    assert_eq!(back, 1, "the pictures start over once");
    run.destroy(&mut bed, None);

    // Twice as fast: a picture a frame, the two seconds in one.
    let mut run = Run::open(
        &mut bed,
        "svt_8bit_320x180.webm",
        &VideoDesc {
            sound: false,
            ..VideoDesc::default()
        },
    );
    run.video.set_speed(2.0);
    let pause = if run.video.decodes_on_gpu() {
        Duration::ZERO
    } else {
        Duration::from_millis(4)
    };
    run.play_to_end(&mut bed, 100, pause);
    assert!(run.ended);
    println!("at twice the speed: ended after {} frames", run.frame);
    assert!(run.frame <= 66, "{} frames", run.frame);
    run.destroy(&mut bed, None);
    assert_eq!(bed.finish(&instance), 0, "validation errors");
}

#[test]
fn every_clip_plays_to_its_end() {
    let Some((instance, device)) = gpu(true) else {
        return;
    };
    let clips = [
        "svt_8bit_320x180.webm",
        "aom_10bit_tiles_320x180.webm",
        "svt_grain_tiles_640x360.webm",
        "rav1e_8bit_320x180.webm",
        "aom_tools_320x180.webm",
        "aom_resilient_320x180.webm",
        "aom_lossless_160x90.webm",
        "svt_superres_320x180.webm",
        "svt_resize_320x180.webm",
        "svt_superres_320x176.webm",
        "svt_resize_320x176.webm",
    ];
    let mut bed = Bed::new(device);
    for name in clips {
        let mut run = Run::open(
            &mut bed,
            name,
            &VideoDesc {
                sound: false,
                ..VideoDesc::default()
            },
        );
        let started = run.video.decodes_on_gpu();
        run.play_to_end(&mut bed, 400, Duration::from_millis(2));
        let finished = run.video.decodes_on_gpu();
        let decoder = match (started, finished) {
            (true, true) => "GPU",
            (true, false) => "GPU, then rav1d",
            _ => "rav1d",
        };
        println!(
            "{name}: {} pictures shown ({decoder}), {:.3} s",
            run.shown.len(),
            run.video.duration()
        );
        assert!(run.ended, "{name} did not end");
        assert!(increasing(&run.shown), "{name}: {:?}", run.shown);
        assert!(run.shown.len() >= 5, "{name}: {} pictures", run.shown.len());
        run.destroy(&mut bed, None);
    }
    assert_eq!(bed.finish(&instance), 0, "validation errors");
}
