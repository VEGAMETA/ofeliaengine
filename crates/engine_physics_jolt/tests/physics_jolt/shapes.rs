//! Compound and scaled shapes, and shapes baked into Jolt's binary form (ADR 0101), against
//! geometry with known answers.

#![allow(clippy::unwrap_used)]

use engine_core::math::{Quat, Vec3};
use engine_physics::{
    BodyDesc, CompoundChild, PhysicsBackend, PhysicsSettings, QueryFilter, Ray, ShapeCast,
    ShapeDesc, ShapeId,
};
use engine_physics_jolt::JoltBackend;

fn world() -> JoltBackend {
    JoltBackend::new(&PhysicsSettings {
        threads: 0,
        ..PhysicsSettings::default()
    })
    .unwrap()
}

/// Where a ray from far along `from` (towards the origin) first hits the static body.
fn hit_distance(w: &JoltBackend, from: Vec3) -> Option<f32> {
    let ray = Ray {
        origin: from * 100.0,
        direction: -from * 200.0,
    };
    w.raycast(&ray, QueryFilter::default())
        .map(|h| h.point.dot(from))
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 2e-3
}

fn place(w: &mut JoltBackend, shape: ShapeId) {
    w.create_body(&BodyDesc::fixed(shape, Vec3::ZERO)).unwrap();
}

#[test]
fn a_compound_holds_its_children_where_placed() {
    let mut w = world();
    let small = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(0.5),
        })
        .unwrap();
    let ball = w.create_shape(&ShapeDesc::Sphere { radius: 0.25 }).unwrap();
    let children = [
        CompoundChild {
            shape: small,
            position: Vec3::new(2.0, 0.0, 0.0),
            rotation: Quat::IDENTITY,
        },
        CompoundChild {
            shape: ball,
            position: Vec3::new(0.0, 0.0, -3.0),
            rotation: Quat::IDENTITY,
        },
    ];
    let compound = w
        .create_shape(&ShapeDesc::Compound {
            children: &children,
        })
        .unwrap();
    // The children may go: the compound keeps them.
    w.destroy_shape(small);
    w.destroy_shape(ball);
    place(&mut w, compound);
    assert!(close(hit_distance(&w, Vec3::X).unwrap(), 2.5));
    assert!(close(hit_distance(&w, -Vec3::Z).unwrap(), 3.25));
    // Nothing at the compound's own origin.
    assert!(hit_distance(&w, Vec3::Y).is_none());
}

#[test]
fn scales_jolt_takes_and_refuses() {
    let mut w = world();
    let cube = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(0.5),
        })
        .unwrap();
    let ball = w.create_shape(&ShapeDesc::Sphere { radius: 0.5 }).unwrap();
    let stretch = Vec3::new(3.0, 1.0, 0.5);
    assert!(w.is_valid_scale(cube, stretch));
    assert!(!w.is_valid_scale(ball, stretch));
    assert!(w.is_valid_scale(ball, Vec3::splat(-2.0)));
    // A refused scale is an error to rebuild for, not a silent average.
    assert!(
        w.create_shape(&ShapeDesc::Scaled {
            shape: ball,
            scale: stretch
        })
        .is_err()
    );
    let scaled = w
        .create_shape(&ShapeDesc::Scaled {
            shape: cube,
            scale: stretch,
        })
        .unwrap();
    place(&mut w, scaled);
    assert!(close(hit_distance(&w, Vec3::X).unwrap(), 1.5));
    assert!(close(hit_distance(&w, Vec3::Z).unwrap(), 0.25));
    // Mirrored: the same box.
    let mut w = world();
    let cube = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(0.5),
        })
        .unwrap();
    let mirrored = w
        .create_shape(&ShapeDesc::Scaled {
            shape: cube,
            scale: Vec3::new(-2.0, 1.0, 1.0),
        })
        .unwrap();
    place(&mut w, mirrored);
    assert!(close(hit_distance(&w, Vec3::X).unwrap(), 1.0));
    assert!(close(hit_distance(&w, -Vec3::X).unwrap(), 1.0));
}

#[test]
fn a_turned_child_refuses_a_shearing_scale() {
    let mut w = world();
    let cube = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(0.5),
        })
        .unwrap();
    let turned = [CompoundChild {
        shape: cube,
        position: Vec3::ZERO,
        rotation: Quat::from_rotation_z(0.6),
    }];
    let compound = w
        .create_shape(&ShapeDesc::Compound { children: &turned })
        .unwrap();
    assert!(w.is_valid_scale(compound, Vec3::splat(2.0)));
    assert!(!w.is_valid_scale(compound, Vec3::new(2.0, 1.0, 1.0)));
}

#[test]
fn baked_shapes_restore_the_same_geometry() {
    let mut w = world();
    let format = w.baked_shape_format().unwrap();
    assert_eq!(Some(format), world().baked_shape_format());
    // A mesh (a square pyramid without its base) and a compound with a hull.
    let vertices = [
        Vec3::new(-1.0, 0.0, -1.0),
        Vec3::new(1.0, 0.0, -1.0),
        Vec3::new(1.0, 0.0, 1.0),
        Vec3::new(-1.0, 0.0, 1.0),
        Vec3::new(0.0, 1.0, 0.0),
    ];
    let triangles = [[0, 4, 1], [1, 4, 2], [2, 4, 3], [3, 4, 0]];
    let mesh = w
        .create_shape(&ShapeDesc::TriangleMesh {
            vertices: &vertices,
            triangles: &triangles,
            surfaces: &[],
        })
        .unwrap();
    let hull = w
        .create_shape(&ShapeDesc::ConvexHull { points: &vertices })
        .unwrap();
    let compound = w
        .create_shape(&ShapeDesc::Compound {
            children: &[CompoundChild {
                shape: hull,
                position: Vec3::new(0.0, 0.0, 5.0),
                rotation: Quat::IDENTITY,
            }],
        })
        .unwrap();
    let mesh_bytes = w.bake_shape(mesh).unwrap();
    let compound_bytes = w.bake_shape(compound).unwrap();

    let mut restored = world();
    let mesh = restored.create_baked_shape(&mesh_bytes, false).unwrap();
    let compound = restored.create_baked_shape(&compound_bytes, true).unwrap();
    // Baking again gives the same bytes.
    assert_eq!(restored.bake_shape(mesh).unwrap(), mesh_bytes);
    assert_eq!(restored.bake_shape(compound).unwrap(), compound_bytes);
    place(&mut restored, mesh);
    place(&mut restored, compound);
    let down = Ray {
        origin: Vec3::new(0.25, 10.0, 0.0),
        direction: Vec3::new(0.0, -20.0, 0.0),
    };
    let hit = restored.raycast(&down, QueryFilter::default()).unwrap();
    assert!(close(hit.point.y, 0.75), "{hit:?}");
    let down = Ray {
        origin: Vec3::new(0.0, 10.0, 5.0),
        direction: Vec3::new(0.0, -20.0, 0.0),
    };
    assert!(close(
        restored
            .raycast(&down, QueryFilter::default())
            .unwrap()
            .point
            .y,
        1.0
    ));
    // A castable compound sweeps; a mesh does not.
    let cast = ShapeCast {
        shape: compound,
        position: Vec3::new(0.0, 20.0, -5.0),
        rotation: Quat::IDENTITY,
        direction: Vec3::new(0.0, -20.0, 0.0),
    };
    assert!(restored.cast_shape(&cast, QueryFilter::default()).is_some());
    assert!(
        restored
            .cast_shape(
                &ShapeCast {
                    shape: mesh,
                    ..cast
                },
                QueryFilter::default()
            )
            .is_none()
    );
}

#[test]
fn damaged_bytes_are_refused() {
    let mut w = world();
    let cube = w
        .create_shape(&ShapeDesc::Box {
            half_extents: Vec3::splat(0.5),
        })
        .unwrap();
    let bytes = w.bake_shape(cube).unwrap();
    assert!(
        w.create_baked_shape(&bytes[..bytes.len() - 1], true)
            .is_err()
    );
    assert!(w.create_baked_shape(&[], true).is_err());
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(w.create_baked_shape(&longer, true).is_err());
}
