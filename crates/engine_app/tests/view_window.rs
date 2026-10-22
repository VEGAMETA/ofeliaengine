//! The view in a window of its own (ADR 0107 E3-4) beside the player's window: a second
//! swapchain the scene is drawn into and presented with the main one, then shown in the
//! main window again, then closed. Small windows, a few frames.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

#[test]
fn a_second_window_shows_the_view_beside_the_main_one() {
    let Some((mut player, mut game)) = common::player() else {
        return;
    };
    player
        .open_view_window("engine_app view window test", 200, 150, None)
        .unwrap();
    assert!(player.has_view_window());
    let id = player.view_window_id().unwrap();
    assert_ne!(Some(id), player.window().map(|w| w.id));
    let extent = player.view_window_extent().unwrap();
    assert!(extent.width >= 200 && extent.height >= 150, "{extent:?}");
    for _ in 0..10 {
        player.frame(&mut game).unwrap();
    }
    assert!(!player.view_window_close_requested());
    // Back in the main window, the second keeping its last frame; then there again.
    player.show_view_in_window(false);
    for _ in 0..5 {
        player.frame(&mut game).unwrap();
    }
    player.show_view_in_window(true);
    for _ in 0..5 {
        player.frame(&mut game).unwrap();
    }
    player.close_view_window();
    assert!(!player.has_view_window());
    for _ in 0..5 {
        player.frame(&mut game).unwrap();
    }
    let report = player.validation_report();
    assert_eq!(report.errors, 0, "{:?}", report.first_errors);
}
