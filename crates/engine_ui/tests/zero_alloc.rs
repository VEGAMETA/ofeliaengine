//! CLAUDE.md §5: a steady-state UI frame (build, layout, hit test, draw list) makes no
//! heap allocation. One test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used)]

use engine_ui::*;
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

const KEYS: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];

fn build(ui: &mut Ui) {
    ui.row("root", Style::fill(), |ui| {
        ui.panel(
            "side",
            Style::default()
                .size(Size::Fixed(200.0), Size::Grow(1.0))
                .padding(4.0),
            |ui| {
                ui.scroll_column("list", |ui| {
                    for k in KEYS {
                        ui.row(k, Style::default().gap(4.0), |ui| {
                            ui.label("Entity");
                            ui.button("Select");
                        });
                    }
                });
            },
        );
        ui.column("main", Style::fill().padding(8.0), |ui| {
            ui.paragraph("help", "Select an entity to edit its components.");
            ui.container(
                "popup",
                Style {
                    floating: Some([10.0, 10.0]),
                    ..Style::default()
                },
                |ui| {
                    ui.button("Close");
                },
            );
        });
    });
}

#[test]
fn steady_state_frames_do_not_allocate() {
    let mut ui = Ui::new(Theme::default());
    let mut measure = MonospaceMeasure;
    // Keys are reused inputs; the input's key list is empty and its capacity reused.
    let mut input = UiInput {
        mouse: Some([30.0, 30.0]),
        ..UiInput::default()
    };
    for i in 0..20 {
        input.wheel = if i % 2 == 0 { -5.0 } else { 5.0 };
        ui.begin_frame(&input, [800.0, 600.0]);
        build(&mut ui);
        ui.end_frame(&mut measure);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 0..200 {
        input.wheel = if i % 2 == 0 { -5.0 } else { 5.0 };
        input.mouse_pressed = i % 7 == 0;
        input.mouse_down = i % 7 < 3;
        input.mouse_released = i % 7 == 3;
        ui.begin_frame(&input, [800.0, 600.0]);
        build(&mut ui);
        let f = ui.end_frame(&mut measure);
        std::hint::black_box(f.commands.len());
    }
    let n = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(n, 0, "{n} allocations in 200 frames");
}
