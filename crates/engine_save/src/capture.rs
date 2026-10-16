//! Taking a save inside a frame (ADR 0104): which entities and values, copied as they are
//! (plain data) or written at once (the rest), so the frame pays for copies only. No
//! entity is looked up: archetypes are walked chunk by chunk, the hierarchy read from
//! the `Parent` column, saved entities numbered in a dense table.

use crate::schema::Node;
use crate::value::write_value;
use engine_ecs::reflect::{Reflect, TypeInfo};
use engine_ecs::{Children, Entity, Parent, ReflectedKind, World, reflected_types};
use engine_scene::SceneId;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// An entity a game does not save: effects, UI, anything made again on load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transient;

// SAFETY: a unit struct: no bytes, every (empty) bit pattern valid.
unsafe impl engine_ecs::bytemuck::Zeroable for Transient {}
// SAFETY: as above; `Copy`.
unsafe impl engine_ecs::bytemuck::Pod for Transient {}
engine_ecs::pod_component!(Transient);

/// Hashes ids that are random already (`SceneId`s): their bits, folded.
#[derive(Default)]
pub(crate) struct IdHasher(u64);

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(w));
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(23) ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A set or map keyed by `SceneId`, hashed by its own random bits.
pub(crate) type IdBuild = BuildHasherDefault<IdHasher>;

/// What [`capture`] takes besides the world.
#[derive(Debug, Default, Clone, Copy)]
pub struct CaptureOptions<'a> {
    /// The ids of the entities the level made when it loaded: those gone from the world
    /// are recorded as destroyed, so loading the level again removes them. `None`: no
    /// level.
    pub level: Option<&'a HashSet<SceneId>>,
}

/// How an entity is known in a save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    /// By its [`SceneId`]: matched to the level's (or an authored scene's) entity on load.
    Stable(SceneId),
    /// By its place in the save: created again on load.
    Local,
}

#[derive(Debug, Clone)]
pub(crate) struct SavedType {
    pub(crate) name: &'static str,
    pub(crate) version: u32,
    pub(crate) node: Node,
    pub(crate) info: &'static TypeInfo,
}

/// A value of a saved type in [`SaveImage::bytes`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct SavedValue {
    pub(crate) ty: u32,
    /// Copied as it lies in memory (plain data: encoded when the save is written), or
    /// already in the binary form.
    pub(crate) raw: bool,
    pub(crate) at: u32,
    pub(crate) len: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct SavedEntity {
    pub(crate) key: Key,
    /// The parent's index, `u32::MAX` for none.
    pub(crate) parent: u32,
    /// The previous sibling's index, `u32::MAX` for the first (or one not saved).
    pub(crate) prev: u32,
    /// Its components in [`SaveImage::components`].
    pub(crate) first: u32,
    pub(crate) count: u32,
    /// Left out by [`SaveImage::exclude`]: not created on load, references to it none.
    pub(crate) excluded: bool,
}

/// Saved entities by their slot: an index and generation per slot, no hashing.
#[derive(Debug, Default, Clone)]
pub(crate) struct EntityIndex(Vec<(u32, u32)>);

impl EntityIndex {
    fn insert(&mut self, e: Entity, i: u32) {
        let slot = e.index() as usize;
        if slot >= self.0.len() {
            self.0.resize(slot + 1, (u32::MAX, u32::MAX));
        }
        self.0[slot] = (e.generation(), i);
    }

    /// The saved entity's index, or `u32::MAX`.
    pub(crate) fn get(&self, e: Entity) -> u32 {
        match self.0.get(e.index() as usize) {
            Some(&(generation, i)) if generation == e.generation() => i,
            _ => u32::MAX,
        }
    }
}

/// A save taken from a world, ready to be written on any thread.
#[derive(Debug, Default, Clone)]
pub struct SaveImage {
    pub(crate) types: Vec<SavedType>,
    pub(crate) entities: Vec<SavedEntity>,
    pub(crate) components: Vec<SavedValue>,
    pub(crate) resources: Vec<SavedValue>,
    pub(crate) states: Vec<(String, SavedValue)>,
    pub(crate) bytes: Vec<u8>,
    /// Each saved entity's index: plain-data values keep live entity references until
    /// they are encoded.
    pub(crate) index: EntityIndex,
    pub(crate) destroyed: Vec<SceneId>,
    pub(crate) sim_tick: u64,
    by_info: HashMap<usize, u32>,
}

impl SaveImage {
    /// Entities saved.
    pub fn entities(&self) -> usize {
        self.entities.len()
    }

    /// Bytes of values held.
    pub fn bytes(&self) -> usize {
        self.bytes.len()
    }

    /// The simulation tick it was taken at.
    pub fn sim_tick(&self) -> u64 {
        self.sim_tick
    }

    fn type_index(&mut self, info: &'static TypeInfo) -> u32 {
        let key = core::ptr::from_ref(info) as usize;
        if let Some(&i) = self.by_info.get(&key) {
            return i;
        }
        let i = self.types.len() as u32;
        self.types.push(SavedType {
            name: info.name,
            version: info.attrs.version,
            node: Node::of(info),
            info,
        });
        self.by_info.insert(key, i);
        i
    }

    /// Writes a value now (it may hold pointers), returning where it went.
    fn encode_now(&mut self, ty: u32, ptr: *const u8) -> SavedValue {
        let at = self.bytes.len();
        let index = &self.index;
        let refs = |e: Entity| index.get(e);
        // SAFETY: the callers pass a live value of the type `ty` was made from.
        unsafe { write_value(&mut self.bytes, self.types[ty as usize].info, ptr, &refs) };
        SavedValue {
            ty,
            raw: false,
            at: at as u32,
            len: (self.bytes.len() - at) as u32,
        }
    }

    /// Adds (or replaces) a named state of the game or an engine subsystem: physics, the
    /// clock, a quest log kept outside the world.
    pub fn set_state<T: Reflect>(&mut self, name: &str, value: &T) {
        let ty = self.type_index(T::TYPE_INFO);
        let saved = self.encode_now(ty, core::ptr::from_ref(value).cast());
        match self.states.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = saved,
            None => self.states.push((name.to_owned(), saved)),
        }
    }

    /// Leaves `entity` out (a game's `before_save` correcting an autosave): its children
    /// are saved as roots, references to it as none.
    pub fn exclude(&mut self, entity: Entity) {
        let i = self.index.get(entity);
        if let Some(e) = self.entities.get_mut(i as usize) {
            e.excluded = true;
        }
    }
}

/// How one archetype is saved: its id and parent columns, and its saved columns
/// (column, type, plain data, size).
struct Plan {
    archetype: usize,
    id: Option<usize>,
    parent: Option<usize>,
    columns: Vec<(usize, u32, bool, usize)>,
}

/// Takes the world's saved state (see the crate documentation): what the frame pays for.
pub fn capture(world: &World, options: &CaptureOptions<'_>) -> SaveImage {
    engine_profiler::zone!("save::capture");
    let mut image = SaveImage {
        sim_tick: world.sim_tick(),
        ..SaveImage::default()
    };
    let components = world.components();
    let (parent_id, children_id) = (
        world.component_id::<Parent>(),
        world.component_id::<Children>(),
    );
    let scene_id = world.component_id::<SceneId>();
    let transient = world.component_id::<Transient>();
    let mut plans: Vec<Plan> = Vec::new();
    let (mut count, mut values, mut bytes) = (0usize, 0usize, 0usize);
    for (a, arch) in world.archetypes().iter().enumerate() {
        if arch.is_empty() || transient.is_some_and(|t| arch.column_index(t).is_some()) {
            continue;
        }
        let mut columns = Vec::new();
        for (c, &cid) in arch.components().iter().enumerate() {
            // The hierarchy is saved as each entity's parent, and restored through it.
            if Some(cid) == parent_id || Some(cid) == children_id {
                continue;
            }
            let Some(info) = components.type_info(cid).filter(|i| !i.attrs.skip) else {
                continue;
            };
            let ty = image.type_index(info);
            columns.push((c, ty, components.is_pod(cid), info.size));
        }
        let id = scene_id.and_then(|s| arch.column_index(s));
        if columns.is_empty() && id.is_none() {
            continue;
        }
        count += arch.len();
        values += arch.len() * columns.len();
        bytes += arch.len() * columns.iter().map(|c| c.3).sum::<usize>();
        plans.push(Plan {
            archetype: a,
            id,
            parent: parent_id.and_then(|p| arch.column_index(p)),
            columns,
        });
    }
    image.entities.reserve(count);
    image.components.reserve(values);
    // Plain data as it lies; the rest takes about as much once written.
    image.bytes.reserve(bytes + bytes / 8);
    // Number the entities first: references are written with their indices.
    let level = options.level;
    let mut present: HashSet<SceneId, IdBuild> = HashSet::with_capacity_and_hasher(
        if level.is_some() { count } else { 0 },
        IdBuild::default(),
    );
    for plan in &plans {
        let arch = &world.archetypes()[plan.archetype];
        for chunk in 0..arch.chunk_count() {
            let ids: Option<&[SceneId]> = plan
                .id
                .and_then(|c| arch.pod_column(chunk, c))
                .map(engine_ecs::bytemuck::cast_slice);
            for (row, &e) in arch.chunk_entities(chunk).iter().enumerate() {
                let key = match ids {
                    Some(ids) => {
                        if level.is_some() {
                            present.insert(ids[row]);
                        }
                        Key::Stable(ids[row])
                    }
                    None => Key::Local,
                };
                image.index.insert(e, image.entities.len() as u32);
                image.entities.push(SavedEntity {
                    key,
                    parent: u32::MAX,
                    prev: u32::MAX,
                    first: 0,
                    count: 0,
                    excluded: false,
                });
            }
        }
    }
    let mut next = 0usize;
    let mut slices: Vec<Option<&[u8]>> = Vec::new();
    for plan in &plans {
        let arch = &world.archetypes()[plan.archetype];
        for chunk in 0..arch.chunk_count() {
            let parents: Option<&[Parent]> = plan
                .parent
                .and_then(|c| arch.pod_column(chunk, c))
                .map(engine_ecs::bytemuck::cast_slice);
            slices.clear();
            slices.extend(
                plan.columns
                    .iter()
                    .map(|&(c, _, pod, _)| if pod { arch.pod_column(chunk, c) } else { None }),
            );
            for row in 0..arch.chunk_len(chunk) {
                let first = image.components.len() as u32;
                for (k, &(c, ty, pod, size)) in plan.columns.iter().enumerate() {
                    if pod {
                        let Some(column) = slices[k] else {
                            continue;
                        };
                        let at = image.bytes.len();
                        image
                            .bytes
                            .extend_from_slice(&column[row * size..(row + 1) * size]);
                        image.components.push(SavedValue {
                            ty,
                            raw: true,
                            at: at as u32,
                            len: size as u32,
                        });
                    } else {
                        // SAFETY: row `row` of the column holds a live value of the type
                        // the description `ty` was made from.
                        let ptr = unsafe { arch.column_base(chunk, c).add(row * size) };
                        let saved = image.encode_now(ty, ptr);
                        image.components.push(saved);
                    }
                }
                let (parent, prev) = parents.map_or((u32::MAX, u32::MAX), |p| {
                    let link = &p[row];
                    let parent = image.index.get(link.get());
                    let prev = image.index.get(link.previous_sibling());
                    (parent, if parent == u32::MAX { u32::MAX } else { prev })
                });
                let saved = &mut image.entities[next];
                saved.first = first;
                saved.count = image.components.len() as u32 - first;
                saved.parent = parent;
                saved.prev = prev;
                next += 1;
            }
        }
    }
    for t in reflected_types() {
        let ReflectedKind::Resource { get, .. } = t.kind else {
            continue;
        };
        if t.info.attrs.skip {
            continue;
        }
        let Some(ptr) = get(world) else {
            continue;
        };
        let ty = image.type_index(t.info);
        let saved = image.encode_now(ty, ptr);
        image.resources.push(saved);
    }
    if let Some(level) = level {
        image.destroyed = level
            .iter()
            .filter(|id| !present.contains(*id))
            .copied()
            .collect();
        image.destroyed.sort_unstable();
    }
    image
}
