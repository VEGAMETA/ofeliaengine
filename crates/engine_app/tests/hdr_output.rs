//! HDR output (ADR 0038) of an offscreen player told its display runs in HDR
//! (`Player::assume_display`): scRGB keeps light brighter than SDR white up to the display's
//! peak and draws the UI at Windows' SDR white level; HDR10 encodes the same in PQ; with
//! the display back in SDR the frame is 8-bit sRGB again. `ENGINE_HDR_PNG=<file>` writes
//! the SDR frame.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::prelude::*;
use engine_app::{DisplayInfo, OutputColor, OutputLevels};
use engine_frame::Placement;
use engine_render2d::{Canvas, Quad};

struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
/// The UI's white square: its center and size in pixels.
const SQUARE: (Vec2, f32) = (Vec2::new(20.0, 20.0), 16.0);
/// The display assumed: SDR white at 200 nits, a peak of 1,000.
const WHITE_NITS: f32 = 200.0;
const PEAK_NITS: f32 = 1000.0;

fn hdr_display(enabled: bool) -> DisplayInfo {
    DisplayInfo {
        monitor: 0,
        refresh_hz: None,
        hdr_supported: true,
        hdr_enabled: enabled,
        sdr_white_nits: if enabled { WHITE_NITS } else { 80.0 },
        max_nits: PEAK_NITS,
        max_full_frame_nits: 600.0,
        min_nits: 0.05,
    }
}

/// Inverse of PQ (SMPTE ST 2084): a code value in 0..1 to nits.
fn pq_nits(v: f32) -> f32 {
    let (m1, m2) = (0.159_301_76_f32, 78.843_75_f32);
    let (c1, c2, c3) = (0.835_937_5_f32, 18.851_562_f32, 18.6875_f32);
    let p = v.max(0.0).powf(1.0 / m2);
    10_000.0 * ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1)
}

/// A pixel's linear color, in nits, from a capture in `color`'s encoding (BT.709 for
/// scRGB, BT.2020 for HDR10: luminance is what the checks compare).
fn nits(bytes: &[u8], color: OutputColor, x: u32, y: u32) -> [f32; 3] {
    let i = (y * WIDTH + x) as usize;
    match color {
        OutputColor::ScRgb => {
            let half = |c: usize| {
                let at = i * 8 + c * 2;
                engine_core::half::to_f32(u16::from_le_bytes([bytes[at], bytes[at + 1]])) * 80.0
            };
            [half(0), half(1), half(2)]
        }
        OutputColor::Hdr10 => {
            let at = i * 4;
            let packed =
                u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
            let channel = |shift: u32| pq_nits(((packed >> shift) & 0x3ff) as f32 / 1023.0);
            [channel(0), channel(10), channel(20)]
        }
        OutputColor::Sdr => unreachable!("an HDR capture"),
    }
}

fn luminance([r, g, b]: [f32; 3]) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

#[test]
fn hdr_output_follows_the_display() {
    let settings = PlayerSettings {
        title: String::from("hdr output test"),
        window: Window::Offscreen {
            width: WIDTH,
            height: HEIGHT,
        },
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            return;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    // A white floor under a strong sun: brighter than SDR white where the sun hits it.
    let setup = player.setup();
    let floor = setup.geometry.add_mesh(&cube().as_data()).unwrap();
    let white = setup.materials.add(MaterialDesc {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.6,
        ..MaterialDesc::default()
    });
    let world = player.world_mut();
    world.spawn((
        Transform::from_xyz(0.0, -0.5, 0.0).with_scale(Vec3::new(30.0, 1.0, 30.0)),
        GlobalTransform::default(),
        MeshRenderer::new(floor, white),
    ));
    world.spawn((
        Transform::from_rotation(Quat::from_rotation_x(-1.2)),
        GlobalTransform::default(),
        Sun {
            color: Vec3::splat(12.0),
            ambient: Vec3::splat(0.05),
        },
    ));
    let eye = Vec3::new(0.0, 3.0, 6.0);
    let camera = player.camera(1.0, 0.05, 100.0);
    player.world_mut().spawn((
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        camera,
    ));
    // The UI's white square in the top left corner.
    let mut hud = Canvas::new();
    hud.draw(&Quad::solid(SQUARE.0, Vec2::splat(SQUARE.1)).color(Vec4::ONE));
    hud.prepare();
    player
        .ui_mut()
        .unwrap()
        .canvases
        .push((hud, Placement::Screen));
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    let mut capture = |player: &mut Player| {
        for _ in 0..8 {
            player.frame(&mut game).unwrap();
        }
        player.capture(&mut game).unwrap().unwrap()
    };
    let (sx, sy) = (SQUARE.0.x as u32, SQUARE.0.y as u32);
    let (fx, fy) = (WIDTH / 2, HEIGHT * 3 / 4);

    // SDR: 8-bit sRGB, the lit floor at most white.
    let sdr = capture(&mut player);
    assert_eq!(player.levels(), OutputLevels::SDR);
    assert_eq!(sdr.len(), (WIDTH * HEIGHT * 4) as usize);
    if let Ok(path) = std::env::var("ENGINE_HDR_PNG") {
        let png = engine_render::png::encode_png(
            WIDTH,
            HEIGHT,
            engine_render::png::PngFormat::Rgba,
            &sdr,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    let i = ((sy * WIDTH + sx) * 4) as usize;
    assert_eq!(&sdr[i..i + 3], &[255, 255, 255], "the UI's white in SDR");

    // scRGB on a display in HDR: the UI at SDR white, the floor brighter, below the peak.
    player.assume_display(hdr_display(true));
    let scrgb = capture(&mut player);
    let levels = player.levels();
    assert_eq!(levels.color, OutputColor::ScRgb);
    assert_eq!(levels.white_nits, WHITE_NITS);
    assert_eq!(levels.peak_nits, PEAK_NITS);
    assert_eq!(scrgb.len(), (WIDTH * HEIGHT * 8) as usize, "half floats");
    let ui = luminance(nits(&scrgb, OutputColor::ScRgb, sx, sy));
    assert!(
        (ui - WHITE_NITS).abs() < 2.0,
        "the UI's white at the SDR white level: {ui} nits"
    );
    let lit = luminance(nits(&scrgb, OutputColor::ScRgb, fx, fy));
    println!("scRGB: UI {ui:.0} nits, the lit floor {lit:.0} nits");
    assert!(
        lit > 1.3 * WHITE_NITS && lit <= PEAK_NITS * 1.02,
        "the lit floor between SDR white and the peak: {lit} nits"
    );

    // HDR10: the same light, PQ-encoded in BT.2020.
    player.set_hdr(OutputColor::Hdr10);
    let hdr10 = capture(&mut player);
    assert_eq!(player.levels().color, OutputColor::Hdr10);
    assert_eq!(hdr10.len(), (WIDTH * HEIGHT * 4) as usize);
    let ui10 = luminance(nits(&hdr10, OutputColor::Hdr10, sx, sy));
    let lit10 = luminance(nits(&hdr10, OutputColor::Hdr10, fx, fy));
    println!("HDR10: UI {ui10:.0} nits, the lit floor {lit10:.0} nits");
    assert!(
        (ui10 - WHITE_NITS).abs() < 0.04 * WHITE_NITS,
        "the UI's white at the SDR white level: {ui10} nits"
    );
    assert!(
        (lit10 - lit).abs() < 0.05 * lit,
        "HDR10 shows what scRGB shows: {lit10} and {lit} nits"
    );

    // Back to a display in SDR: 8-bit again, as at first.
    player.assume_display(hdr_display(false));
    let again = capture(&mut player);
    assert_eq!(player.levels(), OutputLevels::SDR);
    assert_eq!(again.len(), sdr.len());
    let differ = sdr
        .iter()
        .zip(&again)
        .filter(|(a, b)| a.abs_diff(**b) > 8)
        .count();
    assert!(
        differ < sdr.len() / 100,
        "{differ} bytes differ from the first SDR frame"
    );
}

/// What HDR output costs the GPU at 1920 × 1080, offscreen (presentation and Windows'
/// composition not included): the median frame and the passes that change, in SDR, scRGB
/// and HDR10. `cargo test --release -p engine_app --test hdr_output -- --ignored --nocapture`
#[test]
#[ignore = "a measurement"]
fn hdr_output_cost() {
    let (width, height) = (1920u32, 1080u32);
    let settings = PlayerSettings {
        title: String::from("hdr output cost"),
        window: Window::Offscreen { width, height },
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let mut player = Player::new(&settings).unwrap();
    let setup = player.setup();
    let floor = setup.geometry.add_mesh(&cube().as_data()).unwrap();
    let white = setup.materials.add(MaterialDesc::default());
    let world = player.world_mut();
    world.spawn((
        Transform::from_xyz(0.0, -0.5, 0.0).with_scale(Vec3::new(30.0, 1.0, 30.0)),
        GlobalTransform::default(),
        MeshRenderer::new(floor, white),
    ));
    world.spawn((
        Transform::from_rotation(Quat::from_rotation_x(-1.2)),
        GlobalTransform::default(),
        Sun {
            color: Vec3::splat(12.0),
            ambient: Vec3::splat(0.05),
        },
    ));
    let eye = Vec3::new(0.0, 3.0, 6.0);
    let camera = player.camera(1.0, 0.05, 100.0);
    player.world_mut().spawn((
        Transform::from_translation(eye).with_rotation(look_rotation(eye, Vec3::ZERO, Vec3::Y)),
        camera,
    ));
    let mut hud = Canvas::new();
    for i in 0..40 {
        let x = 60.0 + 45.0 * (i % 20) as f32;
        let y = 60.0 + 300.0 * (i / 20) as f32;
        hud.draw(
            &Quad::rect(Vec2::new(x, y), Vec2::new(40.0, 200.0))
                .color(Vec4::new(1.0, 1.0, 1.0, 0.5)),
        );
    }
    hud.prepare();
    player
        .ui_mut()
        .unwrap()
        .canvases
        .push((hud, Placement::Screen));
    let mut game = Still;
    player.set_frame_time(Some(1.0 / 60.0));
    for (name, display, hdr) in [
        ("SDR", hdr_display(false), OutputColor::ScRgb),
        ("scRGB", hdr_display(true), OutputColor::ScRgb),
        ("HDR10", hdr_display(true), OutputColor::Hdr10),
    ] {
        player.assume_display(display);
        player.set_hdr(hdr);
        for _ in 0..60 {
            player.frame(&mut game).unwrap();
        }
        let mut frames = Vec::new();
        let mut passes: std::collections::BTreeMap<&'static str, Vec<f64>> = Default::default();
        for _ in 0..300 {
            player.frame(&mut game).unwrap();
            if let Some(us) = player.graph().gpu_frame_time() {
                frames.push(f64::from(us));
            }
            for t in player.graph().gpu_pass_times() {
                if matches!(t.name, "tonemap" | "encode hdr10" | "ui" | "2d")
                    || t.name.starts_with("ui")
                {
                    passes.entry(t.name).or_default().push(f64::from(t.gpu_us));
                }
            }
        }
        let median = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(0.0)
        };
        let frame = median(&mut frames);
        let mut line = format!("{name}: frame {frame:.0} µs");
        for (pass, times) in &mut passes {
            line.push_str(&format!(", {pass} {:.0} µs", median(times)));
        }
        println!("{line}");
    }
}
