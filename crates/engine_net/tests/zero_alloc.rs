//! CLAUDE.md §5: no allocation per tick in steady state, on the server (receive, inputs,
//! quantization, priorities, snapshots, hit shapes, notifications) and on the client
//! (snapshots, prediction, a rollback every second, interpolation, inputs), over the
//! in-memory network with loss and jitter. One test per file (a global allocator).

#![allow(clippy::unwrap_used, clippy::print_stdout, missing_docs)]

mod common;

use common::*;
use engine_net::ServerSettings;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn counted(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    ALLOCATIONS.load(Ordering::Relaxed) - before
}

#[test]
fn no_allocation_per_tick_in_steady_state() {
    let mut sim = Sim::new(21, conditions(30, 8, 0.05), ServerSettings::default());
    sim.spawn_movers(500, 5);
    sim.add_client();
    sim.add_client();
    // The server pushes the pawns once a second: rollbacks are part of steady state.
    sim.rules = Some(Box::new(|s: &mut ServerSide| {
        if s.server.tick().is_multiple_of(64) {
            for &(_, pawn) in &s.pawns {
                if let Some(p) = s.world.get_mut::<Pawn>(pawn) {
                    p.pos.x += 0.5;
                }
            }
        }
    }));
    let mut frame = 0u32;
    let mut input = move |sim: &mut Sim| {
        frame += 1;
        for (i, c) in sim.clients.iter_mut().enumerate() {
            let t = frame / 30 + i as u32;
            c.input.move_x = [1.0, 0.0, -1.0, 0.0][(t % 4) as usize];
            c.input.move_z = [0.0, 1.0, 0.0, -1.0][(t % 4) as usize];
            c.input.fire = u8::from(frame.is_multiple_of(7));
        }
    };
    // Warm up: every buffer reaches its steady size.
    sim.run_with(6.0, &mut input);
    let (mut server_allocs, mut client_allocs) = (0, 0);
    let (mut ticks, mut frames) = (0, 0);
    let end = sim.now_us + 4_000_000;
    while sim.now_us < end {
        if sim.next_tick_us <= sim.next_frame_us {
            sim.now_us = sim.next_tick_us;
            server_allocs += counted(|| sim.server_tick());
            sim.next_tick_us += sim.tick_us;
            ticks += 1;
        } else {
            sim.now_us = sim.next_frame_us;
            input(&mut sim);
            for i in 0..sim.clients.len() {
                client_allocs += counted(|| sim.client_frame(i));
            }
            sim.next_frame_us += sim.frame_us;
            frames += 1;
        }
    }
    let stats = sim.clients[0].client.stats();
    println!(
        "{ticks} server ticks: {server_allocs} allocations; {frames} client frames: \
         {client_allocs}; {stats:?}"
    );
    assert!(stats.resimulated_ticks > 0, "rollbacks happened");
    assert_eq!(server_allocs, 0);
    assert_eq!(client_allocs, 0);
}
