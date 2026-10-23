//! Parent-child relationships as plain data.
//!
//! A child's [`Parent`] holds its parent and its neighbours in the parent's list of
//! children; the parent's [`Children`] holds the first and last child and the count. The
//! list is intrusive, so both components are plain data: they are snapshotted, restored
//! and hashed like any other component, never allocate, and keep children in insertion
//! order (deterministic). Attaching, detaching and reordering are O(1); a cycle check
//! walks up from the new parent, O(depth).
//!
//! Neither component fragments archetypes by parent: every child with a parent shares
//! the archetypes its other components select (unlike parent-keyed relationships).

use crate::entity::Entity;
use crate::world::World;
use crate::{bytemuck, reflect_component};
use engine_reflect::Reflect;

/// The entity's parent and its place among the parent's children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct Parent {
    parent: Entity,
    #[reflect(hidden)]
    pub(crate) prev: Entity,
    #[reflect(hidden)]
    pub(crate) next: Entity,
}

impl Parent {
    /// The parent entity.
    pub fn get(&self) -> Entity {
        self.parent
    }

    /// The sibling before this entity among the parent's children, or `Entity::NULL` for
    /// the first: what keeps the children's order through a save (ADR 0104).
    pub fn previous_sibling(&self) -> Entity {
        self.prev
    }

    /// The sibling after this entity among the parent's children, or `Entity::NULL` for
    /// the last: a step of a loop over them that keeps no iterator (scripts' `children()`).
    pub fn next_sibling(&self) -> Entity {
        self.next
    }
}

/// The entity's children: the ends of the list and how many there are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
#[reflect(hidden)]
pub struct Children {
    #[reflect(hidden)]
    pub(crate) first: Entity,
    #[reflect(hidden)]
    pub(crate) last: Entity,
    pub(crate) count: u32,
    #[reflect(hidden, skip)]
    _padding: u32,
}

impl Children {
    /// Number of children.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether there are none (the component is removed when the last child leaves).
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

reflect_component!(pod Parent, Children);

/// Errors from changing the hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HierarchyError {
    /// An entity is not alive.
    NotAlive(Entity),
    /// The new parent is the child itself or one of its descendants.
    Cycle,
}

impl core::fmt::Display for HierarchyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAlive(e) => write!(f, "entity {e:?} is not alive"),
            Self::Cycle => write!(f, "the new parent is the child or one of its descendants"),
        }
    }
}

impl std::error::Error for HierarchyError {}

/// Iterator over an entity's children in order.
#[derive(Debug, Clone)]
pub struct ChildIter<'w> {
    world: &'w World,
    next: Entity,
    remaining: u32,
}

impl Iterator for ChildIter<'_> {
    type Item = Entity;

    fn next(&mut self) -> Option<Entity> {
        if self.remaining == 0 {
            return None;
        }
        let current = self.next;
        self.remaining -= 1;
        self.next = self
            .world
            .get::<Parent>(current)
            .map_or(Entity::NULL, |p| p.next);
        Some(current)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining as usize, Some(self.remaining as usize))
    }
}

impl ExactSizeIterator for ChildIter<'_> {}

impl World {
    /// The parent of `entity`, if it has one.
    pub fn parent(&self, entity: Entity) -> Option<Entity> {
        self.get::<Parent>(entity).map(|p| p.parent)
    }

    /// The children of `entity`, in the order they were attached.
    pub fn children(&self, entity: Entity) -> ChildIter<'_> {
        let (next, remaining) = self
            .get::<Children>(entity)
            .map_or((Entity::NULL, 0), |c| (c.first, c.count));
        ChildIter {
            world: self,
            next,
            remaining,
        }
    }

    /// Makes `parent` the parent of `child` (appended after its other children), or
    /// detaches `child` with `None`.
    pub fn set_parent(
        &mut self,
        child: Entity,
        parent: Option<Entity>,
    ) -> Result<(), HierarchyError> {
        if !self.contains(child) {
            return Err(HierarchyError::NotAlive(child));
        }
        if let Some(p) = parent {
            if !self.contains(p) {
                return Err(HierarchyError::NotAlive(p));
            }
            let mut up = Some(p);
            while let Some(a) = up {
                if a == child {
                    return Err(HierarchyError::Cycle);
                }
                up = self.parent(a);
            }
        }
        if self.parent(child) == parent {
            return Ok(());
        }
        self.detach(child);
        if let Some(p) = parent {
            let children = self.get::<Children>(p).copied();
            let prev = children.map_or(Entity::NULL, |c| c.last);
            self.insert(
                child,
                Parent {
                    parent: p,
                    prev,
                    next: Entity::NULL,
                },
            );
            match children {
                Some(mut c) => {
                    if let Some(last) = self.get_mut::<Parent>(prev) {
                        last.next = child;
                    }
                    c.last = child;
                    c.count += 1;
                    self.insert(p, c);
                }
                None => {
                    self.insert(
                        p,
                        Children {
                            first: child,
                            last: child,
                            count: 1,
                            _padding: 0,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Makes `parent` the parent of `child` at position `index` among its children
    /// (clamped to the end), or detaches `child` with `None`. Undo uses it to put a
    /// child back where it was.
    pub fn set_parent_at(
        &mut self,
        child: Entity,
        parent: Option<Entity>,
        index: usize,
    ) -> Result<(), HierarchyError> {
        self.set_parent(child, None)?;
        self.set_parent(child, parent)?;
        let Some(p) = parent else {
            return Ok(());
        };
        let count = self.get::<Children>(p).map_or(0, |c| c.count as usize);
        if index.saturating_add(1) >= count {
            return Ok(()); // already last
        }
        // `child` was appended; move it before the child now at `index`.
        let before = self.children(p).nth(index).unwrap_or(Entity::NULL);
        let link = self.get::<Parent>(child).copied().unwrap_or(Parent {
            parent: p,
            prev: Entity::NULL,
            next: Entity::NULL,
        });
        // Unlink from the end.
        if let Some(prev) = self.get_mut::<Parent>(link.prev) {
            prev.next = Entity::NULL;
        }
        let mut c = self.get::<Children>(p).copied().unwrap_or(Children {
            first: child,
            last: child,
            count: 1,
            _padding: 0,
        });
        c.last = link.prev;
        // Link before `before`.
        let before_prev = self.get::<Parent>(before).map_or(Entity::NULL, |b| b.prev);
        if let Some(bp) = self.get_mut::<Parent>(before_prev) {
            bp.next = child;
        } else {
            c.first = child;
        }
        if let Some(b) = self.get_mut::<Parent>(before) {
            b.prev = child;
        }
        if let Some(me) = self.get_mut::<Parent>(child) {
            me.prev = before_prev;
            me.next = before;
        }
        self.insert(p, c);
        Ok(())
    }

    /// Position of `child` among its parent's children.
    pub fn child_index(&self, child: Entity) -> Option<usize> {
        let p = self.parent(child)?;
        self.children(p).position(|c| c == child)
    }

    /// Unlinks `child` from its parent's list and removes its `Parent`.
    fn detach(&mut self, child: Entity) {
        let Some(link) = self.remove::<Parent>(child) else {
            return;
        };
        self.unlink(child, link);
    }

    /// Takes `child` (whose link was `link`) out of its parent's list of children. Its
    /// own `Parent` is left to the caller.
    pub(crate) fn unlink(&mut self, child: Entity, link: Parent) {
        if let Some(prev) = self.get_mut::<Parent>(link.prev) {
            prev.next = link.next;
        }
        if let Some(next) = self.get_mut::<Parent>(link.next) {
            next.prev = link.prev;
        }
        if let Some(mut c) = self.get::<Children>(link.parent).copied() {
            c.count -= 1;
            if c.count == 0 {
                self.remove::<Children>(link.parent);
            } else {
                if c.first == child {
                    c.first = link.next;
                }
                if c.last == child {
                    c.last = link.prev;
                }
                self.insert(link.parent, c);
            }
        }
    }

    /// Despawns `entity` and all its descendants (children before parents are detached,
    /// deepest first). Returns how many entities were despawned.
    pub fn despawn_recursive(&mut self, entity: Entity) -> usize {
        if !self.contains(entity) {
            return 0;
        }
        self.detach(entity);
        let mut count = 0;
        // Depth-first, without recursion on the call stack.
        let mut stack = vec![entity];
        while let Some(e) = stack.pop() {
            stack.extend(self.children(e));
            if self.despawn(e) {
                count += 1;
            }
        }
        count
    }
}
