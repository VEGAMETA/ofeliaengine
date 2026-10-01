//! Steady-state 2D queries and mover moves do not allocate (CLAUDE.md §5, ADR 0066). Box2D
//! itself uses its own allocator only when the world changes (bodies, shapes, contacts).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::Vec2;
use engine_physics2d::{BodyKind, Material, Mover2d, QueryFilter2d, Shape, World2d};
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

#[test]
fn steady_state_2d_queries_and_moves_do_not_allocate() {
    let mut w = World2d::new(Vec2::new(0.0, -10.0));
    let ground = w.create_body(BodyKind::Static, Vec2::new(0.0, -1.0), 0.0);
    w.add_shape(
        ground,
        Shape::Box {
            half_extents: Vec2::new(50.0, 1.0),
        },
        Material::default(),
    )
    .unwrap();
    for i in 0..20 {
        let b = w.create_body(BodyKind::Static, Vec2::new(i as f32 * 2.0 - 20.0, 3.0), 0.3);
        w.add_shape(b, Shape::Circle { radius: 0.5 }, Material::default())
            .unwrap();
    }
    let all = QueryFilter2d::default();
    let mut hits = Vec::new();
    let mut bodies = Vec::new();
    let mut mover = Mover2d::new(Vec2::new(0.0, 0.0), 1.8, 0.3);
    let mut frame = |w: &mut World2d, i: u32| {
        hits.clear();
        bodies.clear();
        std::hint::black_box(w.raycast(Vec2::new(-30.0, 3.0), Vec2::new(60.0, 0.0), all));
        w.raycast_all(Vec2::new(-30.0, 3.0), Vec2::new(60.0, 0.0), all, &mut hits);
        let capsule = Shape::Capsule {
            half_length: 0.5,
            radius: 0.3,
        };
        std::hint::black_box(w.cast_shape(
            capsule,
            Vec2::new(-30.0, 3.0),
            0.2,
            Vec2::new(60.0, 0.0),
            all,
        ));
        w.cast_shape_all(
            capsule,
            Vec2::new(-30.0, 3.0),
            0.0,
            Vec2::new(60.0, 0.0),
            all,
            &mut hits,
        );
        w.overlap_shape(
            Shape::Box {
                half_extents: Vec2::new(10.0, 5.0),
            },
            Vec2::ZERO,
            0.0,
            all,
            &mut bodies,
        );
        let walk = if (i / 60).is_multiple_of(2) {
            3.0
        } else {
            -3.0
        };
        let v = Vec2::new(walk, mover.velocity.y - 10.0 / 60.0);
        w.move_mover(&mut mover, v, 1.0 / 60.0);
        w.step(1.0 / 60.0, 4);
        assert!(hits.len() > 20 && bodies.len() > 5);
    };
    for i in 0..60 {
        frame(&mut w, i);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 60..300 {
        frame(&mut w, i);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!("240 frames of 2D queries and moves: {allocations} allocations");
    assert_eq!(allocations, 0);
}
