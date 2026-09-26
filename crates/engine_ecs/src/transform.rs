//! Local and world transforms, and their propagation down the hierarchy.
//!
//! [`Transform`] is what game code and the editor write: translation, rotation and scale
//! relative to the parent (or the world, for a root). [`GlobalTransform`] is derived:
//! [`TransformPropagation::run`] recomputes it from the roots down, over [`Parent`] /
//! [`Children`]. Entities need both components to take part; a child without them
//! breaks the chain for its own subtree.
//!
//! The pass skips all work when no `Transform`, `Parent` or `Children` was written since
//! its previous run (a static scene costs three chunk-version scans). Otherwise it
//! recomputes every world transform: the cost is linear in the entities with transforms
//! (ADR 0027 has numbers). Updating only the changed subtrees is a later optimization.

use crate::entity::Entity;
use crate::hierarchy::{Children, Parent};
use crate::query::{Changed, QueryState, With, Without};
use crate::world::World;
use crate::{bytemuck, pod_component, reflect_component};
use engine_core::math::{Affine3A, Mat4, Quat, Vec3};
use engine_reflect::Reflect;

/// Position, rotation and scale relative to the parent (or the world for a root).
///
/// Scale is applied first, then rotation, then translation. Non-uniform scale on a parent
/// shears rotated children, as in every engine that composes affine matrices.
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct Transform {
    /// Rotation (unit quaternion).
    pub rotation: Quat,
    /// Translation.
    pub translation: Vec3,
    /// Scale per axis.
    pub scale: Vec3,
    #[reflect(hidden, skip)]
    _padding: [u32; 2],
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    /// No translation, no rotation, scale 1.
    pub const IDENTITY: Self = Self {
        rotation: Quat::IDENTITY,
        translation: Vec3::ZERO,
        scale: Vec3::ONE,
        _padding: [0; 2],
    };

    /// A translation.
    pub const fn from_translation(translation: Vec3) -> Self {
        Self {
            translation,
            ..Self::IDENTITY
        }
    }

    /// A translation from coordinates.
    pub const fn from_xyz(x: f32, y: f32, z: f32) -> Self {
        Self::from_translation(Vec3::new(x, y, z))
    }

    /// A rotation.
    pub const fn from_rotation(rotation: Quat) -> Self {
        Self {
            rotation,
            ..Self::IDENTITY
        }
    }

    /// A scale.
    pub const fn from_scale(scale: Vec3) -> Self {
        Self {
            scale,
            ..Self::IDENTITY
        }
    }

    /// This transform with another rotation.
    pub const fn with_rotation(self, rotation: Quat) -> Self {
        Self { rotation, ..self }
    }

    /// This transform with another scale.
    pub const fn with_scale(self, scale: Vec3) -> Self {
        Self { scale, ..self }
    }

    /// The transform as an affine matrix.
    #[inline]
    pub fn to_affine(&self) -> Affine3A {
        Affine3A::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
}

reflect_component!(pod Transform);

/// World-space transform, written by [`TransformPropagation`]. Read it for rendering,
/// physics sync and audio; write [`Transform`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct GlobalTransform(pub Affine3A);

impl GlobalTransform {
    /// The identity.
    pub const IDENTITY: Self = Self(Affine3A::IDENTITY);

    /// World-space position.
    pub fn translation(&self) -> Vec3 {
        self.0.translation.into()
    }

    /// As a 4×4 matrix (for GPU upload).
    pub fn to_mat4(&self) -> Mat4 {
        Mat4::from(self.0)
    }
}

pod_component!(GlobalTransform);

/// Roots of hierarchies: no parent, at least one child.
type RootsWithChildren = (Without<Parent>, With<Children>);

/// A child in the hierarchy: one entity lookup fetches everything the walk needs.
type Node = (
    &'static Parent,
    &'static Transform,
    &'static mut GlobalTransform,
    Option<&'static Children>,
);

/// Recomputes [`GlobalTransform`] from [`Transform`] down the hierarchy.
///
/// Create it once per world and run it after game logic, before anything reads world
/// transforms. It needs `&mut World`, so in a [`Schedule`](crate::Schedule) it is an
/// exclusive step:
///
/// ```
/// # use engine_ecs::*;
/// let mut world = World::new();
/// let mut transforms = TransformPropagation::new(&mut world);
/// let mut schedule = Schedule::new();
/// schedule.add_exclusive("transforms", move |w| {
///     transforms.run(w);
/// });
/// let car = world.spawn((Transform::from_xyz(10.0, 0.0, 0.0), GlobalTransform::IDENTITY));
/// let wheel = world.spawn((Transform::from_xyz(1.0, 0.0, 0.0), GlobalTransform::IDENTITY));
/// world.set_parent(wheel, Some(car)).unwrap();
/// schedule.run(&mut world, None);
/// assert_eq!(world.get::<GlobalTransform>(wheel).unwrap().translation().x, 11.0);
/// ```
pub struct TransformPropagation {
    roots: QueryState<(&'static Transform, &'static mut GlobalTransform), Without<Parent>>,
    parents: QueryState<(&'static GlobalTransform, &'static Children), RootsWithChildren>,
    nodes: QueryState<Node>,
    changed_local: QueryState<Entity, Changed<Transform>>,
    changed_parent: QueryState<Entity, Changed<Parent>>,
    changed_children: QueryState<Entity, Changed<Children>>,
    /// Child lists still to visit (first child, count) with their parent's world
    /// transform.
    stack: Vec<(Entity, u32, Affine3A)>,
}

impl TransformPropagation {
    /// Creates the pass's queries in `world`.
    pub fn new(world: &mut World) -> Self {
        Self {
            roots: QueryState::new(world),
            parents: QueryState::new(world),
            nodes: QueryState::new(world),
            changed_local: QueryState::new(world),
            changed_parent: QueryState::new(world),
            changed_children: QueryState::new(world),
            stack: Vec::new(),
        }
    }

    /// Updates every [`GlobalTransform`] if anything in the hierarchy or any local
    /// transform changed since the previous run. Returns whether it did.
    pub fn run(&mut self, world: &mut World) -> bool {
        engine_profiler::zone!("ecs::TransformPropagation::run");
        // Evaluate all three so each query's change tick advances.
        let local = self.changed_local.iter(world).next().is_some();
        let parent = self.changed_parent.iter(world).next().is_some();
        let children = self.changed_children.iter(world).next().is_some();
        if !(local || parent || children) {
            return false;
        }
        self.roots.for_each_chunk(world, |(locals, globals)| {
            for (l, g) in locals.iter().zip(globals.iter_mut()) {
                g.0 = l.to_affine();
            }
        });
        self.stack.clear();
        let stack = &mut self.stack;
        self.parents
            .for_each(world, |(g, c)| stack.push((c.first, c.count, g.0)));
        while let Some((first, count, global)) = self.stack.pop() {
            let mut child = first;
            for _ in 0..count {
                let next = match self.nodes.get(world, child) {
                    Some((parent, local, g, children)) => {
                        let child_global = global * local.to_affine();
                        g.0 = child_global;
                        if let Some(c) = children {
                            self.stack.push((c.first, c.count, child_global));
                        }
                        parent.next
                    }
                    // A child without transforms: its subtree is skipped.
                    None => match world.get::<Parent>(child) {
                        Some(p) => p.next,
                        None => break,
                    },
                };
                child = next;
            }
        }
        true
    }
}
