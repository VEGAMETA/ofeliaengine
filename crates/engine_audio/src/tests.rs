#![allow(clippy::unwrap_used, clippy::float_cmp)]

use crate::dsp::{Biquad, Block, Compressor, Delay, Effect, Limiter, db_to_gain};
use crate::{
    Audio, AudioConfig, AudioError, BusId, Clip, ClipError, Emitter, Listener, PlayParams, Rolloff,
    SendTap,
};
use engine_core::math::Vec3;

fn mono(samples: &[f32]) -> Clip {
    Clip::from_interleaved(48_000, 1, samples).unwrap()
}

fn render(renderer: &mut crate::Renderer, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0; frames * 2];
    renderer.render(&mut out, 2);
    out
}

fn left(out: &[f32]) -> Vec<f32> {
    out.chunks_exact(2).map(|f| f[0]).collect()
}

fn right(out: &[f32]) -> Vec<f32> {
    out.chunks_exact(2).map(|f| f[1]).collect()
}

const CENTER: f32 = core::f32::consts::FRAC_1_SQRT_2;

#[test]
fn silence_without_voices() {
    let (_audio, mut renderer) = Audio::new(&AudioConfig::default());
    assert!(render(&mut renderer, 1000).iter().all(|&x| x == 0.0));
    assert_eq!(renderer.clock(), 1000);
}

#[test]
fn one_shot_plays_exactly_and_ends() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let samples: Vec<f32> = (0..300).map(|i| i as f32 / 300.0).collect();
    let v = audio
        .play_with(
            &mono(&samples),
            &PlayParams {
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    let out = render(&mut renderer, 400);
    let l = left(&out);
    // Hard left: the left gain is cos(0) = 1, so the samples come out unchanged.
    for i in 0..300 {
        assert!(
            (l[i] - samples[i]).abs() < 1e-6,
            "{i}: {} vs {}",
            l[i],
            samples[i]
        );
    }
    assert!(l[300..].iter().all(|&x| x == 0.0));
    assert!(right(&out).iter().all(|&x| x.abs() < 1e-6));

    assert!(audio.is_playing(v));
    audio.update();
    assert!(!audio.is_playing(v), "the end came back to the game thread");
}

#[test]
fn centre_pan_is_constant_power() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    audio.play(&mono(&[1.0; 100])).unwrap();
    let out = render(&mut renderer, 50);
    assert!((out[0] - CENTER).abs() < 1e-6 && (out[1] - CENTER).abs() < 1e-6);
}

#[test]
fn pitch_resamples_with_interpolation() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let ramp: Vec<f32> = (0..1000).map(|i| i as f32 * 1e-3).collect();
    audio
        .play_with(
            &mono(&ramp),
            &PlayParams {
                pitch: 0.5,
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    let l = left(&render(&mut renderer, 100));
    // Half speed over a linear ramp: 0, 0.5, 1, 1.5, ... (thousandths)
    for (i, x) in l.iter().enumerate() {
        assert!((x - i as f32 * 0.5e-3).abs() < 1e-6, "{i}: {x}");
    }

    // A clip at a different rate plays at its own speed.
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let clip = Clip::from_interleaved(24_000, 1, &ramp).unwrap();
    audio
        .play_with(
            &clip,
            &PlayParams {
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    let l = left(&render(&mut renderer, 2100));
    assert!((l[10] - 5e-3).abs() < 1e-6);
    // 1000 frames at 24 kHz last 2000 frames at 48 kHz (the last one fades to silence).
    assert!(l[1998] > 0.0 && l[2000] == 0.0);
}

#[test]
fn looping_wraps_seamlessly() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let samples = [0.1, 0.2, 0.3, 0.4, 0.5];
    let v = audio
        .play_with(
            &mono(&samples),
            &PlayParams {
                looping: true,
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    let l = left(&render(&mut renderer, 23));
    for (i, x) in l.iter().enumerate() {
        assert!((x - samples[i % 5]).abs() < 1e-6);
    }
    audio.update();
    assert!(audio.is_playing(v));
    audio.stop(v, 0.0);
    render(&mut renderer, 256);
    audio.update();
    assert!(!audio.is_playing(v));
}

#[test]
fn start_at_is_sample_accurate() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    render(&mut renderer, 100);
    let start = audio.clock().frames + 1234;
    audio
        .play_with(
            &mono(&[1.0; 10]),
            &PlayParams {
                start_at: Some(start),
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    let l = left(&render(&mut renderer, 2000));
    let first = l.iter().position(|&x| x != 0.0).unwrap();
    assert_eq!(100 + first as u64, start);
    assert_eq!(l[first..first + 10], [1.0; 10]);
}

#[test]
fn late_start_skips_ahead_to_stay_in_time() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    render(&mut renderer, 500);
    let ramp: Vec<f32> = (0..1000).map(|i| i as f32 * 1e-3).collect();
    audio
        .play_with(
            &mono(&ramp),
            &PlayParams {
                start_at: Some(400),
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    let l = left(&render(&mut renderer, 10));
    assert!(
        (l[0] - 0.1).abs() < 1e-6,
        "started 100 frames late, so 100 frames in: {}",
        l[0]
    );
}

#[test]
fn stop_fades_out_without_a_click() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let v = audio
        .play_with(
            &mono(&[1.0; 48_000]),
            &PlayParams {
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    render(&mut renderer, 100);
    audio.stop(v, 0.01);
    let l = left(&render(&mut renderer, 1000));
    assert!(l[0] > 0.99);
    for w in l.windows(2) {
        assert!(w[1] <= w[0] + 1e-6, "monotonic fade");
        assert!(w[0] - w[1] < 0.01, "no step larger than a 480-frame ramp");
    }
    assert_eq!(l[480], 0.0);
    audio.update();
    assert!(!audio.is_playing(v));
}

#[test]
fn bus_gain_ramps_and_mute() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let sfx = audio.add_bus("sfx", BusId::MASTER).unwrap();
    assert_eq!(audio.bus_by_name("sfx"), Some(sfx));
    audio
        .play_with(
            &mono(&[1.0; 48_000]),
            &PlayParams {
                bus: sfx,
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    render(&mut renderer, 64);
    audio.set_bus_gain(sfx, 0.5, 0.0);
    let l = left(&render(&mut renderer, 200));
    // Default declick ramp: 64 frames from 1 to 0.5.
    assert!(l[0] > 0.99 && (l[32] - 0.75).abs() < 0.01 && l[100] == 0.5);

    assert_eq!(audio.voice_counts(), (1, 0));
    audio.set_bus_mute(sfx, true);
    let l = left(&render(&mut renderer, 200));
    assert!(l[100] == 0.0);
    // Muted bus: the voice fades out and stays virtual.
    assert_eq!(audio.voice_counts(), (0, 1));
}

#[test]
fn virtual_voices_keep_time_and_come_back() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let ramp: Vec<f32> = (0..10_000).map(|i| i as f32 * 1e-4).collect();
    let v = audio
        .play_with(
            &mono(&ramp),
            &PlayParams {
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    render(&mut renderer, 100);
    audio.set_voice_gain(v, 0.0, 0.0);
    render(&mut renderer, 100); // fades out; the voice goes virtual after this call
    render(&mut renderer, 1000);
    assert_eq!(audio.voice_counts(), (0, 1));
    audio.set_voice_gain(v, 1.0, 0.0);
    let l = left(&render(&mut renderer, 200));
    assert_eq!(audio.voice_counts(), (1, 0));
    // It resumes where it would have been, fading in over the declick ramp.
    assert!((l[199] - 0.1399).abs() < 1e-6, "{}", l[199]);
}

#[test]
fn real_voice_limit_keeps_the_most_important() {
    let config = AudioConfig {
        max_real_voices: 2,
        ..AudioConfig::default()
    };
    let (mut audio, mut renderer) = Audio::new(&config);
    let clip = mono(&[1.0; 48_000]);
    for (priority, gain) in [(10, 1.0), (200, 0.1), (10, 0.5), (200, 0.2)] {
        audio
            .play_with(
                &clip,
                &PlayParams {
                    priority,
                    gain,
                    pan: -1.0,
                    ..PlayParams::default()
                },
            )
            .unwrap();
    }
    let l = left(&render(&mut renderer, 10));
    // The two priority-200 voices win despite being quieter.
    assert!((l[0] - 0.3).abs() < 1e-5, "{}", l[0]);
    assert_eq!(audio.voice_counts(), (2, 2));

    // A per-bus limit applies on top.
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let steps = audio.add_bus("footsteps", BusId::MASTER).unwrap();
    audio.set_bus_voice_limit(steps, Some(1));
    for gain in [0.25, 0.5] {
        audio
            .play_with(
                &clip,
                &PlayParams {
                    bus: steps,
                    gain,
                    pan: -1.0,
                    ..PlayParams::default()
                },
            )
            .unwrap();
    }
    let l = left(&render(&mut renderer, 10));
    assert!((l[0] - 0.5).abs() < 1e-5, "the louder one: {}", l[0]);
}

#[test]
fn sends_and_parents_route_by_schedule() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let world = audio.add_bus("world", BusId::MASTER).unwrap();
    let reverb = audio.add_bus("reverb", BusId::MASTER).unwrap();
    let footsteps = audio.add_bus("footsteps", world).unwrap();
    audio
        .add_send(footsteps, reverb, 0.5, SendTap::PostFader)
        .unwrap();
    audio.set_bus_gain(reverb, 0.5, 0.0);
    audio
        .play_with(
            &mono(&[1.0; 1000]),
            &PlayParams {
                bus: footsteps,
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    render(&mut renderer, 64);
    audio.take_peak(reverb);
    let l = left(&render(&mut renderer, 10));
    // Direct 1.0 through world, plus 0.5 send × 0.5 reverb fader.
    assert!((l[0] - 1.25).abs() < 1e-5, "{}", l[0]);
    let [peak_l, _] = audio.take_peak(reverb);
    assert!((peak_l - 0.25).abs() < 1e-5);
    assert_eq!(audio.take_peak(reverb), [0.0, 0.0], "reset on read");

    // Cycles are rejected and leave the graph unchanged.
    assert_eq!(
        audio.add_send(reverb, footsteps, 1.0, SendTap::PostFader),
        Err(AudioError::Cycle)
    );
    assert_eq!(
        audio.set_bus_parent(world, footsteps),
        Err(AudioError::Cycle)
    );
    assert_eq!(audio.remove_bus(BusId::MASTER), Err(AudioError::Master));

    // Removing a bus moves its children up and keeps its voices on the master.
    audio.remove_bus(world).unwrap();
    let l = left(&render(&mut renderer, 10));
    assert!((l[0] - 1.25).abs() < 1e-5, "{}", l[0]);
}

#[test]
fn effects_process_in_order_and_come_back_to_be_freed() {
    struct Gain(f32);
    impl Effect for Gain {
        fn prepare(&mut self, _: u32) {}
        fn set_param(&mut self, _: u32, value: f32) {
            self.0 = value;
        }
        fn process(&mut self, block: Block<'_>) {
            for x in block.left.iter_mut().chain(block.right.iter_mut()) {
                *x = *x * self.0 + 0.25;
            }
        }
    }
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let a = audio.add_effect(BusId::MASTER, Gain(2.0)).unwrap();
    let b = audio.add_effect(BusId::MASTER, Gain(0.5)).unwrap();
    audio
        .play_with(
            &mono(&[1.0; 1000]),
            &PlayParams {
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    let l = left(&render(&mut renderer, 10));
    // (1 * 2 + 0.25) * 0.5 + 0.25
    assert!((l[0] - 1.375).abs() < 1e-6);
    audio.set_effect_param(a, 0, 1.0);
    audio.remove_effect(b).unwrap();
    let l = left(&render(&mut renderer, 10));
    assert!((l[0] - 1.25).abs() < 1e-6);
    assert_eq!(audio.remove_effect(b), Err(AudioError::Unknown("effect")));
}

#[test]
fn sidechain_compressor_ducks_music_under_dialogue() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let music = audio.add_bus("music", BusId::MASTER).unwrap();
    let dialogue = audio.add_bus("dialogue", BusId::MASTER).unwrap();
    let mut comp = Compressor::new(-30.0, 10.0);
    comp.set_param(Compressor::ATTACK_MS, 1.0);
    let duck = audio.add_effect(music, comp).unwrap();
    audio.set_sidechain(duck, Some(dialogue)).unwrap();
    audio
        .play_with(
            &mono(&[0.5; 48_000]),
            &PlayParams {
                bus: music,
                pan: -1.0,
                looping: true,
                ..PlayParams::default()
            },
        )
        .unwrap();
    let before = left(&render(&mut renderer, 4800));
    assert!((before[4799] - 0.5).abs() < 1e-3, "no dialogue: no ducking");
    audio
        .play_with(
            &mono(&[0.5; 48_000]),
            &PlayParams {
                bus: dialogue,
                pan: 1.0, // right only; the music is measured on the left
                ..PlayParams::default()
            },
        )
        .unwrap();
    let during = left(&render(&mut renderer, 4800));
    assert!(during[4799] < 0.1, "ducked to {}", during[4799]);
    // A sidechain from the ducked bus to itself would be a cycle.
    assert_eq!(
        audio.set_sidechain(duck, Some(music)),
        Err(AudioError::Cycle)
    );
}

#[test]
fn output_is_independent_of_render_call_sizes() {
    let make = || {
        let (mut audio, renderer) = Audio::new(&AudioConfig::default());
        let fx = audio.add_bus("fx", BusId::MASTER).unwrap();
        audio
            .add_effect(fx, Biquad::low_pass(3_000.0, 0.9))
            .unwrap();
        audio.add_effect(fx, Delay::new(0.003, 0.5, 0.4)).unwrap();
        audio.add_effect(BusId::MASTER, Limiter::new(-3.0)).unwrap();
        let noise: Vec<f32> = (0..20_000u32)
            .map(|i| ((i.wrapping_mul(2_654_435_761) >> 8) as f32 / 8_388_608.0) - 1.0)
            .collect();
        let clip = mono(&noise);
        for (k, pitch) in [1.0, 1.37, 0.61].into_iter().enumerate() {
            audio
                .play_with(
                    &clip,
                    &PlayParams {
                        bus: fx,
                        pitch,
                        pan: k as f32 * 0.4 - 0.4,
                        start_at: Some(k as u64 * 777),
                        fade_in: 0.01,
                        ..PlayParams::default()
                    },
                )
                .unwrap();
        }
        (audio, renderer)
    };
    let total = 12_000;
    let mut outputs = Vec::new();
    for sizes in [vec![256], vec![128], vec![480], vec![1, 7, 1000, 13]] {
        let (_audio, mut renderer) = make();
        let mut out = Vec::new();
        let mut i = 0;
        while out.len() < total * 2 {
            let n = sizes[i % sizes.len()].min(total - out.len() / 2);
            out.extend(render(&mut renderer, n));
            i += 1;
        }
        outputs.push(out);
    }
    assert!(outputs[0].iter().any(|&x| x.abs() > 0.1));
    for other in &outputs[1..] {
        assert_eq!(&outputs[0], other, "bit-identical for any device period");
    }
}

#[test]
fn limiter_never_exceeds_the_ceiling() {
    let mut limiter = Limiter::new(-1.0);
    limiter.prepare(48_000);
    let ceiling = db_to_gain(-1.0);
    let mut max: f32 = 0.0;
    for block in 0..200 {
        let mut l: Vec<f32> = (0..256)
            .map(|i| ((block * 256 + i) as f32 * 0.07).sin() * (1.0 + (block % 7) as f32))
            .collect();
        let mut r = l.clone();
        limiter.process(Block {
            left: &mut l,
            right: &mut r,
            sidechain: None,
        });
        max = l.iter().fold(max, |m, x| m.max(x.abs()));
    }
    assert!(max <= ceiling + 1e-6, "{max} > {ceiling}");
    assert!(max > ceiling * 0.9, "still loud: {max}");
    assert_eq!(limiter.latency(), 72);
}

#[test]
fn biquad_low_pass_attenuates_high_frequencies() {
    let tone = |freq: f32| {
        let mut f = Biquad::low_pass(1_000.0, 0.707);
        f.prepare(48_000);
        let mut peak: f32 = 0.0;
        for block in 0..40 {
            let mut l: Vec<f32> = (0..256)
                .map(|i| {
                    ((block * 256 + i) as f32 * freq * core::f32::consts::TAU / 48_000.0).sin()
                })
                .collect();
            let mut r = l.clone();
            f.process(Block {
                left: &mut l,
                right: &mut r,
                sidechain: None,
            });
            if block > 20 {
                peak = l.iter().fold(peak, |m, x| m.max(x.abs()));
            }
        }
        peak
    };
    assert!(tone(100.0) > 0.95);
    assert!(
        (tone(1_000.0) - core::f32::consts::FRAC_1_SQRT_2).abs() < 0.02,
        "-3 dB at cutoff"
    );
    assert!(tone(10_000.0) < 0.02);
}

#[test]
fn wav_decoding() {
    let bytes = crate::wav::encode_pcm16(22_050, 2, &[0, 16_384, -32_768, 32_767]);
    let clip = Clip::from_wav(&bytes).unwrap();
    assert_eq!(
        (clip.sample_rate(), clip.channels(), clip.frames()),
        (22_050, 2, 2)
    );
    assert_eq!(clip.channel(0), &[0.0, -1.0]);
    assert_eq!(clip.channel(1)[0], 0.5);
    assert_eq!(
        Clip::from_wav(b"RIFF\0\0\0\0WAVE").err(),
        Some(ClipError::Truncated)
    );
    assert_eq!(Clip::from_wav(b"nope").err(), Some(ClipError::NotWav));
    // Every truncation of a valid file is an error or a shorter clip, never a panic.
    for len in 0..bytes.len() {
        let _ = Clip::from_wav(&bytes[..len]);
    }
}

#[test]
fn stale_handles_are_ignored() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig {
        max_voices: 1,
        ..AudioConfig::default()
    });
    let clip = mono(&[1.0; 10]);
    let first = audio.play(&clip).unwrap();
    assert_eq!(audio.play(&clip), Err(AudioError::VoicesExhausted));
    render(&mut renderer, 64);
    audio.update();
    let second = audio.play(&clip).unwrap();
    assert!(!audio.is_playing(first) && audio.is_playing(second));
    // Stopping the old handle must not stop the new voice in the same slot.
    audio.stop(first, 0.0);
    let out = render(&mut renderer, 5);
    assert!(out[0] > 0.5);
}

fn looping_spatial(emitter: Emitter) -> PlayParams {
    PlayParams {
        looping: true,
        spatial: Some(emitter),
        ..PlayParams::default()
    }
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

#[test]
fn spatial_voice_is_panned_and_attenuated_by_distance() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    // 2 m to the right of the default listener (facing -Z): inverse rolloff from 1 m
    // halves it, and it is fully right.
    audio
        .play_with(
            &mono(&[1.0; 64]),
            &looping_spatial(Emitter::at(Vec3::new(2.0, 0.0, 0.0))),
        )
        .unwrap();
    let out = render(&mut renderer, 256);
    assert!(right(&out).iter().all(|&x| (x - 0.5).abs() < 1e-5));
    assert!(left(&out).iter().all(|&x| x.abs() < 1e-5));

    // Turned around (facing +Z) the same sound is on the left, after one ramp.
    audio.set_listener(&Listener {
        forward: Vec3::Z,
        ..Listener::default()
    });
    render(&mut renderer, 256);
    let out = render(&mut renderer, 256);
    assert!(left(&out).iter().all(|&x| (x - 0.5).abs() < 1e-5));
    assert!(right(&out).iter().all(|&x| x.abs() < 1e-5));
}

#[test]
fn a_jump_in_position_is_ramped_over_the_render_call() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let v = audio
        .play_with(
            &mono(&[1.0; 64]),
            &looping_spatial(Emitter::at(Vec3::new(-5.0, 0.0, 0.0))),
        )
        .unwrap();
    let mut out = render(&mut renderer, 256);
    audio.set_voice_position(v, Vec3::new(5.0, 0.0, 0.0), Vec3::ZERO);
    out.extend(render(&mut renderer, 256));
    out.extend(render(&mut renderer, 256));
    for channel in [left(&out), right(&out)] {
        let steepest = channel
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        // 0.2 (5 m) across 256 frames.
        assert!(steepest < 0.001, "{steepest}");
    }
    assert!(left(&out)[767].abs() < 1e-5);
    assert!((right(&out)[767] - 0.2).abs() < 1e-5);
}

#[test]
fn approaching_sounds_play_higher() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let sine: Vec<f32> = (0..48_000)
        .map(|i| (core::f32::consts::TAU * 1_000.0 * i as f32 / 48_000.0).sin())
        .collect();
    // 50 m ahead, coming closer at a tenth of the speed of sound: 1000 Hz / 0.9.
    audio
        .play_with(
            &mono(&sine),
            &looping_spatial(Emitter {
                velocity: Vec3::new(0.0, 0.0, 34.3),
                ..Emitter::at(Vec3::new(0.0, 0.0, -50.0))
            }),
        )
        .unwrap();
    let mut l = Vec::new();
    for _ in 0..(48_000 / 240) {
        l.extend(left(&render(&mut renderer, 240)));
    }
    let rising = l.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    assert!((rising as f32 - 1_000.0 / 0.9).abs() <= 2.0, "{rising}");
}

#[test]
fn sounds_beyond_a_linear_rolloff_become_virtual() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let v = audio
        .play_with(
            &mono(&[1.0; 64]),
            &looping_spatial(Emitter {
                rolloff: Rolloff::Linear,
                max_distance: 30.0,
                ..Emitter::at(Vec3::new(0.0, 0.0, -40.0))
            }),
        )
        .unwrap();
    assert!(render(&mut renderer, 256).iter().all(|&x| x == 0.0));
    assert_eq!(audio.voice_counts(), (0, 1));
    // Walking up to it brings it back.
    audio.set_listener(&Listener {
        position: Vec3::new(0.0, 0.0, -30.0),
        ..Listener::default()
    });
    render(&mut renderer, 256);
    assert_eq!(audio.voice_counts(), (1, 0));
    assert!(audio.is_playing(v));
}

#[test]
fn occlusion_muffles_and_lowers_the_sound() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    // The highest frequency there is (alternating samples), 1 m ahead.
    let buzz: Vec<f32> = (0..64)
        .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
        .collect();
    let v = audio
        .play_with(
            &mono(&buzz),
            &looping_spatial(Emitter::at(Vec3::new(0.0, 0.0, -1.0))),
        )
        .unwrap();
    let clear = rms(&left(&render(&mut renderer, 1024)));
    assert!((clear - CENTER).abs() < 1e-3, "{clear}");
    audio.set_voice_occlusion(v, 1.0);
    render(&mut renderer, 1024);
    let occluded = rms(&left(&render(&mut renderer, 1024)));
    // −15 dB of gain and a 600 Hz low-pass at the top of the spectrum: over 40 dB.
    assert!(occluded < clear * 0.01, "{occluded} vs {clear}");
}

#[test]
fn spatial_updates_keep_a_fade_in() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    audio
        .play_with(
            &mono(&[1.0; 64]),
            &PlayParams {
                fade_in: 0.5,
                ..looping_spatial(Emitter::at(Vec3::new(0.0, 0.0, -1.0)))
            },
        )
        .unwrap();
    let mut l = Vec::new();
    for _ in 0..4 {
        l.extend(left(&render(&mut renderer, 256)));
    }
    // 1024 of the fade's 24,000 frames: about 4% of the way to the centre level.
    let expected = CENTER * 1_024.0 / 24_000.0;
    assert!(
        (l[1023] - expected).abs() < 0.002,
        "{} vs {expected}",
        l[1023]
    );
}

#[test]
fn the_four_step_low_pass_matches_the_plain_recursion() {
    for a in [0.9f32, 0.3, 0.05, 0.001] {
        for len in [0usize, 1, 3, 4, 5, 7, 64, 255] {
            let input: Vec<f32> = (0..len)
                .map(|i| ((i * 37 % 11) as f32 - 5.0) / 5.0)
                .collect();
            let (mut plain, mut y) = (input.clone(), 0.25f32);
            for x in &mut plain {
                y += (*x - y) * a;
                *x = y;
            }
            let (mut fast, mut state) = (input, 0.25f32);
            crate::mixer::one_pole(&mut fast, &mut state, a);
            for (i, (p, f)) in plain.iter().zip(&fast).enumerate() {
                assert!((p - f).abs() < 1e-5, "a {a}, len {len}, {i}: {p} vs {f}");
            }
            assert!((state - y).abs() < 1e-5);
        }
    }
}

/// Waits for the stream thread in tests (real time on purpose: the thread polls on its
/// own clock).
#[allow(clippy::disallowed_methods)]
fn wait_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// A decoder of a known signal: frame `i` of channel 0 is `value(i % period)`, channel 1
/// its negative. Reports when it is dropped.
struct Synthetic {
    rate: u32,
    channels: u16,
    frames: u64,
    period: u64,
    pos: u64,
    dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

fn value(i: u64) -> f32 {
    (i as f32 + 1.0) / 16_384.0
}

impl Synthetic {
    fn new(rate: u32, channels: u16, frames: u64) -> Self {
        Self {
            rate,
            channels,
            frames,
            period: frames,
            pos: 0,
            dropped: std::sync::Arc::default(),
        }
    }
}

impl Drop for Synthetic {
    fn drop(&mut self) {
        self.dropped
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

impl crate::Decoder for Synthetic {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn frames(&self) -> Option<u64> {
        Some(self.frames)
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize, crate::DecodeError> {
        let channels = usize::from(self.channels);
        let n = ((out.len() / channels) as u64).min(self.frames - self.pos) as usize;
        for (f, frame) in out.chunks_exact_mut(channels).take(n).enumerate() {
            let v = value((self.pos + f as u64) % self.period);
            frame[0] = v;
            if channels == 2 {
                frame[1] = -v;
            }
        }
        self.pos += n as u64;
        Ok(n)
    }

    fn seek(&mut self, frame: u64) -> Result<(), crate::DecodeError> {
        self.pos = frame.min(self.frames);
        Ok(())
    }
}

#[test]
fn a_stream_plays_exactly_like_the_clip_it_decodes_to() {
    // 44.1 kHz stereo into 48 kHz: resampled with interpolation on both paths. The buffer
    // holds the whole stream, so it is decoded before `play_stream` returns.
    let (mut audio, mut renderer) = Audio::new(&AudioConfig {
        stream_buffer_ms: 2_000,
        ..AudioConfig::default()
    });
    let clip = Clip::from_decoder(&mut Synthetic::new(44_100, 2, 10_000)).unwrap();
    let params = PlayParams {
        pan: -1.0,
        ..PlayParams::default()
    };
    let from_clip = audio.play_with(&clip, &params).unwrap();
    let mut clip_out = Vec::new();
    for _ in 0..50 {
        clip_out.extend(render(&mut renderer, 256));
    }
    audio.update();
    assert!(!audio.is_playing(from_clip));

    let stream = audio
        .play_stream(Box::new(Synthetic::new(44_100, 2, 10_000)), &params)
        .unwrap();
    let mut stream_out = Vec::new();
    for _ in 0..50 {
        stream_out.extend(render(&mut renderer, 256));
    }
    audio.update();
    assert!(!audio.is_playing(stream));
    assert_eq!(audio.stream_underruns(), 0);
    let expected_frames = (10_000.0f64 * 48_000.0 / 44_100.0).ceil() as usize;
    for (i, (s, c)) in stream_out.iter().zip(&clip_out).enumerate() {
        assert!((s - c).abs() < 1e-6, "{i}: {s} vs {c}");
    }
    assert!(left(&stream_out)[expected_frames - 2] != 0.0);
    assert!(
        left(&stream_out)[expected_frames + 1..]
            .iter()
            .all(|&x| x == 0.0)
    );
}

#[test]
fn a_looping_stream_is_kept_filled_by_the_stream_thread() {
    // A 50 ms ring (4096 frames) and a 3000-frame loop: most of it is decoded while it
    // plays, across the loop point, by the stream thread.
    let (mut audio, mut renderer) = Audio::new(&AudioConfig {
        stream_buffer_ms: 50,
        ..AudioConfig::default()
    });
    let voice = audio
        .play_stream(
            Box::new(Synthetic::new(48_000, 1, 3_000)),
            &PlayParams {
                pan: -1.0,
                looping: true,
                ..PlayParams::default()
            },
        )
        .unwrap();
    let mut out = Vec::new();
    for _ in 0..24 {
        // 21 ms of audio every 40 ms: the thread (polling every 10 ms) keeps ahead.
        out.extend(left(&render(&mut renderer, 1_024)));
        wait_ms(40);
    }
    assert_eq!(audio.stream_underruns(), 0);
    for (i, &s) in out.iter().enumerate() {
        assert_eq!(s, value(i as u64 % 3_000), "frame {i}");
    }
    assert!(audio.is_playing(voice));
    assert_eq!(audio.stream_errors(), 0);
}

#[test]
fn a_starved_stream_waits_instead_of_skipping() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig {
        stream_buffer_ms: 50,
        ..AudioConfig::default()
    });
    audio
        .play_stream(
            Box::new(Synthetic::new(48_000, 1, 200_000)),
            &PlayParams {
                pan: -1.0,
                ..PlayParams::default()
            },
        )
        .unwrap();
    // A whole second at once: far more than the 4096-frame ring holds, so it starves.
    let mut out = left(&render(&mut renderer, 48_000));
    assert!(audio.stream_underruns() > 0);
    for _ in 0..8 {
        wait_ms(30);
        out.extend(left(&render(&mut renderer, 1_024)));
    }
    // Every sample of the signal is non-zero; starving adds silence. Without the
    // silence, the signal must come out whole: paused, never skipped.
    let heard: Vec<f32> = out.into_iter().filter(|&s| s != 0.0).collect();
    assert!(heard.len() > 8_000, "{}", heard.len());
    for (i, &s) in heard.iter().enumerate() {
        assert_eq!(s, value(i as u64), "sample {i}");
    }
}

#[test]
fn a_virtual_stream_keeps_time_and_ends() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig {
        stream_buffer_ms: 2_000,
        ..AudioConfig::default()
    });
    let muted = audio.add_bus("muted", BusId::MASTER).unwrap();
    audio.set_bus_mute(muted, true);
    let voice = audio
        .play_stream(
            Box::new(Synthetic::new(48_000, 2, 5_000)),
            &PlayParams {
                bus: muted,
                ..PlayParams::default()
            },
        )
        .unwrap();
    render(&mut renderer, 256);
    assert_eq!(audio.voice_counts(), (0, 1));
    for _ in 0..20 {
        render(&mut renderer, 256);
    }
    audio.update();
    assert!(!audio.is_playing(voice));
}

#[test]
fn stopping_a_stream_frees_its_decoder_on_the_stream_thread() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig {
        stream_buffer_ms: 50,
        ..AudioConfig::default()
    });
    let decoder = Synthetic::new(48_000, 1, 1_000_000);
    let dropped = decoder.dropped.clone();
    let voice = audio
        .play_stream(Box::new(decoder), &PlayParams::default())
        .unwrap();
    render(&mut renderer, 1_024);
    audio.stop(voice, 0.0);
    render(&mut renderer, 1_024);
    audio.update();
    assert!(!audio.is_playing(voice));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !dropped.load(std::sync::atomic::Ordering::Acquire) {
        assert!(std::time::Instant::now() < deadline, "decoder not dropped");
        wait_ms(5);
    }
}

#[test]
fn streams_with_unsupported_formats_are_refused() {
    let (mut audio, _renderer) = Audio::new(&AudioConfig::default());
    let result = audio.play_stream(
        Box::new(Synthetic::new(48_000, 6, 1_000)),
        &PlayParams::default(),
    );
    assert!(matches!(result, Err(AudioError::Decode(_))));
    let result = audio.play_stream(
        Box::new(Synthetic::new(0, 2, 1_000)),
        &PlayParams::default(),
    );
    assert!(matches!(result, Err(AudioError::Decode(_))));
}

#[test]
fn a_stream_reports_how_far_the_mix_has_taken_it() {
    let (mut audio, mut renderer) = Audio::new(&AudioConfig::default());
    let voice = audio
        .play_stream(
            Box::new(Synthetic::new(48_000, 1, 48_000)),
            &PlayParams::default(),
        )
        .unwrap();
    assert_eq!(audio.stream_position(voice), Some(0));
    for _ in 0..4 {
        render(&mut renderer, 256);
    }
    // The renderer runs at the stream's rate here: one frame of stream per frame mixed.
    assert_eq!(audio.stream_position(voice), Some(1024));
    // A clip is not a stream.
    let clip = Clip::from_interleaved(48_000, 1, &[0.0; 64]).unwrap();
    let not_a_stream = audio.play(&clip).unwrap();
    assert_eq!(audio.stream_position(not_a_stream), None);
}
