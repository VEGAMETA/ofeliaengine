//! Test and benchmark scenes that run on any backend (CLAUDE.md §8 benchmark suite; after
//! the scenes of Jolt's `PerformanceTest`). Every scene is built by the same API calls in
//! the same order, so two backends (or two runs) see identical worlds.

use crate::{
    BodyDesc, BodyId, ConstraintDesc, GravityField, GravityMode, MotionQuality, PhysicsBackend,
    PhysicsError, Ray, ShapeDesc,
};
use engine_core::math::{Quat, Vec3};

/// A ground slab: 200 × 1 × 200 m, top at y = 0.
pub fn ground(world: &mut dyn PhysicsBackend) -> Result<BodyId, PhysicsError> {
    let shape = world.create_shape(&ShapeDesc::Box {
        half_extents: Vec3::new(100.0, 0.5, 100.0),
    })?;
    world.create_body(&BodyDesc::fixed(shape, Vec3::new(0.0, -0.5, 0.0)))
}

/// A pyramid of 1 m boxes, `levels` high, on the ground (stability). Returns the boxes,
/// bottom row first.
pub fn pyramid(world: &mut dyn PhysicsBackend, levels: u32) -> Result<Vec<BodyId>, PhysicsError> {
    ground(world)?;
    let shape = world.create_shape(&ShapeDesc::Box {
        half_extents: Vec3::splat(0.5),
    })?;
    let mut bodies = Vec::new();
    for level in 0..levels {
        let count = levels - level;
        for i in 0..count {
            let x = (i as f32 - (count - 1) as f32 * 0.5) * 1.02;
            let y = 0.5 + level as f32 * 1.0;
            bodies.push(world.create_body(&BodyDesc::dynamic(shape, Vec3::new(x, y, 0.0)))?);
        }
    }
    Ok(bodies)
}

/// `count` mixed bodies (boxes, spheres, capsules) dropped from a grid above the ground
/// (a pile; broad phase and contact load).
pub fn pile(world: &mut dyn PhysicsBackend, count: u32) -> Result<Vec<BodyId>, PhysicsError> {
    ground(world)?;
    let shapes = [
        world.create_shape(&ShapeDesc::Box {
            half_extents: Vec3::new(0.4, 0.3, 0.5),
        })?,
        world.create_shape(&ShapeDesc::Sphere { radius: 0.4 })?,
        world.create_shape(&ShapeDesc::Capsule {
            half_height: 0.3,
            radius: 0.25,
        })?,
    ];
    let side = (count as f32).cbrt().ceil() as u32;
    let mut bodies = Vec::with_capacity(count as usize);
    for i in 0..count {
        let (x, y, z) = (i % side, i / (side * side), (i / side) % side);
        let position = Vec3::new(
            (x as f32 - side as f32 * 0.5) * 1.2,
            2.0 + y as f32 * 1.2,
            (z as f32 - side as f32 * 0.5) * 1.2,
        );
        let rotation =
            Quat::from_rotation_y(i as f32 * 0.7) * Quat::from_rotation_x(i as f32 * 0.3);
        bodies.push(world.create_body(&BodyDesc {
            rotation,
            ..BodyDesc::dynamic(shapes[(i % 3) as usize], position)
        })?);
    }
    Ok(bodies)
}

/// A chain of `links` capsules hanging from a world anchor at (0, `links` + 1, 0), joined
/// by point constraints (joints). Returns the links, top first.
pub fn chain(world: &mut dyn PhysicsBackend, links: u32) -> Result<Vec<BodyId>, PhysicsError> {
    let shape = world.create_shape(&ShapeDesc::Capsule {
        half_height: 0.3,
        radius: 0.1,
    })?;
    let top = links as f32 + 1.0;
    let mut bodies = Vec::with_capacity(links as usize);
    let mut previous: Option<BodyId> = None;
    for i in 0..links {
        // Horizontal to start with, so the chain swings down.
        let center = Vec3::new(0.4 + i as f32 * 0.8, top, 0.0);
        let body = world.create_body(&BodyDesc {
            rotation: Quat::from_rotation_z(core::f32::consts::FRAC_PI_2),
            allow_sleeping: false,
            ..BodyDesc::dynamic(shape, center)
        })?;
        world.create_constraint(&ConstraintDesc::Point {
            a: body,
            b: previous,
            anchor: Vec3::new(i as f32 * 0.8, top, 0.0),
        })?;
        bodies.push(body);
        previous = Some(body);
    }
    Ok(bodies)
}

/// A point attractor at the origin with `count` spheres in circular orbits around it
/// (gravity fields). Returns the spheres.
pub fn orbits(world: &mut dyn PhysicsBackend, count: u32) -> Result<Vec<BodyId>, PhysicsError> {
    let strength = 400.0;
    world.set_gravity_fields(&[GravityField::Point {
        center: Vec3::ZERO,
        strength,
        min_distance: 1.0,
        radius: 1000.0,
    }]);
    let shape = world.create_shape(&ShapeDesc::Sphere { radius: 0.2 })?;
    let mut bodies = Vec::with_capacity(count as usize);
    for i in 0..count {
        let r = 5.0 + (i % 10) as f32;
        let angle = i as f32 * 2.399_963;
        let position = Vec3::new(r * angle.cos(), (i % 3) as f32 * 0.5 - 0.5, r * angle.sin());
        // Circular orbit speed: v = sqrt(strength / r).
        let tangent = Vec3::new(-angle.sin(), 0.0, angle.cos());
        bodies.push(world.create_body(&BodyDesc {
            linear_velocity: tangent * (strength / r).sqrt(),
            gravity: GravityMode::Fields,
            linear_damping: 0.0,
            angular_damping: 0.0,
            allow_sleeping: false,
            quality: MotionQuality::Discrete,
            ..BodyDesc::dynamic(shape, position)
        })?);
    }
    Ok(bodies)
}

/// `n × n` rays pointing down from y = 50 over a 100 m square (batched queries).
pub fn ray_grid(n: u32) -> Vec<Ray> {
    let mut rays = Vec::with_capacity((n * n) as usize);
    for i in 0..n {
        for j in 0..n {
            let x = (i as f32 + 0.5) / n as f32 * 100.0 - 50.0;
            let z = (j as f32 + 0.5) / n as f32 * 100.0 - 50.0;
            rays.push(Ray {
                origin: Vec3::new(x, 50.0, z),
                direction: Vec3::new(0.0, -100.0, 0.0),
            });
        }
    }
    rays
}
