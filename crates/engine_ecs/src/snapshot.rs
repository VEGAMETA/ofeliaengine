//! Saving and restoring the whole simulated state, and hashing it.
//!
//! A [`Snapshot`] copies every row of every archetype (entity handles and plain-data
//! components) plus the entity allocator and the simulation tick. Restoring it returns
//! the world to exactly that state: the same entities, rows, allocation order and
//! [`World::state_hash`]. This is the rollback primitive for prediction and lockstep
//! (ADR 0019). Buffers are reused, so saving every tick allocates nothing in steady
//! state.

use crate::entity::Meta;
use crate::world::World;
use engine_core::hash::StateHasher;

#[derive(Default, Clone)]
struct ArchetypeSnapshot {
    len: u32,
    bytes: Vec<u8>,
}

/// A saved world state. Reuse one per slot of a rollback ring.
#[derive(Default, Clone)]
pub struct Snapshot {
    world_id: u64,
    archetypes: Vec<ArchetypeSnapshot>,
    metas: Vec<Meta>,
    free: Vec<u32>,
    alive: u32,
    sim_tick: u64,
    valid: bool,
}

impl core::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Snapshot")
            .field("entities", &self.alive)
            .field("sim_tick", &self.sim_tick)
            .field("bytes", &self.byte_len())
            .finish()
    }
}

impl Snapshot {
    /// An empty snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulation tick the snapshot was taken at.
    pub fn sim_tick(&self) -> u64 {
        self.sim_tick
    }

    /// Bytes of component and entity data held.
    pub fn byte_len(&self) -> usize {
        self.archetypes.iter().map(|a| a.bytes.len()).sum()
    }
}

/// Errors from saving or restoring snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// An entity has a component that is not plain data.
    NotPlainData {
        /// The component's name.
        component: Box<str>,
    },
    /// The snapshot is empty or was taken from another world.
    WrongWorld,
}

impl core::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotPlainData { component } => write!(
                f,
                "component `{component}` is not plain data and cannot be snapshotted"
            ),
            Self::WrongWorld => write!(f, "the snapshot is empty or belongs to another world"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl World {
    /// Saves the simulated state into `snapshot`, reusing its memory.
    ///
    /// Every entity's components must be plain data ([`Pod`](crate::Pod)).
    pub fn save(&self, snapshot: &mut Snapshot) -> Result<(), SnapshotError> {
        for arch in &self.archetypes {
            if !arch.is_empty()
                && let Some(column) = arch.columns.iter().find(|c| !c.pod)
            {
                return Err(SnapshotError::NotPlainData {
                    component: self.components.info(column.id).name.clone(),
                });
            }
        }
        snapshot.world_id = self.id();
        snapshot
            .archetypes
            .resize_with(self.archetypes.len(), ArchetypeSnapshot::default);
        for (arch, saved) in self.archetypes.iter().zip(&mut snapshot.archetypes) {
            saved.len = arch.len;
            saved.bytes.clear();
            for chunk in 0..arch.chunks.len() {
                let rows = arch.chunk_len(chunk);
                // SAFETY: rows below the chunk's length are initialized plain data
                // (entities and `Pod` components have no padding).
                unsafe {
                    saved.bytes.extend_from_slice(core::slice::from_raw_parts(
                        arch.entity_ptr(chunk, 0).cast::<u8>(),
                        rows * core::mem::size_of::<crate::Entity>(),
                    ));
                    for (column, info) in arch.columns.iter().enumerate() {
                        saved.bytes.extend_from_slice(core::slice::from_raw_parts(
                            arch.column_ptr(chunk, column),
                            rows * info.size as usize,
                        ));
                    }
                }
            }
        }
        snapshot.metas.clone_from(&self.entities.metas);
        snapshot.free.clone_from(&self.entities.free);
        snapshot.alive = self.entities.alive;
        snapshot.sim_tick = self.sim_tick;
        snapshot.valid = true;
        Ok(())
    }

    /// Returns the world to the state saved in `snapshot`.
    ///
    /// Archetypes created after the snapshot become empty (their non-plain-data values
    /// are dropped). Every restored chunk counts as changed.
    pub fn restore(&mut self, snapshot: &Snapshot) -> Result<(), SnapshotError> {
        if !snapshot.valid || snapshot.world_id != self.id() {
            return Err(SnapshotError::WrongWorld);
        }
        let tick = self.bump_change_tick();
        for (index, arch) in self.archetypes.iter_mut().enumerate() {
            if !arch.all_pod() {
                // Saving guaranteed these were empty; drop whatever they hold now.
                arch.clear(&mut self.pool);
            }
            let saved = snapshot.archetypes.get(index);
            arch.resize_for_restore(&mut self.pool, saved.map_or(0, |s| s.len), tick);
            let Some(saved) = saved else { continue };
            let mut offset = 0;
            for chunk in 0..arch.chunks.len() {
                let rows = arch.chunk_len(chunk);
                let entity_bytes = rows * core::mem::size_of::<crate::Entity>();
                // SAFETY: the snapshot holds the bytes `save` wrote in this order for an
                // archetype with the same layout (same world); plain data only.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        saved.bytes.as_ptr().add(offset),
                        arch.entity_ptr(chunk, 0).cast::<u8>(),
                        entity_bytes,
                    );
                    offset += entity_bytes;
                    for column in 0..arch.columns.len() {
                        let bytes = rows * arch.columns[column].size as usize;
                        core::ptr::copy_nonoverlapping(
                            saved.bytes.as_ptr().add(offset),
                            arch.column_ptr(chunk, column),
                            bytes,
                        );
                        offset += bytes;
                    }
                }
            }
        }
        self.entities.metas.clone_from(&snapshot.metas);
        self.entities.free.clone_from(&snapshot.free);
        self.entities.alive = snapshot.alive;
        self.sim_tick = snapshot.sim_tick;
        // Entities may have vanished without removal logs: mirrors start over.
        self.resync_epoch += 1;
        Ok(())
    }

    /// Hash of the simulated state: the simulation tick, the entity allocator, and every
    /// entity with its plain-data components, in canonical order (independent of chunk
    /// size and of the order archetypes were created in). Components that are not plain
    /// data are not hashed.
    ///
    /// Equal hashes on two machines mean equal states; compare them to detect desyncs.
    pub fn state_hash(&self) -> u64 {
        let mut h = StateHasher::new();
        h.write_u64(self.sim_tick);
        h.write_u64(u64::from(self.entities.alive));
        h.write_u64(self.entities.metas.len() as u64);
        h.write(bytemuck::cast_slice(&self.entities.free));
        for &index in &self.canonical {
            let arch = &self.archetypes[index as usize];
            if arch.is_empty() {
                continue;
            }
            h.write_u64(arch.key.len() as u64);
            for &key in arch.key.iter() {
                h.write_u64(key);
            }
            h.write_u64(u64::from(arch.len));
            for chunk in 0..arch.chunks.len() {
                let rows = arch.chunk_len(chunk);
                // SAFETY: initialized entity handles.
                let entities =
                    unsafe { core::slice::from_raw_parts(arch.entity_ptr(chunk, 0), rows) };
                h.write(bytemuck::cast_slice(entities));
            }
            for &column in arch.canonical_columns.iter() {
                let info = &arch.columns[column as usize];
                if !info.pod {
                    continue;
                }
                for chunk in 0..arch.chunks.len() {
                    let rows = arch.chunk_len(chunk);
                    // SAFETY: initialized plain data without padding.
                    h.write(unsafe {
                        core::slice::from_raw_parts(
                            arch.column_ptr(chunk, column as usize),
                            rows * info.size as usize,
                        )
                    });
                }
            }
        }
        h.finish()
    }
}
