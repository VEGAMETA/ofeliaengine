//! The Jolt backend through the `PhysicsBackend` interface: physical behavior, queries,
//! events, gravity modes and determinism, on the shared scenes.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Quat, Vec3};
use engine_physics::scenes;
use engine_physics::{
    BodyDesc, BodyId, ContactKind, GravityMode, Layer, PhysicsBackend, PhysicsSettings,
    QueryFilter, Ray, ShapeDesc,
};
use engine_physics_jolt::JoltBackend;

const DT: f32 = 1.0 / 60.0;

/// A world on the calling thread plus `threads` workers, used for every step.
fn world(threads: u32) -> JoltBackend {
    JoltBackend::new(&PhysicsSettings {
        threads,
        single_thread_below: 0,
        ..PhysicsSettings::default()
    })
    .unwrap()
}

fn run(world: &mut dyn PhysicsBackend, seconds: f32) {
    for _ in 0..(seconds / DT).round() as u32 {
        world.step(DT, 1).unwrap();
    }
}

#[test]
fn free_fall_matches_kinematics() {
    let mut w = world(0);
    let sphere = w.create_shape(&ShapeDesc::Sphere { radius: 0.5 }).unwrap();
    let body = w
        .create_body(&BodyDesc {
            linear_damping: 0.0,
            ..BodyDesc::dynamic(sphere, Vec3::new(0.0, 100.0, 0.0))
        })
        .unwrap();
    run(&mut w, 1.0);
    let (p, _) = w.transform(body).unwrap();
    let (v, _) = w.velocity(body).unwrap();
    // Semi-implicit Euler over 60 steps: close to 0.5 g t² = 4.905 m.
    let fallen = 100.0 - p.y;
    println!("fell {fallen} m, v = {v}");
    assert!((fallen - 4.905).abs() < 0.1, "{fallen}");
    assert!((v.y + 9.81).abs() < 0.05, "{v}");
}

#[test]
fn pyramid_is_stable() {
    let mut w = world(4);
    let boxes = scenes::pyramid(&mut w, 10).unwrap();
    let top = *boxes.last().unwrap();
    let (start, _) = w.transform(top).unwrap();
    run(&mut w, 5.0);
    let (end, _) = w.transform(top).unwrap();
    println!("top box: {start} -> {end}; stats {:?}", w.stats());
    assert!(
        (end - start).length() < 0.1,
        "the pyramid moved: {start} -> {end}"
    );
    // Everything came to rest.
    assert!(boxes.iter().all(|&b| !w.is_active(b)) || w.stats().active_bodies < 5);
}

#[test]
fn raycasts_and_layer_filters() {
    let mut w = world(0);
    scenes::ground(&mut w).unwrap();
    let boxy = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(1.0),
        })
        .unwrap();
    let crate_body = w
        .create_body(&BodyDesc {
            user_data: 42,
            ..BodyDesc::fixed(boxy, Vec3::new(10.0, 1.0, 0.0))
        })
        .unwrap();
    w.step(DT, 1).unwrap();

    let down = Ray {
        origin: Vec3::new(0.0, 10.0, 0.0),
        direction: Vec3::new(0.0, -20.0, 0.0),
    };
    let hit = w.raycast(&down, QueryFilter::default()).unwrap();
    assert!((hit.point.y).abs() < 1e-3, "{hit:?}");
    assert!((hit.normal - Vec3::Y).length() < 1e-3, "{hit:?}");
    assert!((hit.fraction - 0.5).abs() < 1e-4);

    let onto_crate = Ray {
        origin: Vec3::new(10.0, 10.0, 0.0),
        direction: Vec3::new(0.0, -20.0, 0.0),
    };
    let hit = w.raycast(&onto_crate, QueryFilter::default()).unwrap();
    assert_eq!((hit.body, hit.user_data), (crate_body, 42));
    assert!((hit.point.y - 2.0).abs() < 1e-3);

    // Layer 0 excluded: nothing (everything static is on layer 0).
    let only_moving = QueryFilter::layers(1 << 1);
    assert!(w.raycast(&down, only_moving).is_none());

    // Batched: every ray over the ground hits it at y = 0.
    let rays = scenes::ray_grid(16);
    let mut hits = vec![None; rays.len()];
    w.raycast_batch(&rays, QueryFilter::default(), &mut hits);
    assert!(hits.iter().all(|h| h.is_some_and(|h| h.point.y > -1e-3)));
}

#[test]
fn overlap_and_contact_events() {
    let mut w = world(0);
    let ground = scenes::ground(&mut w).unwrap();
    let sphere = w.create_shape(&ShapeDesc::Sphere { radius: 0.5 }).unwrap();
    let ball = w
        .create_body(&BodyDesc::dynamic(sphere, Vec3::new(0.0, 2.0, 0.0)))
        .unwrap();
    let trigger_shape = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(0.5),
        })
        .unwrap();
    let trigger = w
        .create_body(&BodyDesc {
            sensor: true,
            layer: Layer(1),
            motion: engine_physics::MotionType::Kinematic,
            ..BodyDesc::dynamic(trigger_shape, Vec3::new(0.0, 1.2, 0.0))
        })
        .unwrap();

    let mut began: Vec<(BodyId, BodyId, bool)> = Vec::new();
    for _ in 0..90 {
        w.step(DT, 1).unwrap();
        for e in w.contact_events() {
            assert!(e.a < e.b);
            if e.kind == ContactKind::Begin {
                began.push((e.a, e.b, e.sensor));
            }
        }
    }
    let pair = |x: BodyId, y: BodyId| (x.min(y), x.max(y));
    println!("begin events: {began:?}");
    assert!(
        began
            .iter()
            .any(|&(a, b, s)| (a, b) == pair(ball, trigger) && s),
        "sensor overlap"
    );
    assert!(
        began
            .iter()
            .any(|&(a, b, s)| (a, b) == pair(ball, ground) && !s),
        "landing"
    );

    // The ball rests on the ground: a sphere query around it finds ball and ground but
    // not the sensor unless asked.
    let (p, _) = w.transform(ball).unwrap();
    assert!((p.y - 0.5).abs() < 0.05, "{p}");
    let mut found = Vec::new();
    w.overlap_sphere(p, 0.6, QueryFilter::default(), &mut found);
    assert_eq!(found, {
        let mut v = vec![ground, ball];
        v.sort();
        v
    });
    found.clear();
    w.overlap_sphere(
        p,
        0.6,
        QueryFilter {
            sensors: true,
            ..QueryFilter::default()
        },
        &mut found,
    );
    assert!(found.contains(&trigger));
}

#[test]
fn gravity_modes_and_fields() {
    let mut w = world(0);
    let sphere = w.create_shape(&ShapeDesc::Sphere { radius: 0.5 }).unwrap();
    let body = |w: &mut JoltBackend, x: f32, gravity| {
        w.create_body(&BodyDesc {
            gravity,
            linear_damping: 0.0,
            allow_sleeping: false,
            ..BodyDesc::dynamic(sphere, Vec3::new(x, 0.0, 0.0))
        })
        .unwrap()
    };
    let normal = body(&mut w, 0.0, GravityMode::World);
    let floating = body(&mut w, 10.0, GravityMode::Scaled(0.0));
    let rising = body(&mut w, 20.0, GravityMode::Custom(Vec3::new(0.0, 9.81, 0.0)));
    let sideways = body(&mut w, 30.0, GravityMode::Scaled(1.0));
    w.set_body_gravity(sideways, GravityMode::Custom(Vec3::new(5.0, 0.0, 0.0)));
    run(&mut w, 1.0);
    let y = |w: &JoltBackend, b| w.transform(b).unwrap().0;
    println!(
        "{} {} {} {}",
        y(&w, normal),
        y(&w, floating),
        y(&w, rising),
        y(&w, sideways)
    );
    assert!((y(&w, normal).y + 4.9).abs() < 0.1);
    assert!(y(&w, floating).y.abs() < 1e-4);
    assert!((y(&w, rising).y - 4.9).abs() < 0.1);
    assert!((y(&w, sideways).x - 32.5).abs() < 0.1 && y(&w, sideways).y.abs() < 1e-4);

    // Orbits around a point attractor stay near their radius for several revolutions.
    let mut w = world(0);
    w.set_gravity(Vec3::ZERO);
    let bodies = scenes::orbits(&mut w, 20).unwrap();
    let radii: Vec<f32> = bodies
        .iter()
        .map(|&b| {
            let p = w.transform(b).unwrap().0;
            Vec3::new(p.x, 0.0, p.z).length()
        })
        .collect();
    run(&mut w, 10.0);
    for (&b, r0) in bodies.iter().zip(&radii) {
        let p = w.transform(b).unwrap().0;
        let r = Vec3::new(p.x, 0.0, p.z).length();
        assert!((r - r0).abs() / r0 < 0.1, "orbit drifted from {r0} to {r}");
    }
}

#[test]
fn chain_stays_connected() {
    let mut w = world(2);
    let links = scenes::chain(&mut w, 12).unwrap();
    run(&mut w, 3.0);
    // Consecutive capsule centers stay about one link length (0.8 m) apart.
    for pair in links.windows(2) {
        let a = w.transform(pair[0]).unwrap().0;
        let b = w.transform(pair[1]).unwrap().0;
        let d = (a - b).length();
        assert!((d - 0.8).abs() < 0.1, "links {} apart", d);
    }
    // It swings as a pendulum: the last link left its start and stays within the chain's
    // length (12 × 0.8 m) of the anchor at (0, 13, 0).
    let last = w.transform(*links.last().unwrap()).unwrap().0;
    let start = Vec3::new(0.4 + 11.0 * 0.8, 13.0, 0.0);
    assert!((last - start).length() > 2.0, "{last}");
    assert!(
        (last - Vec3::new(0.0, 13.0, 0.0)).length() < 12.0 * 0.8 + 0.2,
        "{last}"
    );
}

fn pile_hash(threads: u32, steps: u32) -> u64 {
    let mut w = world(threads);
    scenes::pile(&mut w, 300).unwrap();
    for _ in 0..steps {
        w.step(DT, 1).unwrap();
    }
    w.state_hash()
}

#[test]
fn deterministic_for_the_same_binary() {
    let a = pile_hash(0, 120);
    let b = pile_hash(0, 120);
    assert_eq!(a, b, "two single-threaded runs differ");
    let c = pile_hash(4, 120);
    let d = pile_hash(4, 120);
    assert_eq!(c, d, "two multi-threaded runs differ");
    println!("single-threaded {a:#018x}, four threads {c:#018x}");
    assert_eq!(a, c, "the thread count changes the result");
}

#[test]
fn the_engines_pool_steps_the_same_world() {
    // Jolt's jobs handed to `engine_core::jobs` (review 2 §3.10): the same result as on
    // the calling thread and on Jolt's own threads, run after run.
    let pool = std::sync::Arc::new(
        engine_core::jobs::ThreadPool::builder()
            .workers(4)
            .build()
            .unwrap(),
    );
    let on_pool = |steps| {
        let mut w = world(4);
        w.set_jobs(Some(pool.clone()));
        scenes::pile(&mut w, 300).unwrap();
        for _ in 0..steps {
            w.step(DT, 1).unwrap();
        }
        assert_eq!(w.stats().step_threads, 5);
        w.state_hash()
    };
    let (a, b) = (on_pool(120), on_pool(120));
    assert_eq!(a, b, "two runs on the engine's pool differ");
    assert_eq!(a, pile_hash(0, 120), "the engine's pool changes the result");
    // Back to Jolt's own threads.
    let mut w = world(4);
    w.set_jobs(Some(pool.clone()));
    w.set_jobs(None);
    scenes::pile(&mut w, 300).unwrap();
    for _ in 0..120 {
        w.step(DT, 1).unwrap();
    }
    assert_eq!(w.state_hash(), a);
}

#[test]
fn awake_body_count_picks_the_thread_count() {
    let adaptive = |single_thread_below| {
        JoltBackend::new(&PhysicsSettings {
            threads: 3,
            single_thread_below,
            ..PhysicsSettings::default()
        })
        .unwrap()
    };
    // 300 falling bodies: below a threshold of 500 on the calling thread, above 100 on
    // the pool.
    let mut small = adaptive(500);
    scenes::pile(&mut small, 300).unwrap();
    small.step(DT, 1).unwrap();
    assert_eq!(small.stats().step_threads, 1);
    let mut large = adaptive(100);
    scenes::pile(&mut large, 300).unwrap();
    large.step(DT, 1).unwrap();
    assert_eq!(large.stats().step_threads, 4);
    // Switching between the two job systems does not change the result.
    for _ in 1..120 {
        small.step(DT, 1).unwrap();
        large.step(DT, 1).unwrap();
    }
    assert_eq!(small.state_hash(), large.state_hash());
    assert_eq!(small.state_hash(), pile_hash(0, 120));
}

#[test]
fn shapes_and_teleport() {
    let mut w = world(0);
    let hull = w
        .create_shape(&ShapeDesc::ConvexHull {
            points: &[
                Vec3::new(-1.0, 0.0, -1.0),
                Vec3::new(1.0, 0.0, -1.0),
                Vec3::new(0.0, 0.0, 1.0),
                Vec3::new(0.0, 1.5, 0.0),
            ],
            convex_radius: engine_physics::HULL_RADIUS,
        })
        .unwrap();
    let mesh = w
        .create_shape(&ShapeDesc::TriangleMesh {
            vertices: &[
                Vec3::new(-5.0, 0.0, -5.0),
                Vec3::new(5.0, 0.0, -5.0),
                Vec3::new(5.0, 0.0, 5.0),
                Vec3::new(-5.0, 0.0, 5.0),
            ],
            triangles: &[[0, 2, 1], [0, 3, 2]],
            surfaces: &[],
        })
        .unwrap();
    w.create_body(&BodyDesc::fixed(mesh, Vec3::ZERO)).unwrap();
    let cylinder = w
        .create_shape(&ShapeDesc::Cylinder {
            half_height: 0.5,
            radius: 0.3,
        })
        .unwrap();
    let a = w
        .create_body(&BodyDesc::dynamic(hull, Vec3::new(0.0, 2.0, 0.0)))
        .unwrap();
    let b = w
        .create_body(&BodyDesc::dynamic(cylinder, Vec3::new(2.0, 2.0, 0.0)))
        .unwrap();
    run(&mut w, 3.0);
    // Both rest on the mesh.
    assert!(w.transform(a).unwrap().0.y.abs() < 0.1);
    assert!((w.transform(b).unwrap().0.y - 0.5).abs() < 0.1);
    w.set_transform(a, Vec3::new(0.0, 5.0, 0.0), Quat::IDENTITY);
    assert!((w.transform(a).unwrap().0.y - 5.0).abs() < 1e-5);
    assert!(w.is_active(a));
    // Degenerate hull is rejected.
    assert!(
        w.create_shape(&ShapeDesc::ConvexHull {
            points: &[Vec3::ZERO; 4],
            convex_radius: engine_physics::HULL_RADIUS,
        })
        .is_err()
    );
    w.destroy_body(b);
    assert!(w.transform(b).is_none());
}
