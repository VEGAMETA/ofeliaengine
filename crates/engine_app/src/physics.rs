//! A scene's physics (ADR 0101): the bodies of a pack's colliders in a Jolt world the game
//! owns and steps from its fixed tick.
//!
//! ```ignore
//! let mut physics = ScenePhysics::new(&PhysicsSettings::default())?;
//! physics.load_scene(&mut player)?;      // after Player::new: the pack's bodies
//! // Game::tick:    physics.step(world, dt)?;
//! // Game::present: physics.interpolate(world, alpha);
//! ```
//!
//! - **Shapes** are restored from the pack's baked bytes when the pack was baked by this
//!   Jolt (a copy: Intel Sponza's level in 5.5 ms); otherwise built from the engine's form
//!   (a pack written without baking). A pack baked by another Jolt version and holding meshes
//!   only in that form is refused: cook it again.
//! - **Placement** follows each body's transform at any scale
//!   (`engine_physics::colliders::placed`); shapes made for a scale are kept per collider
//!   and scale, so bodies scaled alike share one.
//! - **Dynamic and kinematic bodies** get an entity each (`RigidBody`, `PhysicsPose`,
//!   `Transform`); the scene instances they move become its children, so transform
//!   propagation carries them. Static bodies have no entity.
//! - **Layers:** a body the scene names a collision layer for takes the layer of that name
//!   in the world's [`PhysicsSettings::layer_names`] (a name the settings lack is an error);
//!   others take their motion's (static or moving).
//! - **Surfaces:** what a query hit is made of is
//!   `physics().surface_of(hit.body, hit.part)`: the glTF material of the triangle, or of
//!   the prop's mesh.
//! - The game reaches the world through [`ScenePhysics::physics`] (queries, characters,
//!   bodies of its own).

use crate::AppError;
use engine_assets::colliders::{CookedBody, CookedCollider, Motion};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Mat4, Quat, Vec3};
use engine_ecs::{Entity, GlobalTransform, Transform, World};
use engine_physics::ecs::{KinematicTarget, PhysicsPose, PhysicsSync, RigidBody};
use engine_physics::{
    BodyDesc, BodyId, GravityMode, MotionType, PhysicsBackend, PhysicsError, PhysicsSettings,
    ShapeId,
};
use engine_physics_jolt::JoltBackend;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

/// What [`ScenePhysics::load_scene`] made.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SceneBodies {
    /// Static bodies (level geometry).
    pub statics: usize,
    /// Dynamic bodies.
    pub dynamic: usize,
    /// Kinematic bodies.
    pub kinematic: usize,
    /// Sensors among them.
    pub sensors: usize,
    /// Colliders restored from baked bytes.
    pub restored: usize,
    /// Colliders built from the engine's form (not baked, or baked by another backend).
    pub built: usize,
    /// Shapes made for a scale (or a shear) the collider's own shape could not take as it is.
    pub placed_shapes: usize,
    /// Milliseconds: reading the entries, making the shapes, adding the bodies.
    pub load_ms: f64,
}

/// A scene's physics world (see the module's documentation).
pub struct ScenePhysics {
    physics: JoltBackend,
    sync: Option<PhysicsSync>,
    /// Each collider's shape.
    shapes: Vec<ShapeId>,
    /// Shapes made for a placement, by collider and [`placement_key`]: the shape, and
    /// whether the transform's shear went into it (the body then only translates).
    placed: HashMap<(u32, [i32; 9]), (ShapeId, bool)>,
    bodies: Vec<BodyId>,
    /// The world's settings: the layers the scene's bodies name.
    settings: PhysicsSettings,
}

/// Placement tolerance for round shapes rebuilt for a scale (scene units).
const TOLERANCE: f32 = 1e-3;

impl ScenePhysics {
    /// An empty world.
    pub fn new(settings: &PhysicsSettings) -> Result<Self, AppError> {
        Ok(Self {
            physics: JoltBackend::new(settings).map_err(AppError::at("the physics world"))?,
            sync: None,
            shapes: Vec::new(),
            placed: HashMap::new(),
            bodies: Vec::new(),
            settings: settings.clone(),
        })
    }

    /// The physics world: queries, characters, bodies of the game's own.
    pub fn physics(&mut self) -> &mut JoltBackend {
        &mut self.physics
    }

    /// The bodies made from the scene, in the pack's order.
    pub fn bodies(&self) -> &[BodyId] {
        &self.bodies
    }

    /// The bodies of the pack at `path` (see the module's documentation). `instances`: the
    /// scene instances' entities in the pack's order (what dynamic bodies move).
    pub fn load_pack(
        &mut self,
        path: &Path,
        jobs: &ThreadPool,
        world: &mut World,
        instances: &[Entity],
    ) -> Result<SceneBodies, AppError> {
        engine_profiler::zone!("app::physics::load_pack");
        let start = Instant::now();
        let pack = engine_assets::pack::Pack::open(path).map_err(AppError::at("the pack"))?;
        let (colliders, bodies) = engine_assets::colliders::load_colliders(&pack, jobs, 8)
            .map_err(AppError::at("the pack's colliders"))?;
        let mut report = self.add(&colliders, &bodies, world, instances)?;
        report.load_ms = start.elapsed().as_secs_f64() * 1e3;
        Ok(report)
    }

    /// The bodies of colliders and bodies already read (as [`Self::load_pack`] does).
    pub fn add(
        &mut self,
        colliders: &[CookedCollider],
        bodies: &[CookedBody],
        world: &mut World,
        instances: &[Entity],
    ) -> Result<SceneBodies, AppError> {
        let mut report = SceneBodies::default();
        let first_collider = self.shapes.len() as u32;
        let format = self.physics.baked_shape_format();
        for (k, c) in colliders.iter().enumerate() {
            let shape = match &c.baked {
                Some(b) if Some(b.format) == format => {
                    report.restored += 1;
                    self.physics.create_baked_shape(&b.bytes, b.castable)
                }
                _ => {
                    let has_geometry = c.collider.shapes.iter().all(|s| match &s.shape {
                        engine_collision::Shape::Mesh { triangles, .. } => !triangles.is_empty(),
                        _ => true,
                    });
                    if !has_geometry {
                        return Err(AppError::message(
                            "the pack's colliders",
                            format!(
                                "collider {k} was baked by another physics build and keeps its mesh only in \
                                 that form: cook the pack again"
                            ),
                        ));
                    }
                    report.built += 1;
                    engine_physics::colliders::build(&mut self.physics, &c.collider)
                }
            }
            .map_err(AppError::at("a collider's shape"))?;
            self.shapes.push(shape);
        }
        if self.sync.is_none() {
            self.sync = Some(PhysicsSync::new(world));
        }
        for body in bodies {
            let collider = first_collider + body.collider;
            let Some(&shape) = self.shapes.get(collider as usize) else {
                return Err(AppError::message(
                    "the pack's bodies",
                    "a body of a missing collider".into(),
                ));
            };
            let transform = Affine3A::from_mat4(Mat4::from_cols_array(&body.world_from_object));
            let (key_values, sheared) = placement_key(transform);
            let key = (collider, key_values);
            let placed = match self.placed.get(&key) {
                Some(&(s, sheared)) => {
                    let (_, rotation, position) = transform.to_scale_rotation_translation();
                    Some(engine_physics::colliders::Placed {
                        shape: s,
                        made: true,
                        position: if sheared {
                            Vec3::from(transform.translation)
                        } else {
                            position
                        },
                        rotation: if sheared { Quat::IDENTITY } else { rotation },
                    })
                }
                None => engine_physics::colliders::placed(
                    &mut self.physics,
                    &colliders[body.collider as usize].collider,
                    shape,
                    transform,
                    TOLERANCE,
                )
                .map_err(AppError::at("placing a collider"))?,
            };
            let Some(placed) = placed else {
                // A zero scale: disabled.
                continue;
            };
            if placed.made && !self.placed.contains_key(&key) {
                self.placed.insert(key, (placed.shape, sheared));
                report.placed_shapes += 1;
            }
            let motion = match body.motion {
                Motion::Static => MotionType::Static,
                Motion::Kinematic => MotionType::Kinematic,
                Motion::Dynamic => MotionType::Dynamic,
            };
            let mut desc = if motion == MotionType::Dynamic {
                BodyDesc::dynamic(placed.shape, placed.position)
            } else {
                BodyDesc::fixed(placed.shape, placed.position)
            };
            desc.motion = motion;
            if let Some(name) = &body.layer {
                desc.layer = self.settings.layer(name).ok_or_else(|| {
                    AppError::message(
                        "the pack's bodies",
                        format!(
                            "a body's collision layer `{name}` is not in the world's layer names {:?} \
                             (PhysicsSettings::layer_names)",
                            self.settings.layer_names
                        ),
                    )
                })?;
            }
            desc.rotation = placed.rotation;
            desc.friction = body.friction;
            desc.restitution = body.restitution;
            desc.mass = body.mass;
            desc.sensor = body.sensor;
            desc.linear_velocity = Vec3::from(body.linear_velocity);
            desc.angular_velocity = Vec3::from(body.angular_velocity);
            if body.gravity_factor != 1.0 {
                desc.gravity = GravityMode::Scaled(body.gravity_factor);
            }
            let id = self
                .physics
                .create_body(&desc)
                .map_err(AppError::at("a body"))?;
            self.bodies.push(id);
            report.sensors += usize::from(body.sensor);
            match body.motion {
                Motion::Static => report.statics += 1,
                Motion::Kinematic => report.kinematic += 1,
                Motion::Dynamic => report.dynamic += 1,
            }
            if body.motion == Motion::Static {
                continue;
            }
            // An entity per moving body; the instances it moves become its children.
            let pose = Affine3A::from_rotation_translation(placed.rotation, placed.position);
            let entity = world.spawn((
                RigidBody::new(id),
                PhysicsPose::at(placed.position, placed.rotation),
                Transform::from_translation(placed.position).with_rotation(placed.rotation),
                GlobalTransform(pose),
            ));
            if body.motion == Motion::Kinematic {
                world.insert(
                    entity,
                    KinematicTarget::new(placed.position, placed.rotation),
                );
            }
            let from_world = pose.inverse();
            for &i in &body.instances {
                let Some(&child) = instances.get(i as usize) else {
                    continue;
                };
                let Some(&GlobalTransform(at)) = world.get::<GlobalTransform>(child) else {
                    continue;
                };
                let (scale, rotation, translation) =
                    (from_world * at).to_scale_rotation_translation();
                world.insert(
                    child,
                    Transform::from_translation(translation)
                        .with_rotation(rotation)
                        .with_scale(scale),
                );
                world
                    .set_parent(child, Some(entity))
                    .map_err(|e| AppError::message("a body's instances", format!("{e:?}")))?;
            }
        }
        // A level's bodies all at once: the broad phase's tree built for them.
        self.physics.optimize_broad_phase();
        Ok(report)
    }

    /// One fixed step of `dt` seconds: kinematic targets, the simulation, the poses back.
    /// Call it from `Game::tick` after the game's own logic.
    pub fn step(&mut self, world: &mut World, dt: f32) -> Result<(), PhysicsError> {
        engine_profiler::zone!("app::physics::step");
        let Some(sync) = self.sync.as_mut() else {
            return self.physics.step(dt, 1);
        };
        sync.push(world, &mut self.physics, dt);
        self.physics.step(dt, 1)?;
        sync.pull(world, &self.physics);
        Ok(())
    }

    /// Every rendered frame: moving bodies' entities between their last two steps
    /// (`FixedStep::alpha`). Call it from `Game::present`.
    pub fn interpolate(&mut self, world: &mut World, alpha: f32) {
        if let Some(sync) = self.sync.as_mut() {
            sync.interpolate(world, alpha);
        }
    }
}

/// What shapes made for a placement are shared by: the transform's scale when it is
/// translation × rotation × scale (bodies turned differently share the scaled shape),
/// otherwise its whole linear part (the shear went into the shape); quantized to 10⁻⁴.
/// Also whether it has shear.
fn placement_key(transform: Affine3A) -> ([i32; 9], bool) {
    let (scale, rotation, _) = transform.to_scale_rotation_translation();
    let m = transform.matrix3.to_cols_array();
    let size = m.iter().fold(0.0f32, |a, x| a.max(x.abs())).max(1e-30);
    let back = Affine3A::from_scale_rotation_translation(scale, rotation, Vec3::ZERO);
    let sheared = (back.matrix3 - transform.matrix3)
        .to_cols_array()
        .iter()
        .any(|d| d.abs() > 1e-5 * size);
    let values = if sheared {
        m
    } else {
        [scale.x, scale.y, scale.z, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    };
    (values.map(|v| (v * 1e4).round() as i32), sheared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_assets::colliders::CookedCollider;
    use engine_collision::{Collider, ColliderShape, Shape};
    use engine_ecs::TransformPropagation;

    fn collider(shape: Shape) -> CookedCollider {
        CookedCollider {
            collider: Collider {
                shapes: vec![ColliderShape {
                    id: 0,
                    shape,
                    surface: 0,
                }],
                ..Collider::default()
            },
            baked: None,
        }
    }

    #[test]
    fn a_prop_falls_onto_the_level_and_carries_its_instance() {
        let mut world = World::new();
        let mut propagation = TransformPropagation::new(&mut world);
        // The level: a big flat box; the prop: a unit cube scaled by 2 at height 5.
        let colliders = vec![
            collider(Shape::Box {
                center: Vec3::new(0.0, -0.5, 0.0),
                rotation: Quat::IDENTITY,
                half_extents: Vec3::new(10.0, 0.5, 10.0),
            }),
            collider(Shape::Box {
                center: Vec3::ZERO,
                rotation: Quat::IDENTITY,
                half_extents: Vec3::splat(0.5),
            }),
        ];
        let prop_at = Mat4::from_scale_rotation_translation(
            Vec3::splat(2.0),
            Quat::IDENTITY,
            Vec3::new(0.0, 5.0, 0.0),
        );
        let bodies = vec![
            CookedBody {
                collider: 0,
                ..CookedBody::default()
            },
            CookedBody {
                collider: 1,
                world_from_object: prop_at.to_cols_array(),
                instances: vec![0],
                motion: Motion::Dynamic,
                ..CookedBody::default()
            },
        ];
        // The prop's render instance, as the player spawns it.
        let instance = world.spawn((GlobalTransform(Affine3A::from_mat4(prop_at)),));
        let mut physics = ScenePhysics::new(&PhysicsSettings {
            threads: 0,
            ..PhysicsSettings::default()
        })
        .unwrap();
        let report = physics
            .add(&colliders, &bodies, &mut world, &[instance])
            .unwrap();
        assert_eq!((report.statics, report.dynamic, report.built), (1, 1, 2));
        assert_eq!(report.placed_shapes, 1, "the scaled prop");
        for _ in 0..240 {
            physics.step(&mut world, 1.0 / 60.0).unwrap();
        }
        physics.interpolate(&mut world, 1.0);
        propagation.run(&mut world);
        // Resting on the level: a cube of 2 m stands 1 m above the floor.
        let at = world.get::<GlobalTransform>(instance).unwrap().0;
        let (scale, _, translation) = at.to_scale_rotation_translation();
        assert!((translation.y - 1.0).abs() < 0.05, "{translation}");
        assert!(
            (scale - Vec3::splat(2.0)).length() < 1e-4,
            "the instance keeps its scale: {scale}"
        );
    }
}
