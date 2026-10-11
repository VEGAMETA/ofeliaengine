//! Frame generation through the frame renderer (ADR 0091), with the UI layer drawn on the
//! real frame and on the generated one, each with its own camera: a screen HUD, a panel in
//! the world partly behind a wall, and a panel attached to the camera.

use crate::common::{Bed, camera};
use engine_core::math::{Mat4, Quat, Vec2, Vec3, Vec4};
use engine_frame::{
    Antialiasing, FrameGeneration, Placement, UiCanvas, UiRenderer, View, ViewSettings,
};
use engine_render2d::{Canvas, Quad};

const SIZE: (u32, u32) = (256, 192);

fn pixel(image: &[u8], x: u32, y: u32) -> [u8; 3] {
    let i = ((y * SIZE.0 + x) * 4) as usize;
    [image[i], image[i + 1], image[i + 2]]
}

fn is_green(p: [u8; 3]) -> bool {
    p[1] > 150 && p[0] < 100 && p[2] < 100
}

fn is_red(p: [u8; 3]) -> bool {
    p[0] > 150 && p[1] < 100 && p[2] < 100
}

/// The mean x of the panel's green pixels on row `y`, and whether a gap (the wall) splits
/// them.
fn panel_row(image: &[u8], y: u32) -> (f32, bool) {
    let xs: Vec<u32> = (0..SIZE.0)
        .filter(|&x| is_green(pixel(image, x, y)))
        .collect();
    assert!(!xs.is_empty(), "the panel is on row {y}");
    let mean = xs.iter().sum::<u32>() as f32 / xs.len() as f32;
    let split = xs.windows(2).any(|w| w[1] > w[0] + 2);
    (mean, split)
}

#[test]
fn generated_frames_get_the_ui_drawn_with_their_own_camera() {
    let Some(mut bed) = Bed::new(SIZE, true) else {
        return;
    };
    let grey = bed.solid([0.45, 0.45, 0.45], [0.0; 3]);
    let dark = bed.solid([0.05, 0.05, 0.08], [0.0; 3]);
    let blue = bed.solid([0.1, 0.2, 0.8], [0.0; 3]);
    // A floor, a few blocks, and a narrow wall standing in front of the world panel.
    let draws = vec![
        bed.cube_at(grey, Vec3::new(20.0, 0.2, 20.0), Vec3::new(0.0, -0.1, 0.0)),
        bed.cube_at(blue, Vec3::splat(0.6), Vec3::new(-2.0, 0.3, -0.5)),
        bed.cube_at(blue, Vec3::splat(0.6), Vec3::new(2.2, 0.3, -1.0)),
        bed.cube_at(dark, Vec3::new(0.3, 3.0, 0.2), Vec3::new(0.0, 1.5, -1.0)),
    ];

    // The UI: a HUD block in the top-left corner, a green panel in the world behind the
    // wall (2 × 1 m, canvas units of a centimeter, y down), a red square on the camera.
    let mut hud = Canvas::new();
    hud.draw(&Quad::solid(Vec2::new(40.0, 24.0), Vec2::new(64.0, 32.0)).color(Vec4::ONE));
    hud.prepare();
    let mut panel = Canvas::new();
    panel.draw(
        &Quad::solid(Vec2::new(100.0, 50.0), Vec2::new(200.0, 100.0))
            .color(Vec4::new(0.0, 1.0, 0.0, 1.0)),
    );
    panel.prepare();
    let mut visor = Canvas::new();
    visor.draw(
        &Quad::solid(Vec2::new(50.0, 50.0), Vec2::new(100.0, 100.0))
            .color(Vec4::new(1.0, 0.0, 0.0, 1.0)),
    );
    visor.prepare();
    let world_from_panel = Mat4::from_scale_rotation_translation(
        Vec3::new(0.01, -0.01, 1.0),
        Quat::IDENTITY,
        Vec3::new(-1.0, 2.0, -1.6),
    );
    let view_from_visor = Mat4::from_scale_rotation_translation(
        Vec3::new(0.0006, -0.0006, 1.0),
        Quat::IDENTITY,
        Vec3::new(0.15, -0.08, -0.5),
    );
    let layer = [
        UiCanvas {
            canvas: &hud,
            placement: Placement::Screen,
        },
        UiCanvas {
            canvas: &panel,
            placement: Placement::World(world_from_panel),
        },
        UiCanvas {
            canvas: &visor,
            placement: Placement::Camera(view_from_visor),
        },
    ];
    let mut ui = UiRenderer::new(
        &bed.device,
        &mut bed.allocator,
        &bed.bindless,
        crate::common::OUTPUT,
    )
    .unwrap();
    let targets = bed.generation_targets();
    let settings = ViewSettings {
        antialiasing: Antialiasing::TAAU,
        bloom: false,
        frame_generation: FrameGeneration::Fsr,
        ..ViewSettings::default()
    };
    let mut view = View::new(&bed.device, 16);

    // The camera pans right by 0.12 m a frame.
    let mut frames = Vec::new();
    for k in 0..10 {
        let x = -0.6 + 0.12 * k as f32;
        let cam = camera(Vec3::new(x, 1.6, 4.0), Vec3::new(x, 1.2, 0.0), false);
        let (generated, real, out) =
            bed.render_generating(&mut view, &settings, &draws, &cam, &targets, &ui, &layer);
        let generated = generated.expect("frame generation is on");
        assert_eq!(generated.shown, k > 0, "frame {k}");
        frames.push((real, out));
    }

    // The HUD and the camera's panel are the same, sharp, on every frame shown.
    for (k, (real, out)) in frames.iter().enumerate().skip(1) {
        for image in [real, out] {
            assert_eq!(pixel(image, 20, 20), [255, 255, 255], "HUD, frame {k}");
            assert_eq!(pixel(image, 70, 38), [255, 255, 255], "HUD, frame {k}");
        }
        // The visor's centre: the same pixel in the real and the generated frame.
        let visor_at = (0..SIZE.0)
            .flat_map(|x| (0..SIZE.1).map(move |y| (x, y)))
            .filter(|&(x, y)| is_red(pixel(real, x, y)))
            .collect::<Vec<_>>();
        assert!(!visor_at.is_empty(), "the visor is drawn, frame {k}");
        for &(x, y) in &visor_at {
            assert!(is_red(pixel(out, x, y)), "the visor holds still, frame {k}");
        }
    }

    // The world panel: behind the wall (split), and on a generated frame half-way between
    // the real frames around it.
    let row = SIZE.1 / 3;
    for k in 2..frames.len() {
        let (before, _) = panel_row(&frames[k - 1].0, row);
        let (after, split_real) = panel_row(&frames[k].0, row);
        let (between, split_generated) = panel_row(&frames[k].1, row);
        println!("frame {k}: panel at x {before:.1} → {between:.1} → {after:.1}");
        assert!(
            split_real && split_generated,
            "the wall hides the panel's middle"
        );
        let middle = 0.5 * (before + after);
        assert!(
            (between - middle).abs() < 1.5,
            "the panel on the generated frame is half-way: {before} {between} {after}"
        );
    }

    bed.free_generation_targets(targets);
    // SAFETY: the device is idle (the bed waits after each frame).
    unsafe { ui.destroy(&mut bed.allocator) };
    // Its pipelines go with it, before the device.
    drop(ui);
    bed.finish(&mut [&mut view]);
}
