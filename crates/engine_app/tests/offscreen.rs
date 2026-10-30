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

/// A scene with nothing in it still draws its sky (status §6, found by ADR 0124's E12-1a
/// test): with every model gone the frame has no instance, and the view's environment is
/// drawn over its clear, not the clear colour alone.
#[test]
fn an_empty_scene_still_draws_its_sky() {
    let (width, height) = (160u32, 120u32);
    let Some((mut player, mut game)) = common::player_with(|settings| {
        settings.window = Window::Offscreen { width, height };
        settings.validation = Some(true);
    }) else {
        return;
    };
    let world = player.world_mut();
    let mut drawn =
        engine_ecs::QueryState::<(engine_ecs::Entity, &engine_runtime::MeshRenderer)>::new(world);
    let models: Vec<engine_ecs::Entity> = drawn.iter_readonly(world).map(|(e, _)| e).collect();
    assert!(!models.is_empty());
    for e in models {
        world.despawn(e);
    }
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    let rgba = player.capture(&mut game).unwrap().unwrap();
    let distinct: std::collections::BTreeSet<[u8; 4]> = rgba
        .chunks_exact(4)
        .map(|p| [p[0], p[1], p[2], p[3]])
        .collect();
    let (top, bottom) = (&rgba[..4], &rgba[rgba.len() - 4..]);
    println!("{} colors; top {top:?}, bottom {bottom:?}", distinct.len());
    assert!(
        distinct.len() > 10,
        "{} colors: the sky, not the clear colour",
        distinct.len()
    );
    let report = player.validation_report();
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
