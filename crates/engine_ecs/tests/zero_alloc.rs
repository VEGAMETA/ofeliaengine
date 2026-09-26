//! CLAUDE.md §5: zero heap allocations per frame in steady state.
//!
//! A frame here spawns and despawns entities (directly and through commands), moves
//! entities between archetypes, runs sequential, per-chunk, parallel and change-filtered
//! queries, runs a parallel system schedule with commands, and saves, restores and hashes the world, as a predicting client does every
//! tick. After a warm-up that creates every archetype and grows every buffer, a counting
//! global allocator must see nothing. One test per file, so nothing else allocates
//! concurrently.

#![allow(clippy::unwrap_used)]

use engine_ecs::{
    Changed, Commands, Entity, QueryState, Schedule, Snapshot, World, bytemuck, pod_component,
    system,
};
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

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Position([f32; 3]);
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Velocity([f32; 3]);
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Lifetime(u32);
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Burning(f32);
pod_component!(Position, Velocity, Lifetime, Burning);

struct Frame {
    movement: QueryState<(&'static mut Position, &'static Velocity)>,
    aging: QueryState<(Entity, &'static mut Lifetime)>,
    moved: QueryState<&'static Position, Changed<Position>>,
    commands: Commands,
    schedule: Schedule,
    snapshot: Snapshot,
    expired: Vec<Entity>,
    burning: Vec<Entity>,
}

fn frame(world: &mut World, f: &mut Frame, pool: &engine_core::jobs::ThreadPool, n: u32) -> u64 {
    // Spawn a wave of short-lived entities through commands.
    for i in 0..200 {
        let e = f
            .commands
            .spawn((Position([0.0; 3]), Velocity([1.0, 0.0, 0.0])));
        f.commands.insert(e, Lifetime(3 + i % 5));
    }
    world.apply(&mut f.commands);

    f.movement.for_each(world, |(p, v)| p.0[0] += v.0[0]);
    f.movement.for_each_chunk(world, |(ps, vs)| {
        for (p, v) in ps.iter_mut().zip(vs) {
            p.0[1] += v.0[1];
        }
    });
    f.movement
        .par_for_each(world, pool, |(p, v)| p.0[2] += v.0[2]);

    f.expired.clear();
    let expired = &mut f.expired;
    f.aging.for_each(world, |(e, life)| {
        life.0 -= 1;
        if life.0 == 0 {
            expired.push(e);
        }
    });
    for &e in &f.expired {
        f.commands.despawn(e);
    }
    world.apply(&mut f.commands);

    // Move a few long-lived entities between archetypes and back.
    f.burning.clear();
    let mut index = 0;
    f.moved.for_each(world, |_| index += 1);
    for i in 0..50u32 {
        let e = f.aging.iter(world).nth((i * 3) as usize).map(|(e, _)| e);
        if let Some(e) = e {
            f.burning.push(e);
        }
    }
    for &e in &f.burning {
        world.insert(e, Burning(1.0));
    }
    for &e in &f.burning {
        world.remove::<Burning>(e);
    }

    // Prediction: save, simulate, roll back, re-simulate.
    f.schedule.run(world, Some(pool));

    world.save(&mut f.snapshot).unwrap();
    f.movement.for_each(world, |(p, _)| p.0[0] *= 0.5);
    world.restore(&f.snapshot).unwrap();
    f.movement.for_each(world, |(p, _)| p.0[0] *= 0.5);
    world.advance_sim_tick();
    world.state_hash() ^ u64::from(n)
}

/// Two systems in one level (run in parallel); the second spawns an entity through its
/// commands every frame (the frame's aging despawns it again).
fn schedule() -> Schedule {
    let mut s = Schedule::new();
    s.add(system(
        "drift",
        |w, a| a.query::<(&mut Position, &Velocity), ()>(w),
        |q, ctx| ctx.for_each(q, |(p, v)| p.0[1] -= v.0[1]),
    ));
    s.add(system(
        "age",
        |w, a| a.query::<(Entity, &mut Lifetime), ()>(w),
        |q, ctx| {
            let mut spawn = None;
            ctx.for_each(q, |(e, life)| {
                if life.0 == u32::MAX && spawn.is_none() {
                    spawn = Some(e);
                }
            });
            if spawn.is_some() {
                let e = ctx
                    .commands()
                    .spawn((Position([0.0; 3]), Velocity([0.0; 3])));
                ctx.commands().insert(e, Lifetime(1));
            }
        },
    ));
    s
}

#[test]
fn steady_state_frames_do_not_allocate() {
    let pool = engine_core::jobs::ThreadPool::builder()
        .workers(2)
        .build()
        .unwrap();
    let mut world = World::new();
    for i in 0..20_000u32 {
        world.spawn((
            Position([i as f32, 0.0, 0.0]),
            Velocity([0.0, 1.0, 0.0]),
            Lifetime(u32::MAX),
        ));
    }
    let mut f = Frame {
        movement: QueryState::new(&mut world),
        aging: QueryState::new(&mut world),
        moved: QueryState::new(&mut world),
        commands: Commands::new(),
        schedule: schedule(),
        snapshot: Snapshot::new(),
        expired: Vec::with_capacity(1024),
        burning: Vec::with_capacity(64),
    };
    // Warm-up: every archetype exists, every buffer reached its high-water mark.
    for n in 0..30 {
        frame(&mut world, &mut f, &pool, n);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let mut hash = 0;
    for n in 0..100 {
        hash ^= frame(&mut world, &mut f, &pool, n);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    std::hint::black_box(hash);
    assert_eq!(
        allocations, 0,
        "{allocations} heap allocations in 100 steady-state frames"
    );
}
