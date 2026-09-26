//! Deferred structural changes.
//!
//! Code that iterates the world (possibly on several threads) records spawns, inserts,
//! removals and despawns into a [`Commands`] buffer; [`World::apply`] performs them later
//! in recording order. Buffers from parallel jobs are applied in job order, so the result
//! does not depend on which job finished first. Buffers keep their memory between frames.

use crate::bundle::Bundle;
use crate::component::Component;
use crate::entity::Entity;
use crate::world::World;

/// An entity spawned by a [`Commands`] buffer that is not applied yet. Valid only in the
/// buffer that returned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingEntity(u32);

/// The entity a command acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// An existing entity.
    Entity(Entity),
    /// An entity spawned earlier in the same buffer.
    Pending(PendingEntity),
}

impl From<Entity> for Target {
    fn from(e: Entity) -> Self {
        Self::Entity(e)
    }
}

impl From<PendingEntity> for Target {
    fn from(p: PendingEntity) -> Self {
        Self::Pending(p)
    }
}

type ApplyFn = unsafe fn(&mut World, Option<Entity>, *const u8) -> Option<Entity>;
type DiscardFn = unsafe fn(*const u8);

#[derive(Clone, Copy)]
struct Op {
    target: Option<Target>,
    offset: u32,
    apply: ApplyFn,
    discard: Option<DiscardFn>,
}

unsafe fn apply_spawn<B: Bundle>(
    world: &mut World,
    _: Option<Entity>,
    src: *const u8,
) -> Option<Entity> {
    // SAFETY: `src` holds a `B` written by `Commands::spawn`, read exactly once.
    let bundle = unsafe { src.cast::<B>().read_unaligned() };
    Some(world.spawn(bundle))
}

unsafe fn apply_insert<T: Component>(
    world: &mut World,
    target: Option<Entity>,
    src: *const u8,
) -> Option<Entity> {
    // SAFETY: `src` holds a `T` written by `Commands::insert`, read exactly once.
    let value = unsafe { src.cast::<T>().read_unaligned() };
    if let Some(entity) = target {
        world.insert(entity, value);
    }
    None
}

unsafe fn apply_remove<T: Component>(
    world: &mut World,
    target: Option<Entity>,
    _: *const u8,
) -> Option<Entity> {
    if let Some(entity) = target {
        world.remove::<T>(entity);
    }
    None
}

unsafe fn apply_despawn(world: &mut World, target: Option<Entity>, _: *const u8) -> Option<Entity> {
    if let Some(entity) = target {
        world.despawn(entity);
    }
    None
}

unsafe fn discard<T>(src: *const u8) {
    // SAFETY: `src` holds an unconsumed `T`.
    drop(unsafe { src.cast::<T>().read_unaligned() });
}

/// A buffer of deferred structural changes.
///
/// ```
/// use engine_ecs::{bytemuck, pod_component, Commands, World};
///
/// #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
/// #[repr(C)]
/// struct Lifetime(f32);
/// pod_component!(Lifetime);
///
/// let mut world = World::new();
/// let mut commands = Commands::new();
/// let bullet = commands.spawn((Lifetime(2.0),));
/// commands.insert(bullet, Lifetime(1.0));
/// world.apply(&mut commands);
/// assert_eq!(world.len(), 1);
/// ```
#[derive(Default)]
pub struct Commands {
    ops: Vec<Op>,
    data: Vec<u8>,
    /// Operations already consumed by `apply` (for panic safety).
    consumed: usize,
    spawned: u32,
    resolved: Vec<Entity>,
}

// SAFETY: the buffer owns values of `Component`/`Bundle` types, which are `Send + Sync`.
unsafe impl Send for Commands {}
// SAFETY: shared access exposes nothing.
unsafe impl Sync for Commands {}

impl core::fmt::Debug for Commands {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Commands")
            .field("ops", &self.ops.len())
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl Commands {
    /// An empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of recorded commands.
    pub fn len(&self) -> usize {
        self.ops.len() - self.consumed
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn push_value<T>(&mut self, value: T) -> u32 {
        let offset = self.data.len();
        let size = core::mem::size_of::<T>();
        self.data.reserve(size);
        // SAFETY: `size` bytes are reserved past the length; unaligned write; the length
        // then covers exactly the initialized bytes.
        unsafe {
            self.data
                .as_mut_ptr()
                .add(offset)
                .cast::<T>()
                .write_unaligned(value);
            self.data.set_len(offset + size);
        }
        offset as u32
    }

    /// Records a spawn; the returned handle targets it in later commands of this buffer.
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> PendingEntity {
        let offset = self.push_value(bundle);
        self.ops.push(Op {
            target: None,
            offset,
            apply: apply_spawn::<B>,
            discard: core::mem::needs_drop::<B>().then_some(discard::<B> as DiscardFn),
        });
        let pending = PendingEntity(self.spawned);
        self.spawned += 1;
        pending
    }

    /// Records adding or replacing a component.
    pub fn insert<T: Component>(&mut self, target: impl Into<Target>, value: T) {
        let offset = self.push_value(value);
        self.ops.push(Op {
            target: Some(target.into()),
            offset,
            apply: apply_insert::<T>,
            discard: core::mem::needs_drop::<T>().then_some(discard::<T> as DiscardFn),
        });
    }

    /// Records removing (and dropping) a component.
    pub fn remove<T: Component>(&mut self, target: impl Into<Target>) {
        self.ops.push(Op {
            target: Some(target.into()),
            offset: 0,
            apply: apply_remove::<T>,
            discard: None,
        });
    }

    /// Records a despawn.
    pub fn despawn(&mut self, target: impl Into<Target>) {
        self.ops.push(Op {
            target: Some(target.into()),
            offset: 0,
            apply: apply_despawn,
            discard: None,
        });
    }

    fn reset(&mut self) {
        self.ops.clear();
        self.data.clear();
        self.consumed = 0;
        self.spawned = 0;
        self.resolved.clear();
    }

    /// Drops everything recorded without applying it.
    pub fn clear(&mut self) {
        self.discard_rest();
        self.reset();
    }

    fn discard_rest(&mut self) {
        for i in self.consumed..self.ops.len() {
            let op = self.ops[i];
            self.consumed = i + 1;
            if let Some(discard) = op.discard {
                // SAFETY: the value at `offset` was written by the matching method and
                // not consumed (`consumed` advances before any use).
                unsafe { discard(self.data.as_ptr().add(op.offset as usize)) };
            }
        }
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        self.discard_rest();
    }
}

impl World {
    /// Performs the buffer's commands in recording order and empties it (keeping its
    /// memory). Commands on entities that are no longer alive do nothing.
    pub fn apply(&mut self, commands: &mut Commands) {
        commands.resolved.reserve(commands.spawned as usize);
        for i in commands.consumed..commands.ops.len() {
            let op = commands.ops[i];
            commands.consumed = i + 1;
            let target = match op.target {
                None => None,
                Some(Target::Entity(e)) => Some(e),
                Some(Target::Pending(PendingEntity(p))) => {
                    commands.resolved.get(p as usize).copied()
                }
            };
            // SAFETY: the value at `offset` matches `apply`'s type and is consumed once.
            let spawned =
                unsafe { (op.apply)(self, target, commands.data.as_ptr().add(op.offset as usize)) };
            if let Some(entity) = spawned {
                commands.resolved.push(entity);
            }
        }
        commands.reset();
    }
}
