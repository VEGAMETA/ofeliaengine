//! The player runs a game (ADR 0099): its hooks are called in order, the window's events
//! reach it, frames are rendered and presented under validation.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

#[test]
fn a_game_runs_on_the_player() {
    let Some((mut player, mut game)) = common::player() else {
        return;
    };
    println!(
        "{:?}; start-up {:?}; {:?}",
        player,
        player.startup().phases,
        player.presentation()
    );
    let mut rendered = 0;
    for _ in 0..90 {
        if let Some(report) = player.frame(&mut game).unwrap() {
            rendered += 1;
            assert!(report.cpu > 0.0, "{report}");
        }
    }
    let calls = &game.calls;
    println!("{rendered} frames rendered; {calls:?}");
    // A frame or two may go to making the swapchain anew after the first resize.
    assert!(rendered >= 85, "{rendered} of 90 frames rendered");
    // 60 frames of 1/60 s each tick the 64 Hz simulation 64 times a second.
    let expected = (rendered as f64 / 60.0 * 64.0) as usize;
    assert!(calls.ticks.abs_diff(expected) <= 2, "{} ticks", calls.ticks);
    assert_eq!(calls.presents, rendered);
    assert_eq!(calls.animates, rendered);
    assert!(calls.events > 0, "the window's first events reach the game");
    assert!(player.first_present_ms().is_some());
    let extent = player.extent();
    assert!(extent.width >= 100 && extent.height >= 100, "{extent:?}");
    let report = player.validation_report();
    println!(
        "validation: enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
