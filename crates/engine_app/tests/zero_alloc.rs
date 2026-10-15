//! CLAUDE.md §5: zero heap allocations per frame in steady state, for the player's whole
//! frame (ADR 0099): the window's events, latency control, the slot and swapchain image,
//! the runtime's ticks and extraction, the game's hooks, the frame renderer and graph,
//! submission and presentation; and the window thread's work beside it (its display check
//! once a second). One test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;
#[path = "common/counting.rs"]
mod counting;

#[global_allocator]
static GLOBAL: counting::CountingAllocator = counting::CountingAllocator;

#[test]
fn the_players_steady_state_frames_do_not_allocate() {
    let Some((mut player, mut game)) = common::player() else {
        return;
    };
    counting::assert_steady_state(&mut player, &mut game);
}
