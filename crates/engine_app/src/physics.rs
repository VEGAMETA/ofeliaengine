//! A scene's physics (ADR 0101): the bodies of a pack's colliders in a Jolt world the game
//! owns and steps from its fixed tick.
//!
//! ```ignore
//! let mut physics = ScenePhysics::new(&PhysicsSettings::default())?;
//! physics.load_pack(&pack, &jobs, world, &instances)?; // the pack's bodies
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
//! - **Saves** (ADR 0104): a moving body's entity takes its first instance's id, so loading
//!   matches it; [`ScenePhysics::save`] keeps gravity and the bodies' velocities,
//!   [`ScenePhysics::restore`] puts the bodies where their entities were saved, at their
//!   saved velocities (`Game::before_save` and `Game::after_load`, one line each).
//! - **A scene file's models** (ADR 0117): [`ScenePhysics::sync_models`] gives each model
//!   the body its collider asks for ([`crate::Assets::collider`]) and keeps it in step.
//! - **Bodies name their entities** (ADR 0114 E7-3): a model's body, a brush group's and a
//!   pack's moving body carry their entity in their user data
//!   ([`engine_physics::ecs::entity_user_data`]), so query hits and contacts give it back
//!   ([`ScenePhysics::entity_of`]); scripts' `physics.raycast` reads it so.

use crate::AppError;
use crate::assets::{Assets, ColliderState, Model, ModelCollider};
use engine_assets::colliders::{CookedBody, CookedCollider, Motion};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Mat4, Quat, Vec3};
use engine_ecs::{Changed, Entity, GlobalTransform, Or, QueryState, Transform, Without, World};
use engine_physics::ecs::{
    KinematicTarget, PhysicsPose, PhysicsSync, RigidBody, entity_user_data, user_data_entity,
};
use engine_physics::{
    BodyDesc, BodyId, CompoundChild, GravityMode, MotionType, PhysicsBackend, PhysicsError,
    PhysicsSettings, ShapeDesc, ShapeId,
};
use engine_physics_jolt::JoltBackend;
use engine_reflect::Reflect;
use engine_runtime::{ActivityEpoch, Inactive};
use engine_scene::{AssetRef, SceneId};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// What [`ScenePhysics::load_pack`] made.
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
    /// The moving bodies with ids, for saves (made with the pack's bodies).
    saved: Option<engine_ecs::QueryState<(&'static RigidBody, &'static SceneId)>>,
    /// The blockout's bodies, by brush group (ADR 0110), and the groups seen by the last
    /// sync.
    blockout: HashMap<Entity, BlockoutBody>,
    blockout_seen: Vec<Entity>,
    /// The models' bodies (ADR 0117), by entity; each mesh's shape, made from its
    /// collider and shared by its models; the shapes placed for a scale, by mesh and
    /// scale; and what [`Self::sync_models`] follows of the world.
    models: HashMap<Entity, ModelBody>,
    model_shapes: HashMap<AssetRef, (Arc<ModelCollider>, ShapeId)>,
    model_placed: HashMap<(AssetRef, [i32; 9]), (ShapeId, bool)>,
    model_sync: Option<ModelSync>,
}

/// A model's body: the mesh, collider and place it was made for, and whether it moves its
/// entity (dynamic or kinematic).
#[derive(Debug)]
struct ModelBody {
    mesh: AssetRef,
    collider: Arc<ModelCollider>,
    placed: [f32; 12],
    body: BodyId,
    moving: bool,
}

type ModelRows = QueryState<
    (Entity, &'static Model, &'static GlobalTransform),
    (
        Or<(Changed<Model>, Changed<GlobalTransform>)>,
        Without<Inactive>,
    ),
>;
type AllModels = QueryState<(Entity, &'static Model, &'static GlobalTransform), Without<Inactive>>;

/// What [`ScenePhysics::sync_models`] follows of the world: the models that changed, all
/// of them for a rescan, those waiting for their colliders, the epochs that ask for a
/// rescan (the assets' colliders, the entities turned off or on, the world restored), and
/// the frame's models (kept between calls).
struct ModelSync {
    changed: ModelRows,
    all: AllModels,
    waiting: Vec<Entity>,
    epochs: (u64, u64, u64),
    seen: Vec<(Entity, Model, Affine3A)>,
    gone: Vec<Entity>,
}

/// A brush group's static body: the group's epoch and place it was made for, its body and
/// its shapes (the hulls and their compound).
#[derive(Debug)]
struct BlockoutBody {
    epoch: u64,
    placed: [f32; 12],
    body: BodyId,
    shapes: Vec<ShapeId>,
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
            saved: None,
            blockout: HashMap::new(),
            blockout_seen: Vec::new(),
            models: HashMap::new(),
            model_shapes: HashMap::new(),
            model_placed: HashMap::new(),
            model_sync: None,
        })
    }

    /// The physics world: queries, characters, bodies of the game's own.
    pub fn physics(&mut self) -> &mut JoltBackend {
        &mut self.physics
    }

    /// The physics world for queries alone: what sound travels through
    /// ([`crate::Game::acoustics`]).
    pub fn backend(&self) -> &JoltBackend {
        &self.physics
    }

    /// The world's settings: the collision layers by name.
    pub fn settings(&self) -> &PhysicsSettings {
        &self.settings
    }

    /// The entity body `body` belongs to, as its user data names it (a model's, a brush
    /// group's, a pack's moving body's, or a body the game gave
    /// [`engine_physics::ecs::entity_user_data`]).
    pub fn entity_of(&self, body: BodyId) -> Option<Entity> {
        user_data_entity(self.physics.user_data(body))
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
            self.physics.set_user_data(id, entity_user_data(entity));
            // Known to saves by its first instance's id (pack version 9).
            if let Some(id) = body
                .instances
                .first()
                .and_then(|&i| instances.get(i as usize))
                .and_then(|&e| world.get::<SceneId>(e).copied())
            {
                world.insert(entity, body_id(id));
            }
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
        if self.saved.is_none() {
            self.saved = Some(engine_ecs::QueryState::new(world));
        }
        Ok(report)
    }

    /// The blockout's colliders (ADR 0110): a static body per brush group, the pieces of its
    /// brushes that collide as sharp convex hulls in one compound, in the world. Bodies that
    /// move over it with [`BodyDesc::internal_edges`] roll and slide on over the seams
    /// between its pieces (measured: no knock, where rounded hulls or separate bodies knock
    /// 3-8 mm). A group made again when its pieces or its place change, gone with it. Call
    /// it once the player has drawn a frame of the brushes (from `Game::tick`, a frame after
    /// an edit); it returns how many groups it made again.
    pub fn sync_blockout(
        &mut self,
        world: &World,
        blockout: &crate::Blockout,
    ) -> Result<usize, AppError> {
        engine_profiler::zone!("app::physics::sync_blockout");
        let mut made = 0;
        // Kept between calls: called every frame, it allocates nothing while nothing
        // changes.
        let mut seen = core::mem::take(&mut self.blockout_seen);
        seen.clear();
        let mut hulls = Vec::new();
        for (group, epoch) in blockout.groups() {
            seen.push(group);
            let placed = world
                .get::<GlobalTransform>(group)
                .map_or(Affine3A::IDENTITY, |g| g.0);
            let key = placed.to_cols_array();
            if self
                .blockout
                .get(&group)
                .is_some_and(|b| b.epoch == epoch && b.placed == key)
            {
                continue;
            }
            if let Some(old) = self.blockout.remove(&group) {
                self.drop_blockout_body(old);
            }
            blockout.hulls(group, &mut hulls);
            let mut shapes = Vec::with_capacity(hulls.len() + 1);
            let mut children = Vec::with_capacity(hulls.len());
            for points in &hulls {
                let points: Vec<Vec3> =
                    points.iter().map(|&p| placed.transform_point3(p)).collect();
                // A sliver of a piece makes no hull: left out, the rest still collide.
                let Ok(shape) = self.physics.create_shape(&ShapeDesc::ConvexHull {
                    points: &points,
                    convex_radius: 0.0,
                }) else {
                    continue;
                };
                shapes.push(shape);
                children.push(CompoundChild {
                    shape,
                    position: Vec3::ZERO,
                    rotation: Quat::IDENTITY,
                });
            }
            if children.is_empty() {
                for s in shapes {
                    self.physics.destroy_shape(s);
                }
                continue;
            }
            let compound = self
                .physics
                .create_shape(&ShapeDesc::Compound {
                    children: &children,
                })
                .map_err(AppError::at("a brush group's collider"))?;
            shapes.push(compound);
            let body = self
                .physics
                .create_body(&BodyDesc {
                    user_data: entity_user_data(group),
                    ..BodyDesc::fixed(compound, Vec3::ZERO)
                })
                .map_err(AppError::at("a brush group's body"))?;
            self.blockout.insert(
                group,
                BlockoutBody {
                    epoch,
                    placed: key,
                    body,
                    shapes,
                },
            );
            made += 1;
        }
        let gone: Vec<Entity> = self
            .blockout
            .keys()
            .copied()
            .filter(|g| !seen.contains(g))
            .collect();
        for g in gone {
            if let Some(old) = self.blockout.remove(&g) {
                self.drop_blockout_body(old);
            }
        }
        self.blockout_seen = seen;
        Ok(made)
    }

    /// The blockout's body of `group`, if it has one.
    pub fn blockout_body(&self, group: Entity) -> Option<BodyId> {
        self.blockout.get(&group).map(|b| b.body)
    }

    /// The bodies of a scene file's models (ADR 0117): each active model whose mesh has a
    /// collider ([`Assets::collider`]: by the rules beside its glTF, level geometry by
    /// default) gets the body it asks for, placed at any scale:
    /// - a static one at its entity's place, made again when the entity moves or its
    ///   collider changes;
    /// - a dynamic or kinematic one gets `RigidBody` and `PhysicsPose`, and its entity
    ///   becomes a root of the hierarchy where it is in the world: the physics moves it
    ///   ([`Self::step`], [`Self::interpolate`]); a kinematic one also gets a
    ///   `KinematicTarget`;
    /// - a model turned off, despawned, or whose mesh no longer collides loses its body;
    ///   one whose collider is still being cooked gets it once it comes.
    ///
    /// Call it from `Game::tick`, before [`Self::step`]: it visits only the models that
    /// changed (and every one when colliders were made, entities turned off or on, or the
    /// world restored). Returns how many bodies it made.
    pub fn sync_models(
        &mut self,
        world: &mut World,
        assets: &mut Assets,
    ) -> Result<usize, AppError> {
        engine_profiler::zone!("app::physics::sync_models");
        let mut sync = match self.model_sync.take() {
            Some(sync) => sync,
            None => {
                world.track_removals::<Model>();
                ModelSync {
                    changed: QueryState::new(world),
                    all: QueryState::new(world),
                    waiting: Vec::new(),
                    epochs: (u64::MAX, u64::MAX, u64::MAX),
                    seen: Vec::new(),
                    gone: Vec::new(),
                }
            }
        };
        let made = self.sync_models_with(&mut sync, world, assets);
        self.model_sync = Some(sync);
        made
    }

    fn sync_models_with(
        &mut self,
        sync: &mut ModelSync,
        world: &mut World,
        assets: &mut Assets,
    ) -> Result<usize, AppError> {
        // Models gone: their bodies with them.
        sync.gone.clear();
        world.drain_removed::<Model>(|e| sync.gone.push(e));
        for i in 0..sync.gone.len() {
            let e = sync.gone[i];
            if world.get::<Model>(e).is_none()
                && let Some(old) = self.models.remove(&e)
            {
                self.drop_model_body(world, e, old);
            }
        }
        let epochs = (
            assets.collider_epoch(),
            world.resource::<ActivityEpoch>().map_or(0, |a| a.0),
            world.resync_epoch(),
        );
        sync.seen.clear();
        if epochs != sync.epochs {
            sync.epochs = epochs;
            // Every active model; the bodies of the others go.
            sync.seen
                .extend(sync.all.iter_readonly(world).map(|(e, m, g)| (e, *m, g.0)));
            let active: HashSet<Entity> = sync.seen.iter().map(|&(e, _, _)| e).collect();
            let off: Vec<Entity> = self
                .models
                .keys()
                .copied()
                .filter(|e| !active.contains(e))
                .collect();
            for e in off {
                if let Some(old) = self.models.remove(&e) {
                    self.drop_model_body(world, e, old);
                }
            }
        } else {
            sync.seen.extend(
                sync.changed
                    .iter_readonly(world)
                    .map(|(e, m, g)| (e, *m, g.0)),
            );
            for &e in &sync.waiting {
                if let (Some(&m), Some(&g)) =
                    (world.get::<Model>(e), world.get::<GlobalTransform>(e))
                    && world.get::<Inactive>(e).is_none()
                {
                    sync.seen.push((e, m, g.0));
                }
            }
        }
        sync.waiting.clear();
        let mut made = 0;
        for i in 0..sync.seen.len() {
            let (e, model, place) = sync.seen[i];
            let collider = match assets.collider(model.mesh) {
                ColliderState::Made(c) => c,
                ColliderState::Coming => {
                    sync.waiting.push(e);
                    continue;
                }
                ColliderState::None => {
                    if let Some(old) = self.models.remove(&e) {
                        self.drop_model_body(world, e, old);
                    }
                    continue;
                }
            };
            let key = place.to_cols_array();
            if let Some(b) = self.models.get(&e)
                && b.mesh == model.mesh
                && Arc::ptr_eq(&b.collider, &collider)
                // A moving body moves its entity: that is no change.
                && (b.moving || b.placed == key)
            {
                continue;
            }
            if let Some(old) = self.models.remove(&e) {
                self.drop_model_body(world, e, old);
            }
            if let Some(body) = self.make_model_body(world, e, model.mesh, &collider, place)? {
                self.models.insert(e, body);
                made += 1;
            }
        }
        Ok(made)
    }

    /// The body model `e` of mesh `mesh` asks for with `collider`, at world `place`; none
    /// at a zero scale.
    fn make_model_body(
        &mut self,
        world: &mut World,
        e: Entity,
        mesh: AssetRef,
        collider: &Arc<ModelCollider>,
        place: Affine3A,
    ) -> Result<Option<ModelBody>, AppError> {
        // The mesh's shape, shared by its models (made again for a new collider; bodies
        // made from the old one keep it alive until they go).
        let base = match self.model_shapes.get(&mesh) {
            Some((made, shape)) if Arc::ptr_eq(made, collider) => *shape,
            _ => {
                let shape = engine_physics::colliders::build(&mut self.physics, &collider.collider)
                    .map_err(AppError::at("a model's collider"))?;
                if let Some((_, old)) = self
                    .model_shapes
                    .insert(mesh, (Arc::clone(collider), shape))
                {
                    self.physics.destroy_shape(old);
                    let physics = &mut self.physics;
                    self.model_placed.retain(|(m, _), (s, _)| {
                        let keep = *m != mesh;
                        if !keep {
                            physics.destroy_shape(*s);
                        }
                        keep
                    });
                }
                shape
            }
        };
        let (values, sheared) = placement_key(place);
        let placed = match self.model_placed.get(&(mesh, values)) {
            Some(&(shape, sheared)) => {
                let (_, rotation, position) = place.to_scale_rotation_translation();
                Some(engine_physics::colliders::Placed {
                    shape,
                    made: true,
                    position: if sheared {
                        Vec3::from(place.translation)
                    } else {
                        position
                    },
                    rotation: if sheared { Quat::IDENTITY } else { rotation },
                })
            }
            None => engine_physics::colliders::placed(
                &mut self.physics,
                &collider.collider,
                base,
                place,
                TOLERANCE,
            )
            .map_err(AppError::at("placing a model's collider"))?,
        };
        let Some(placed) = placed else {
            // A zero scale: no body.
            return Ok(None);
        };
        if placed.made {
            self.model_placed
                .entry((mesh, values))
                .or_insert((placed.shape, sheared));
        }
        let motion = match collider.motion {
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
        if let Some(name) = &collider.layer {
            desc.layer = self.settings.layer(name).ok_or_else(|| {
                AppError::message(
                    "a model's body",
                    format!(
                        "its collision layer `{name}` is not in the world's layer names {:?} \
                         (PhysicsSettings::layer_names)",
                        self.settings.layer_names
                    ),
                )
            })?;
        }
        desc.rotation = placed.rotation;
        desc.friction = collider.friction;
        desc.restitution = collider.restitution;
        desc.mass = collider.mass;
        desc.sensor = collider.sensor;
        desc.user_data = entity_user_data(e);
        if collider.gravity_factor != 1.0 {
            desc.gravity = GravityMode::Scaled(collider.gravity_factor);
        }
        let body = self
            .physics
            .create_body(&desc)
            .map_err(AppError::at("a model's body"))?;
        let moving = collider.motion != Motion::Static;
        if moving {
            // A root where it is in the world, its scale kept: the body moves it.
            let (scale, _, _) = place.to_scale_rotation_translation();
            let _ = world.set_parent(e, None);
            world.insert(e, RigidBody::new(body));
            world.insert(e, PhysicsPose::at(placed.position, placed.rotation));
            world.insert(
                e,
                Transform::from_translation(placed.position)
                    .with_rotation(placed.rotation)
                    .with_scale(scale),
            );
            if collider.motion == Motion::Kinematic {
                world.insert(e, KinematicTarget::new(placed.position, placed.rotation));
            }
            if self.sync.is_none() {
                self.sync = Some(PhysicsSync::new(world));
            }
            if self.saved.is_none() {
                self.saved = Some(engine_ecs::QueryState::new(world));
            }
        }
        Ok(Some(ModelBody {
            mesh,
            collider: Arc::clone(collider),
            placed: place.to_cols_array(),
            body,
            moving,
        }))
    }

    /// Model `e`'s body gone, and what it gave its entity (if the entity is still there).
    fn drop_model_body(&mut self, world: &mut World, e: Entity, old: ModelBody) {
        self.physics.destroy_body(old.body);
        if old.moving
            && world
                .get::<RigidBody>(e)
                .is_some_and(|r| r.body() == old.body)
        {
            world.remove::<RigidBody>(e);
            world.remove::<PhysicsPose>(e);
            world.remove::<KinematicTarget>(e);
        }
    }

    /// The bodies of the scene's brushes and models in step ([`Self::sync_blockout`],
    /// [`Self::sync_models`]): one call from `Game::before_ticks`. Returns how many bodies
    /// it made.
    pub fn sync_scene(&mut self, scene: &mut crate::SceneParts<'_>) -> Result<usize, AppError> {
        let groups = self.sync_blockout(scene.world, scene.blockout)?;
        let models = self.sync_models(scene.world, scene.assets)?;
        Ok(groups + models)
    }

    /// The body of model `entity`, if it has one ([`Self::sync_models`]).
    pub fn model_body(&self, entity: Entity) -> Option<BodyId> {
        self.models.get(&entity).map(|b| b.body)
    }

    fn drop_blockout_body(&mut self, old: BlockoutBody) {
        self.physics.destroy_body(old.body);
        for s in old.shapes {
            self.physics.destroy_shape(s);
        }
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

    /// Adds the physics' own state to a save (ADR 0104): gravity, and the velocities of
    /// the moving bodies whose entities have ids. Call it from `Game::before_save`.
    pub fn save(&mut self, world: &World, image: &mut engine_save::SaveImage) {
        let mut state = PhysicsState {
            gravity: self.physics.gravity(),
            bodies: Vec::new(),
        };
        let physics = &self.physics;
        let Some(bodies) = self.saved.as_mut() else {
            image.set_state(STATE, &state);
            return;
        };
        bodies.for_each_readonly(world, |(body, id): (&RigidBody, &SceneId)| {
            if let Some((linear, angular)) = physics.velocity(body.body()) {
                let v = id.as_u128();
                state.bodies.push(BodyState {
                    id_high: (v >> 64) as u64,
                    id_low: v as u64,
                    linear,
                    angular,
                });
            }
        });
        image.set_state(STATE, &state);
    }

    /// Takes the physics back from a save (ADR 0104): gravity; every moving body to where
    /// its entity's `Transform` was saved, at its saved velocities, its interpolation
    /// starting there. Call it from `Game::after_load`.
    ///
    /// # Errors
    ///
    /// If the save's physics state is damaged.
    pub fn restore(
        &mut self,
        world: &mut World,
        save: &engine_save::LoadedSave,
    ) -> Result<(), engine_save::SaveError> {
        let Some(state) = save.state::<PhysicsState>(STATE)? else {
            return Ok(());
        };
        self.physics.set_gravity(state.gravity);
        let velocities: HashMap<u128, (Vec3, Vec3)> = state
            .bodies
            .iter()
            .map(|b| {
                (
                    (u128::from(b.id_high) << 64) | u128::from(b.id_low),
                    (b.linear, b.angular),
                )
            })
            .collect();
        let physics = &mut self.physics;
        let mut bodies = engine_ecs::QueryState::<(
            &RigidBody,
            &Transform,
            &mut PhysicsPose,
            Option<&SceneId>,
        )>::new(world);
        bodies.for_each(world, |(body, transform, pose, id)| {
            let (position, rotation) = (transform.translation, transform.rotation);
            physics.set_transform(body.body(), position, rotation);
            *pose = PhysicsPose::at(position, rotation);
            let (linear, angular) = id
                .and_then(|id| velocities.get(&id.as_u128()).copied())
                .unwrap_or((Vec3::ZERO, Vec3::ZERO));
            physics.set_velocity(body.body(), linear, angular);
        });
        Ok(())
    }
}

/// The name of the physics' state in a save.
const STATE: &str = "physics";

/// The physics in a save: what the world does not hold.
#[derive(Debug, Default, Clone, PartialEq, Reflect)]
#[reflect(name = "PhysicsState")]
struct PhysicsState {
    gravity: Vec3,
    bodies: Vec<BodyState>,
}

/// A moving body's velocities, by its entity's id.
#[derive(Debug, Default, Clone, Copy, PartialEq, Reflect)]
#[reflect(name = "BodyState")]
struct BodyState {
    id_high: u64,
    id_low: u64,
    linear: Vec3,
    angular: Vec3,
}

/// A moving body's entity id: its first instance's, mixed so the two differ.
fn body_id(instance: SceneId) -> SceneId {
    let v = instance.as_u128();
    SceneId::from_u128(v.rotate_left(64) ^ 0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C834)
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
