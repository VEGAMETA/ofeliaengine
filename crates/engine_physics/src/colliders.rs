//! Colliders in the engine's own form (`engine_collision`, ADR 0101) as a backend's shapes:
//! built, baked for a pack, and placed at any transform.
//!
//! **Placing a collider** ([`placed`]) takes an instance's object-to-world transform:
//!
//! - **rotation and translation only:** the collider's shape as it is;
//! - **a scale the backend takes** (meshes and hulls any; boxes along their axes): a scaled
//!   shape over it, nothing rebuilt;
//! - **a scale it does not take** (a squashed sphere, a turned box stretched across): the
//!   collider rebuilt for that scale by [`engine_collision::Shape::transformed`], exact
//!   where the result is a box, a hull of exact points or a mesh, within a tolerance for
//!   round shapes;
//! - **a shear** (a transform that is not translation × rotation × scale): the whole linear
//!   part baked into the rebuilt shapes, the body only translated.
//!
//! Rebuilt shapes are the caller's to cache per collider and scale.

use crate::{CompoundChild, PhysicsBackend, PhysicsError, ShapeDesc, ShapeId};
use engine_collision::{Collider, Shape};
use engine_core::math::{Affine3A, Mat3, Quat, Vec3};

/// A collider in a backend's binary form (see [`PhysicsBackend::bake_shape`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakedCollider {
    /// [`PhysicsBackend::baked_shape_format`] of the backend that wrote it.
    pub format: u64,
    /// The shape's bytes.
    pub bytes: Vec<u8>,
    /// Whether it can be swept in shape casts (no triangle mesh in it).
    pub castable: bool,
}

/// Whether `collider` can be swept in shape casts (no triangle mesh in it).
pub fn castable(collider: &Collider) -> bool {
    !collider
        .shapes
        .iter()
        .any(|s| matches!(s.shape, Shape::Mesh { .. }))
}

/// One shape as a backend shape made of `surface` (`u16::MAX`: none) and the place of its
/// origin in the collider.
fn part(
    physics: &mut dyn PhysicsBackend,
    shape: &Shape,
    surface: u16,
) -> Result<(ShapeId, Vec3, Quat), PhysicsError> {
    let (desc, position, rotation) = match shape {
        Shape::Box {
            center,
            rotation,
            half_extents,
        } => (
            ShapeDesc::Box {
                half_extents: *half_extents,
            },
            *center,
            *rotation,
        ),
        Shape::Sphere { center, radius } => (
            ShapeDesc::Sphere { radius: *radius },
            *center,
            Quat::IDENTITY,
        ),
        Shape::Capsule {
            center,
            rotation,
            half_height,
            radius,
        } => (
            ShapeDesc::Capsule {
                half_height: *half_height,
                radius: *radius,
            },
            *center,
            *rotation,
        ),
        Shape::Cylinder {
            center,
            rotation,
            half_height,
            radius,
        } => (
            ShapeDesc::Cylinder {
                half_height: *half_height,
                radius: *radius,
            },
            *center,
            *rotation,
        ),
        Shape::Hull { points } => (ShapeDesc::ConvexHull { points }, Vec3::ZERO, Quat::IDENTITY),
        Shape::Mesh {
            vertices,
            triangles,
            surfaces,
        } => (
            ShapeDesc::TriangleMesh {
                vertices,
                triangles,
                surfaces,
            },
            Vec3::ZERO,
            Quat::IDENTITY,
        ),
    };
    let id = physics.create_shape(&desc)?;
    if surface != u16::MAX {
        physics.set_shape_surface(id, surface);
    }
    Ok((id, position, rotation))
}

/// `collider` as one backend shape: its shape when it has one at its origin, otherwise a
/// compound of them all.
pub fn build(
    physics: &mut dyn PhysicsBackend,
    collider: &Collider,
) -> Result<ShapeId, PhysicsError> {
    engine_profiler::zone!("physics::colliders::build");
    let mut parts = Vec::with_capacity(collider.shapes.len());
    for s in &collider.shapes {
        match part(physics, &s.shape, s.surface) {
            Ok(p) => parts.push(p),
            Err(e) => {
                for (shape, ..) in parts {
                    physics.destroy_shape(shape);
                }
                return Err(e);
            }
        }
    }
    match parts.as_slice() {
        [] => Err(PhysicsError::InvalidShape(
            "a collider without shapes".into(),
        )),
        [(shape, position, rotation)] if *position == Vec3::ZERO && *rotation == Quat::IDENTITY => {
            Ok(*shape)
        }
        _ => {
            let children: Vec<CompoundChild> = parts
                .iter()
                .map(|&(shape, position, rotation)| CompoundChild {
                    shape,
                    position,
                    rotation,
                })
                .collect();
            let compound = physics.create_shape(&ShapeDesc::Compound {
                children: &children,
            });
            // The compound keeps its children.
            for (shape, ..) in parts {
                physics.destroy_shape(shape);
            }
            compound
        }
    }
}

/// `collider` baked into the backend's binary form, or `None` if the backend has none.
pub fn bake(
    physics: &mut dyn PhysicsBackend,
    collider: &Collider,
) -> Result<Option<BakedCollider>, PhysicsError> {
    engine_profiler::zone!("physics::colliders::bake");
    let Some(format) = physics.baked_shape_format() else {
        return Ok(None);
    };
    let shape = build(physics, collider)?;
    let bytes = physics.bake_shape(shape);
    physics.destroy_shape(shape);
    Ok(bytes.map(|bytes| BakedCollider {
        format,
        bytes,
        castable: castable(collider),
    }))
}

/// A collider placed in the world: the shape its body uses and the body's pose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    /// The shape for the body: the collider's own, a scaled one over it, or a rebuilt one.
    pub shape: ShapeId,
    /// Whether [`Self::shape`] was made for this placement (the caller owns and caches it)
    /// rather than being the collider's own.
    pub made: bool,
    /// The body's position.
    pub position: Vec3,
    /// The body's rotation.
    pub rotation: Quat,
}

/// The transform as translation × rotation × scale if it is one (no shear), mirrors
/// included (a negative X).
fn trs(transform: Affine3A) -> Option<(Vec3, Quat, Vec3)> {
    let (scale, rotation, translation) = transform.to_scale_rotation_translation();
    if !scale.is_finite() || !rotation.is_finite() {
        return None;
    }
    let again = Affine3A::from_scale_rotation_translation(scale, rotation, translation);
    let size = Mat3::from(transform.matrix3)
        .to_cols_array()
        .iter()
        .fold(0.0f32, |m, x| m.max(x.abs()));
    let tolerance = 1e-5 * size.max(1e-30);
    let matrix_close = (again.matrix3 - transform.matrix3)
        .to_cols_array()
        .iter()
        .all(|d| d.abs() <= tolerance);
    matrix_close.then_some((translation, rotation, scale))
}

/// `collider`, whose backend shape is `shape`, placed by `transform` (see the module's
/// documentation). Round shapes rebuilt for the scale stay within `tolerance` (scene
/// units, after the transform) of their true surface. A scale under 10⁻⁶ on any axis
/// disables the collider (`Ok(None)`), as glTF's physics draft says of a zero scale.
pub fn placed(
    physics: &mut dyn PhysicsBackend,
    collider: &Collider,
    shape: ShapeId,
    transform: Affine3A,
    tolerance: f32,
) -> Result<Option<Placed>, PhysicsError> {
    let linear = Mat3::from(transform.matrix3);
    let smallest = linear
        .x_axis
        .length()
        .min(linear.y_axis.length())
        .min(linear.z_axis.length());
    if smallest < 1e-6 || linear.determinant().abs() < 1e-18 {
        return Ok(None);
    }
    let translation = Vec3::from(transform.translation);
    if let Some((position, rotation, scale)) = trs(transform) {
        if (scale - Vec3::ONE).abs().max_element() <= 1e-6 {
            return Ok(Some(Placed {
                shape,
                made: false,
                position,
                rotation,
            }));
        }
        if physics.is_valid_scale(shape, scale) {
            let scaled = physics.create_shape(&ShapeDesc::Scaled { shape, scale })?;
            return Ok(Some(Placed {
                shape: scaled,
                made: true,
                position,
                rotation,
            }));
        }
        // Rebuilt for the scale; the body keeps the rotation.
        let rebuilt = build(
            physics,
            &collider.transformed(Affine3A::from_scale(scale), tolerance),
        )?;
        return Ok(Some(Placed {
            shape: rebuilt,
            made: true,
            position,
            rotation,
        }));
    }
    // A shear: the linear part goes into the shapes.
    let rebuilt = build(
        physics,
        &collider.transformed(Affine3A::from_mat3(linear), tolerance),
    )?;
    Ok(Some(Placed {
        shape: rebuilt,
        made: true,
        position: translation,
        rotation: Quat::IDENTITY,
    }))
}
