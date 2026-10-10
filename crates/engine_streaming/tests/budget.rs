//! Streaming under a GPU memory budget (ADR 0076), under the validation layer: Sponza
//! streamed from its pack in a streaming pool too small for its textures renders with
//! coarser levels, within the pool, without validation errors, and gets every level it
//! samples back once the pool grows; no frame records more texture traffic than the
//! pool's frame budget (or its largest single level). A fast turn finds the mesh levels
//! around the camera loaded and the textures it brings into view catch up.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use common::{Bed, FRAME_BUDGET, gpu, sponza_pack};
use engine_core::math::Vec3;

const MIB: u64 = 1 << 20;

#[test]
fn sponza_in_a_tight_budget_streams_coarser_levels_and_recovers() {
    let Some(pack) = sponza_pack() else {
        assert!(
            !std::env::var("ENGINE_REQUIRE_GOLDEN").is_ok_and(|v| v == "1"),
            "Sponza missing (tools/fetch-test-assets.ps1, or ENGINE_TEST_ASSETS)"
        );
        println!("skipped: Sponza missing");
        return;
    };
    let Some((instance, device)) = gpu(true, false) else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    // 12 MiB for Sponza's 91 MiB of textures (2 MiB of tails).
    let mut bed = Bed::new(instance, device, &pack, 12 * MIB);
    // Down the nave from a quarter of its length, as the golden images look.
    let (min, max) = (bed.min, bed.max);
    let size = max - min;
    let center = (min + max) * 0.5;
    let eye = Vec3::new(min.x + size.x * 0.25, min.y + size.y * 0.15, center.z);
    let target = Vec3::new(max.x, min.y + size.y * 0.25, center.z);
    let camera = bed.camera(eye, target);
    let frames = bed.settle(camera, 1500);
    let tight = bed.streamer.as_ref().unwrap().stats();
    let (live, retiring) = bed.textures.memory();
    let client = bed.streamer.as_mut().unwrap().texture_streamer().client();
    let grant = bed.budget.grant(client);
    println!(
        "tight: settled in {frames} frames; textures {:.1} MiB (grant {:.1} MiB), bias {}, \
         planned {:.1} MiB, meshes {:.2} MiB; {} levels arrived",
        live as f64 / MIB as f64,
        grant as f64 / MIB as f64,
        tight.textures.pressure_bias,
        tight.textures.planned as f64 / MIB as f64,
        tight.meshes.resident as f64 / MIB as f64,
        tight.arrived
    );
    assert!(frames < 1500, "streaming did not settle");
    assert_eq!(retiring, 0);
    assert!(tight.textures.pressure_bias >= 1, "the budget is tight");
    assert!(live <= grant, "{live} resident, {grant} granted");

    // The pool grows: every level the view samples comes back.
    bed.set_limit(4096 * MIB);
    let frames = bed.settle(camera, 1500);
    let ample = bed.streamer.as_ref().unwrap().stats();
    let (full, _) = bed.textures.memory();
    println!(
        "ample: settled in {frames} frames; textures {:.1} MiB, bias {}, meshes {:.2} MiB",
        full as f64 / MIB as f64,
        ample.textures.pressure_bias,
        ample.meshes.resident as f64 / MIB as f64,
    );
    assert!(frames < 1500, "streaming did not settle");
    assert_eq!(ample.textures.pressure_bias, 0);
    assert!(full > live + 8 * MIB, "{full} after {live}");
    // Every frame within the frame budget, or one level larger than it.
    let largest = 4096 * 4096 + 2048 * 2048;
    println!(
        "worst frame: {:.1} MiB of texture traffic",
        bed.worst_frame_bytes as f64 / MIB as f64
    );
    assert!(bed.worst_frame_bytes <= FRAME_BUDGET.max(largest));
    let errors = bed.finish();
    assert_eq!(errors, 0);
    let _ = std::fs::remove_file(pack);
}

/// A fast turn (queue item 0b): with mesh levels asked for by distance in every direction
/// (`LodView::around`), a 180° turn after the scene settled loads no mesh level, where the
/// frustum's view loads some; the textures the turn brings into view reach their wanted
/// levels within a bounded number of frames.
#[test]
fn a_fast_turn_finds_the_meshes_loaded_and_the_textures_catch_up() {
    let Some(pack) = sponza_pack() else {
        assert!(
            !std::env::var("ENGINE_REQUIRE_GOLDEN").is_ok_and(|v| v == "1"),
            "Sponza missing (tools/fetch-test-assets.ps1, or ENGINE_TEST_ASSETS)"
        );
        println!("skipped: Sponza missing");
        return;
    };
    let mut loaded_by_turn = [0u64; 2];
    for (k, around) in [false, true].into_iter().enumerate() {
        let Some((instance, device)) = gpu(false, false) else {
            return;
        };
        let mut bed = Bed::new(instance, device, &pack, 4096 * MIB);
        bed.around = around;
        // At the nave's centre, looking down it; then 180° in 6 frames.
        let (min, max) = (bed.min, bed.max);
        let size = max - min;
        let center = (min + max) * 0.5;
        let eye = Vec3::new(center.x, min.y + size.y * 0.2, center.z);
        let look = |yaw: f32| {
            let dir = Vec3::new(yaw.cos(), -0.15, yaw.sin());
            bed.camera(eye, eye + dir)
        };
        let ahead = look(0.0);
        let turn: Vec<_> = (1..=6)
            .map(|i| look(core::f32::consts::PI * i as f32 / 6.0))
            .collect();
        let back = look(core::f32::consts::PI);
        let frames = bed.settle(ahead, 1500);
        assert!(frames < 1500, "streaming did not settle");
        let before = bed.streamer.as_ref().unwrap().stats().mesh_levels;
        for camera in turn {
            bed.frame(camera);
        }
        // Frames until under 1 % of the pixels the feedback saw over its cycle are drawn
        // coarser than wanted.
        let window = u64::from(bed.feedback.settings().slices * bed.feedback.settings().interval);
        let mut catch_up = None;
        for f in 1..=600 {
            bed.frame(back);
            let s = bed
                .streamer
                .as_mut()
                .unwrap()
                .texture_streamer()
                .sharpness(&bed.textures, window);
            if s.pixels > 0 && s.short * 100 < s.pixels {
                catch_up = Some(f);
                break;
            }
        }
        let after = bed.streamer.as_ref().unwrap().stats().mesh_levels;
        loaded_by_turn[k] = after - before;
        println!(
            "{}: settled in {frames} frames; the turn loaded {} mesh levels; textures sharp after {catch_up:?} frames",
            if around { "around" } else { "frustum" },
            after - before
        );
        assert!(catch_up.is_some(), "the textures never caught up");
        assert_eq!(bed.finish(), 0);
    }
    assert!(
        loaded_by_turn[0] > 0,
        "the frustum's view loads levels on a turn"
    );
    assert_eq!(
        loaded_by_turn[1], 0,
        "around the camera, the turn found them loaded"
    );
}
