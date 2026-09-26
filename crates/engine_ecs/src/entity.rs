//! Entity handles and their allocator.

/// A handle to an entity: an index and a generation that changes when the index is
/// reused, so stale handles are detected.
///
/// Plain data: components may hold entities and still be snapshotted byte-wise.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Entity {
    index: u32,
    generation: u32,
}

impl Entity {
    /// A handle that never refers to a live entity (for "none" in plain-data components).
    pub const NULL: Entity = Entity {
        index: u32::MAX,
        generation: u32::MAX,
    };

    /// Slot index (dense, reused after despawn).
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Generation of the slot when this handle was made.
    pub const fn generation(self) -> u32 {
        self.generation
    }

    /// Packs the handle into 64 bits (generation high, index low).
    pub const fn to_bits(self) -> u64 {
        ((self.generation as u64) << 32) | self.index as u64
    }

    /// Inverse of [`Entity::to_bits`].
    pub const fn from_bits(bits: u64) -> Self {
        Self {
            index: bits as u32,
            generation: (bits >> 32) as u32,
        }
    }
}

impl core::fmt::Debug for Entity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}v{}", self.index, self.generation)
    }
}

/// Where an entity's row lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Location {
    pub(crate) archetype: u32,
    pub(crate) chunk: u32,
    pub(crate) row: u32,
}

pub(crate) const DEAD: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) generation: u32,
    pub(crate) location: Location,
}

/// Slot table. Allocation is deterministic: the most recently freed slot is reused
/// first, otherwise the next new index.
#[derive(Default, Clone)]
pub(crate) struct Entities {
    pub(crate) metas: Vec<Meta>,
    pub(crate) free: Vec<u32>,
    pub(crate) alive: u32,
}

impl Entities {
    #[inline]
    pub(crate) fn alloc(&mut self, location: Location) -> Entity {
        self.alive += 1;
        if let Some(index) = self.free.pop() {
            let meta = &mut self.metas[index as usize];
            meta.location = location;
            Entity {
                index,
                generation: meta.generation,
            }
        } else {
            let index = self.metas.len() as u32;
            self.metas.push(Meta {
                generation: 0,
                location,
            });
            Entity {
                index,
                generation: 0,
            }
        }
    }

    #[inline]
    pub(crate) fn free(&mut self, entity: Entity) {
        let meta = &mut self.metas[entity.index as usize];
        meta.generation = meta.generation.wrapping_add(1);
        meta.location.archetype = DEAD;
        self.free.push(entity.index);
        self.alive -= 1;
    }

    #[inline]
    pub(crate) fn location(&self, entity: Entity) -> Option<Location> {
        let meta = self.metas.get(entity.index as usize)?;
        (meta.generation == entity.generation && meta.location.archetype != DEAD)
            .then_some(meta.location)
    }

    #[inline]
    pub(crate) fn set_location(&mut self, index: u32, location: Location) {
        self.metas[index as usize].location = location;
    }
}

impl engine_reflect::Reflect for Entity {
    const TYPE_INFO: &'static engine_reflect::TypeInfo = &engine_reflect::TypeInfo {
        name: "Entity",
        size: core::mem::size_of::<Entity>(),
        align: core::mem::align_of::<Entity>(),
        kind: engine_reflect::TypeKind::Struct(&[
            engine_reflect::Field {
                name: "index",
                offset: core::mem::offset_of!(Entity, index),
                ty: <u32 as engine_reflect::Reflect>::TYPE_INFO,
                attrs: engine_reflect::Attrs::NONE,
            },
            engine_reflect::Field {
                name: "generation",
                offset: core::mem::offset_of!(Entity, generation),
                ty: <u32 as engine_reflect::Reflect>::TYPE_INFO,
                attrs: engine_reflect::Attrs::NONE,
            },
        ]),
        attrs: engine_reflect::Attrs::NONE,
    };
}
