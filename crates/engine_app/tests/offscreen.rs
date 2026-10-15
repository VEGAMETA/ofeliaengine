//! The offscreen player (ADR 0099 step 4): no window, swapchain or latency control; each
//! frame drawn into an image that `Player::capture` reads back, under synchronization
//! validation.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use engine_app::Window;

#[test]
fn an_offscreen_player_captures_its_frames() {
    let (width, height) = (160u32, 120u32);
    let Some((mut player, mut game)) = common::player_with(|settings| {
        settings.window = Window::Offscreen { width, height };
        settings.validation = Some(true);
    }) else {
        return;
    };
    assert!(player.window().is_none());
    assert!(player.swapchain().is_none() && player.latency().is_none());
    let extent = player.extent();
    assert_eq!((extent.width, extent.height), (width, height));
    // Frames without a capture render too (a pass keeps the image).
    for _ in 0..20 {
        assert!(player.frame(&mut game).unwrap().is_some());
    }
    let rgba = player.capture(&mut game).unwrap().unwrap();
    assert_eq!(rgba.len(), (width * height * 4) as usize);
    let pixel = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    };
    // The grey floor fills the bottom; the ring of red cubes stands in the middle; the
    // sky's light above.
    let (bottom, middle, top) = (
        pixel(width / 2, height - 4),
        pixel(width / 2, height / 2),
        pixel(width / 2, 2),
    );
    println!("bottom {bottom:?}, middle {middle:?}, top {top:?}");
    assert!(
        bottom.iter().all(|&c| c > 0),
        "the floor is lit: {bottom:?}"
    );
    let distinct: std::collections::BTreeSet<[u8; 4]> = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .map(|(x, y)| pixel(x, y))
        .collect();
    assert!(
        distinct.len() > 100,
        "{} colors: a scene, not a clear",
        distinct.len()
    );
    // Every pixel is opaque.
    assert!((0..width * height).all(|i| rgba[(i * 4 + 3) as usize] == 255));
    // Another capture after more frames: the cubes turned.
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    let again = player.capture(&mut game).unwrap().unwrap();
    assert_ne!(rgba, again, "the spinning cubes moved");

    let report = player.validation_report();
    println!(
        "validation: enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
