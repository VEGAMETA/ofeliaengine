//! CLAUDE.md §5: zero heap allocations per frame in steady state, for handling input
//! events, sampling the tick and frame views (with contexts and chords), and encoding and
//! decoding the tick as a network user command.

#![allow(clippy::unwrap_used)]

use engine_input::{
    ActionState, Binding, EventKind, GamepadAxis, GamepadButton, Input, InputEvent, InputMap, Key,
    MouseButton, Stick,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // Input handling is single-threaded: count the test thread only. Other threads (the
    // Tracy client's, with `engine_profiler/tracy`) allocate on their own schedule.
    // Const-initialized and without drop glue, so reading it never allocates.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn count() {
    if COUNTING.with(Cell::get) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn steady_state_frames_do_not_allocate() {
    let mut map = InputMap::new();
    let jump = map.add_button("jump").unwrap();
    map.bind(jump, Binding::button(Key::Space)).unwrap();
    map.bind(jump, Binding::button(GamepadButton::South))
        .unwrap();
    let walk = map.add_axis2d("move").unwrap();
    map.bind(walk, Binding::wasd()).unwrap();
    map.bind(walk, Binding::stick(Stick::Left)).unwrap();
    let look = map.add_axis2d("look").unwrap();
    map.bind(look, Binding::mouse_motion(0.2)).unwrap();
    let fire = map.add_button("fire").unwrap();
    map.bind(fire, Binding::button(MouseButton::Left)).unwrap();
    let save = map.add_button("save").unwrap();
    map.bind(save, Binding::chord(&[Key::ControlLeft], Key::KeyS))
        .unwrap();
    let menu = map.add_context("menu", 5).unwrap();
    let back = map.add_button("back").unwrap();
    map.bind(back, Binding::button(Key::Escape)).unwrap();
    let mut input = Input::new(map.clone());
    let mut server = ActionState::default();
    let mut bytes = Vec::with_capacity(256);
    let start = Instant::now();

    let mut frame = |n: u64, input: &mut Input| {
        let t = start + Duration::from_millis(n * 16);
        let keys = [
            Key::KeyW,
            Key::KeyD,
            Key::Space,
            Key::ControlLeft,
            Key::KeyS,
            Key::Escape,
        ];
        let k = keys[(n % keys.len() as u64) as usize];
        input.handle(&InputEvent {
            time: t,
            kind: EventKind::Key {
                key: k,
                pressed: n.is_multiple_of(2),
            },
        });
        input.handle(&InputEvent {
            time: t,
            kind: EventKind::MouseMotion { dx: 1.5, dy: -0.5 },
        });
        input.handle(&InputEvent {
            time: t,
            kind: EventKind::GamepadAxis {
                pad: 0,
                axis: GamepadAxis::LeftX,
                value: (n % 10) as f32 / 10.0,
            },
        });
        input.set_context_enabled(menu, n.is_multiple_of(7));
        input.sample_frame(t + Duration::from_millis(7));
        let tick = input.sample_tick(t + Duration::from_millis(15));
        bytes.clear();
        tick.encode(&map, &mut bytes);
        server.decode(&map, &bytes).unwrap();
        std::hint::black_box((server.vector(walk), input.tick().pressed("jump")));
    };
    for n in 0..50 {
        frame(n, &mut input);
    }
    COUNTING.with(|c| c.set(true));
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for n in 50..500 {
        frame(n, &mut input);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    COUNTING.with(|c| c.set(false));
    assert_eq!(
        allocations, 0,
        "{allocations} heap allocations in 450 steady-state frames"
    );
}
