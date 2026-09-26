//! CLAUDE.md §5 and §11: the audio thread never allocates, and neither does the game side
//! of the mixer in steady state.
//!
//! A frame plays and stops voices (one-shots, loops, pitched, scheduled), changes voice,
//! bus, send and effect parameters, renders through effects, sends and a sidechain with
//! voices going virtual and coming back, and reclaims finished voices. After a warm-up, a
//! counting global allocator must see nothing. One test per file, so nothing else
//! allocates concurrently.

#![allow(clippy::unwrap_used)]

use engine_audio::dsp::{Biquad, Compressor, Delay, Limiter};
use engine_audio::{Audio, AudioConfig, BusId, Clip, PlayParams, SendTap};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only adds counters.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREES.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn steady_state_mixing_does_not_touch_the_heap() {
    let config = AudioConfig {
        max_real_voices: 6,
        ..AudioConfig::default()
    };
    let (mut audio, mut renderer) = Audio::new(&config);
    let music = audio.add_bus("music", BusId::MASTER).unwrap();
    let sfx = audio.add_bus("sfx", BusId::MASTER).unwrap();
    let reverb = audio.add_bus("reverb", BusId::MASTER).unwrap();
    let filter = audio
        .add_effect(sfx, Biquad::low_pass(4_000.0, 0.7))
        .unwrap();
    let duck = audio
        .add_effect(music, Compressor::new(-24.0, 4.0))
        .unwrap();
    audio.set_sidechain(duck, Some(sfx)).unwrap();
    audio
        .add_effect(reverb, Delay::new(0.08, 0.4, 1.0))
        .unwrap();
    audio.add_effect(BusId::MASTER, Limiter::new(-1.0)).unwrap();
    let send = audio
        .add_send(sfx, reverb, 0.3, SendTap::PostFader)
        .unwrap();
    audio.set_bus_voice_limit(sfx, Some(4));

    let tone: Vec<f32> = (0..4_800).map(|i| (i as f32 * 0.03).sin() * 0.3).collect();
    let short = Clip::from_interleaved(48_000, 1, &tone[..700]).unwrap();
    let stereo = Clip::from_interleaved(44_100, 2, &tone).unwrap();
    let bed = audio
        .play_with(
            &stereo,
            &PlayParams {
                bus: music,
                looping: true,
                ..PlayParams::default()
            },
        )
        .unwrap();

    let mut out = vec![0.0f32; 480 * 2];
    let mut frame = |audio: &mut Audio, renderer: &mut engine_audio::Renderer, n: u32| {
        for k in 0..3 {
            let _ = audio.play_with(
                &short,
                &PlayParams {
                    bus: sfx,
                    pitch: 0.8 + 0.1 * k as f32,
                    pan: k as f32 * 0.5 - 0.5,
                    priority: (n % 200) as u8,
                    gain: if n.is_multiple_of(5) { 0.0 } else { 0.5 },
                    start_at: Some(audio.clock().frames + 300 * k),
                    ..PlayParams::default()
                },
            );
        }
        audio.set_voice_gain(bed, 0.5 + 0.5 * (n % 2) as f32, 0.02);
        audio.set_voice_pitch(bed, 1.0 + 0.01 * (n % 3) as f32);
        audio.set_bus_gain(sfx, 0.8, 0.01);
        audio.set_bus_mute(music, n.is_multiple_of(7));
        audio.set_send_gain(send, 0.2 + 0.1 * (n % 2) as f32, 0.01);
        audio.set_effect_param(filter, Biquad::FREQUENCY, 2_000.0 + (n % 10) as f32 * 300.0);
        // Device periods vary between devices; so does the render size here.
        let frames = [480, 128, 256, 441][n as usize % 4];
        renderer.render(&mut out[..frames * 2], 2);
        audio.update();
        let _ = audio.take_peak(BusId::MASTER);
        let _ = audio.voice_counts();
    };

    for n in 0..300 {
        frame(&mut audio, &mut renderer, n);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let frees = FREES.load(Ordering::Relaxed);
    for n in 300..1300 {
        frame(&mut audio, &mut renderer, n);
    }
    let allocated = ALLOCATIONS.load(Ordering::Relaxed) - allocations;
    let freed = FREES.load(Ordering::Relaxed) - frees;
    println!("1000 frames: {allocated} allocations, {freed} frees");
    assert_eq!(allocated, 0);
    assert_eq!(freed, 0);
    assert!(out.iter().any(|&x| x != 0.0));
    assert_eq!(renderer.leaked(), 0);
}
