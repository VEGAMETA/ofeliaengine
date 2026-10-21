//! Turning parts of a scene off, as Godot's disabled nodes: [`Disabled`] on an entity turns
//! it and everything below it off. The engine keeps [`Inactive`] on each entity that is off
//! (it, or something above it, is disabled), the state its systems read: extraction draws
//! no mesh of it, the lights' extraction leaves its lights dark, a blockout group leaves its
//! brushes out.
//!
//! [`ActivityPropagation`] brings `Inactive` in step after every tick (before the transforms
//! are propagated and the world extracted), only when something was disabled or enabled or
//! moved in the tree.

use crate::{MeshRenderer, PointLight, SpotLight, Sun};
use engine_ecs::{Changed, Entity, Parent, QueryState, With, World, bytemuck, reflect_component};
use engine_reflect::Reflect;

/// Turns the entity and everything below it off (saved with the scene): not drawn, its
/// lights dark, its brushes out of their group. The editor's eye in the scene tree.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, bytemuck::Pod, bytemuck::Zeroable, Reflect,
)]
#[repr(C)]
pub struct Disabled {}

reflect_component!(pod Disabled);

/// On every entity that is off: [`Disabled`] itself or below a disabled one. Kept by
/// [`ActivityPropagation`]; not saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Inactive;

engine_ecs::component!(Inactive);

/// How many times the entities that are off changed: a world resource systems that keep
/// their own state compare (`engine_app`'s blockout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ActivityEpoch(pub u64);

/// Keeps [`Inactive`] in step with [`Disabled`] and the hierarchy.
pub struct ActivityPropagation {
    disabled: QueryState<Entity, Changed<Disabled>>,
    reparented: QueryState<Entity, Changed<Parent>>,
    all_disabled: QueryState<Entity, With<Disabled>>,
    inactive: QueryState<Entity, With<Inactive>>,
    off: Vec<Entity>,
    now: Vec<Entity>,
    stack: Vec<Entity>,
    first: bool,
}

impl core::fmt::Debug for ActivityPropagation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ActivityPropagation")
            .field("off", &self.off.len())
            .finish_non_exhaustive()
    }
}

impl ActivityPropagation {
    /// State for `world`; starts the world's removal log of [`Disabled`].
    pub fn new(world: &mut World) -> Self {
        world.track_removals::<Disabled>();
        Self {
            disabled: QueryState::new(world),
            reparented: QueryState::new(world),
            all_disabled: QueryState::new(world),
            inactive: QueryState::new(world),
            off: Vec::new(),
            now: Vec::new(),
            stack: Vec::new(),
            first: true,
        }
    }

    /// Brings [`Inactive`] in step; whether any entity turned on or off. Entities that turn
    /// have their renderers and lights marked changed, so extraction sees them.
    pub fn run(&mut self, world: &mut World) -> bool {
        engine_profiler::zone!("runtime::ActivityPropagation::run");
        // Every query runs, so that each forgets what it reported.
        let mut dirty = self.first;
        self.first = false;
        dirty |= self.disabled.iter_readonly(world).count() > 0;
        dirty |= self.reparented.iter_readonly(world).count() > 0;
        let mut removed = false;
        world.drain_removed::<Disabled>(|_| removed = true);
        if !(dirty || removed) {
            return false;
        }
        // What should be off: every disabled entity and what is below it.
        self.off.clear();
        self.stack.clear();
        self.stack.extend(self.all_disabled.iter_readonly(world));
        while let Some(e) = self.stack.pop() {
            self.off.push(e);
            self.stack.extend(world.children(e));
        }
        self.off.sort_unstable_by_key(|e| e.to_bits());
        self.off.dedup();
        // What is off.
        self.now.clear();
        self.now.extend(self.inactive.iter_readonly(world));
        self.now.sort_unstable_by_key(|e| e.to_bits());
        let mut changed = false;
        for i in 0..self.now.len() {
            let e = self.now[i];
            if self
                .off
                .binary_search_by_key(&e.to_bits(), |x| x.to_bits())
                .is_err()
            {
                world.remove::<Inactive>(e);
                touch(world, e);
                changed = true;
            }
        }
        for i in 0..self.off.len() {
            let e = self.off[i];
            if self
                .now
                .binary_search_by_key(&e.to_bits(), |x| x.to_bits())
                .is_err()
            {
                world.insert(e, Inactive);
                touch(world, e);
                changed = true;
            }
        }
        if changed {
            match world.resource_mut::<ActivityEpoch>() {
                Some(epoch) => epoch.0 += 1,
                None => world.insert_resource(ActivityEpoch(1)),
            }
        }
        changed
    }
}

/// Marks `e`'s renderer and lights changed, so their extraction looks at it again.
fn touch(world: &mut World, e: Entity) {
    let _ = world.get_mut::<MeshRenderer>(e);
    let _ = world.get_mut::<PointLight>(e);
    let _ = world.get_mut::<SpotLight>(e);
    let _ = world.get_mut::<Sun>(e);
}

/// Whether `e` is off ([`Inactive`]).
pub fn is_inactive(world: &World, e: Entity) -> bool {
    world.get::<Inactive>(e).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabling_turns_off_what_is_below_and_enabling_turns_it_back_on() {
        let mut world = World::new();
        let root = world.spawn((Disabled {},));
        let child = world.spawn(());
        let grandchild = world.spawn(());
        let other = world.spawn(());
        world
            .set_parent(child, Some(root))
            .unwrap_or_else(|_| panic!());
        world
            .set_parent(grandchild, Some(child))
            .unwrap_or_else(|_| panic!());
        let mut activity = ActivityPropagation::new(&mut world);
        assert!(activity.run(&mut world));
        for e in [root, child, grandchild] {
            assert!(is_inactive(&world, e), "{e:?}");
        }
        assert!(!is_inactive(&world, other));
        assert_eq!(world.resource::<ActivityEpoch>().map(|e| e.0), Some(1));
        // Nothing changed: nothing to do.
        assert!(!activity.run(&mut world));
        // A child moved out from under the disabled one turns on.
        world.set_parent(child, None).unwrap_or_else(|_| panic!());
        assert!(activity.run(&mut world));
        assert!(is_inactive(&world, root));
        assert!(!is_inactive(&world, child) && !is_inactive(&world, grandchild));
        // Enabled again: all on.
        world.remove::<Disabled>(root);
        assert!(activity.run(&mut world));
        assert!(!is_inactive(&world, root));
        // Disabled below: that subtree only.
        world.insert(child, Disabled {});
        assert!(activity.run(&mut world));
        assert!(is_inactive(&world, child) && is_inactive(&world, grandchild));
        assert!(!is_inactive(&world, root));
        assert_eq!(world.resource::<ActivityEpoch>().map(|e| e.0), Some(4));
    }
}
