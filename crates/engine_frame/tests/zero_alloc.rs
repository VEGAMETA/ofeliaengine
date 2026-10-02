//! CLAUDE.md §5: zero heap allocations per frame in steady state, for a whole frame
//! through the frame renderer (ADR 0072): the pools' frames, `prepare`, `add_view` (scene
//! passes, TAAU or FSR, bloom, automatic exposure, tone mapping, the previous color) and
//! the graph's execution. FSR's own allocations happen in C++ and are not counted. One
//! test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use common::{Bed, camera};
use engine_core::math::Vec3;
use engine_frame::{
    Antialiasing, Exposure, Msaa, PostAntialiasing, Quality, Upscale, View, ViewSettings,
};
use engine_render::postfx::ExposureSettings;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
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
fn steady_state_frames_do_not_allocate() {
    let Some(mut bed) = Bed::new((160, 90), false) else {
        return;
    };
    let grey = bed.solid([0.5, 0.5, 0.5], [0.0; 3]);
    let glow = bed.solid([0.2, 0.2, 0.2], [3.0, 2.0, 1.0]);
    let draws = vec![
        bed.cube_at(grey, Vec3::new(10.0, 0.2, 10.0), Vec3::new(0.0, -0.1, 0.0)),
        bed.cube_at(glow, Vec3::splat(1.0), Vec3::new(0.0, 0.5, 0.0)),
    ];
    // Every kind of path (ADR 0075): spatial, temporal, MSAA, filters after tone mapping.
    for (antialiasing, quality) in [
        (Antialiasing::OFF, Quality::Quality),
        (Antialiasing::TAAU, Quality::Quality),
        (Antialiasing::FSR, Quality::Quality),
        (Antialiasing::spatial(Upscale::Fsr1), Quality::Quality),
        (
            Antialiasing::spatial(Upscale::Integer),
            Quality::Performance,
        ),
        (Antialiasing::FXAA, Quality::Native),
        (
            Antialiasing::msaa(Msaa::X4).with_post(PostAntialiasing::SMAA),
            Quality::Native,
        ),
        (Antialiasing::SMAA_4X, Quality::Native),
        (Antialiasing::TAAU.with_samples(Msaa::X2), Quality::Quality),
    ] {
        let settings = ViewSettings {
            antialiasing,
            quality,
            exposure: Exposure::Auto(ExposureSettings::default()),
            previous_color: true,
            occlusion_culling: true,
            ..ViewSettings::default()
        };
        let mut view = View::new(&bed.device, 16);
        let frame = |bed: &mut Bed, view: &mut View, k: u32| {
            // A slowly moving camera: motion vectors and the history in use.
            let eye = Vec3::new(0.01 * k as f32, 2.0, 5.0);
            bed.render(
                view,
                &settings,
                &draws,
                &camera(eye, Vec3::ZERO, false),
                false,
                |_| {},
            );
        };
        for k in 0..20 {
            frame(&mut bed, &mut view, k);
        }
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        for k in 20..120 {
            frame(&mut bed, &mut view, k);
        }
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        println!(
            "{}: 100 frames, {allocations} heap allocations",
            antialiasing
        );
        assert_eq!(allocations, 0, "{}", antialiasing);
        // SAFETY: the bed's device is idle after `render`.
        unsafe { view.destroy(&mut bed.allocator, &bed.bindless) };
    }
    bed.finish(&mut []);
}
