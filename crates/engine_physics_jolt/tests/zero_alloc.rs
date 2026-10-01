//! Steady-state queries, character moves and state save/restore do not allocate (CLAUDE.md
//! §5, ADR 0066): neither through Rust's global allocator (counted here) nor through
//! Jolt's allocation hooks (counted by the backend). One test function, so no other test
//! allocates concurrently in this binary.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Quat, Vec3};
use engine_physics::scenes;
use engine_physics::{
    CharacterDesc, CharacterMotor, PhysicsBackend, PhysicsSettings, PhysicsSnapshot, QueryFilter,
    Ray, ShapeCast, ShapeDesc,
};
use engine_physics_jolt::JoltBackend;
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

/// Rust and Jolt allocations made by `f`.
fn allocations(mut f: impl FnMut()) -> (usize, u64) {
    let rust = ALLOCATIONS.load(Ordering::Relaxed);
    let jolt = JoltBackend::jolt_allocations();
    f();
    (
        ALLOCATIONS.load(Ordering::Relaxed) - rust,
        JoltBackend::jolt_allocations() - jolt,
    )
}

const DT: f32 = 1.0 / 60.0;

#[test]
fn steady_state_physics_does_not_allocate() {
    let mut w = JoltBackend::new(&PhysicsSettings {
        threads: 0,
        ..PhysicsSettings::default()
    })
    .unwrap();
    scenes::pile(&mut w, 300).unwrap();
    let ball = w.create_shape(&ShapeDesc::Sphere { radius: 0.4 }).unwrap();
    let capsule = w
        .create_shape(&ShapeDesc::Capsule {
            half_height: 0.5,
            radius: 0.3,
        })
        .unwrap();
    let c = w
        .create_character(&CharacterDesc::capsule(Vec3::new(20.0, 0.0, 0.0), 1.8, 0.3))
        .unwrap();
    let motor = CharacterMotor::default();
    for _ in 0..120 {
        w.step(DT, 1).unwrap();
    }

    let ray = Ray {
        origin: Vec3::new(0.0, 30.0, 0.0),
        direction: Vec3::new(0.3, -60.0, 0.2),
    };
    let cast = ShapeCast {
        shape: capsule,
        position: Vec3::new(-1.0, 30.0, 0.5),
        rotation: Quat::from_rotation_x(0.3),
        direction: Vec3::new(0.5, -60.0, 0.0),
    };
    let mut ray_hits = Vec::new();
    let mut shape_hits = Vec::new();
    let mut bodies = Vec::new();
    let mut contacts = Vec::new();
    let mut queries = |w: &JoltBackend| {
        ray_hits.clear();
        shape_hits.clear();
        bodies.clear();
        contacts.clear();
        let filter = QueryFilter::default();
        std::hint::black_box(w.raycast(&ray, filter));
        w.raycast_all(&ray, filter, &mut ray_hits);
        std::hint::black_box(w.cast_shape(&cast, filter));
        w.cast_shape_all(&cast, filter, &mut shape_hits);
        w.overlap_shape(
            ball,
            Vec3::new(0.0, 0.5, 0.0),
            Quat::IDENTITY,
            filter,
            &mut bodies,
        );
        w.overlap_sphere(Vec3::new(1.0, 0.5, 0.0), 2.0, filter, &mut bodies);
        w.character_contacts(c, &mut contacts);
        assert!(!ray_hits.is_empty() && !shape_hits.is_empty() && !bodies.is_empty());
    };
    let walk = |w: &mut JoltBackend, i: u32| {
        let state = w.character(c).unwrap();
        let input = Vec3::new((i as f32 * 0.05).sin(), 0.0, (i as f32 * 0.05).cos()) * 3.0;
        let v = motor.velocity(&state, input, i.is_multiple_of(40), DT);
        w.move_character(c, v, DT).unwrap();
    };

    // Warm-up: buffers reach their sizes.
    queries(&w);
    for i in 0..30 {
        walk(&mut w, i);
        w.step(DT, 1).unwrap();
    }
    let (rust, jolt) = allocations(|| {
        for _ in 0..100 {
            queries(&w);
        }
    });
    println!("100 × 7 queries: {rust} Rust and {jolt} Jolt allocations");
    assert_eq!((rust, jolt), (0, 0), "queries allocate");

    let (rust, jolt) = allocations(|| {
        for i in 30..130 {
            walk(&mut w, i);
        }
    });
    println!("100 character moves: {rust} Rust and {jolt} Jolt allocations");
    assert_eq!((rust, jolt), (0, 0), "character moves allocate");

    // Save and restore: nothing on the Rust side once the snapshot has grown. Jolt's
    // state recorder makes temporary arrays (bodies to save, sorted contact pairs and per
    // pair manifolds, bodies to wake): counted and reported, not zero (ADR 0066).
    let mut snapshot = PhysicsSnapshot::new();
    w.save_state(&mut snapshot).unwrap();
    let (rust, jolt_save) = allocations(|| {
        for _ in 0..10 {
            w.save_state(&mut snapshot).unwrap();
        }
    });
    assert_eq!(rust, 0, "saving allocates on the Rust side");
    // Warm-up (with Tracy compiled in, a zone's first use registers it).
    w.restore_state(&snapshot).unwrap();
    let (rust, jolt_restore) = allocations(|| {
        for _ in 0..10 {
            w.restore_state(&snapshot).unwrap();
        }
    });
    assert_eq!(rust, 0, "restoring allocates on the Rust side");
    println!(
        "save: {} Jolt allocations per call, restore: {} per call ({} bodies, {} bytes)",
        jolt_save / 10,
        jolt_restore / 10,
        w.stats().bodies,
        snapshot.len()
    );
}
