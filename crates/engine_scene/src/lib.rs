//! Text scenes (ADR 0029; formats 2 to 4: ADR 0108): a line-oriented format for entities
//! and their reflected components, built to diff and merge well in version control.
//!
//! ```text
//! [scene version=4 id=9b41d07c2e8a4f6b8d3c1a5e7f902b64]
//!
//! [entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
//! order = 1024
//! Name = "Car"
//! Transform.rotation = (0, 0, 0, 1)
//! Transform.translation = (10, 0, 0)
//! Transform.scale = (1, 1, 1)
//!
//! [entity 0b12e4f8c3a94d5e9f7a6b1c2d3e4f50]
//! parent = 6c1f0a3e9d2b47c88e51f0d4a7b3c912
//! Health = 80
//! Inventory.items.0.id = 3
//! Inventory.tags = ["red", "fast"]
//! Inventory.flags["met the smith"] = true
//! Look.mesh = asset("props/car.gltf#Wheel", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
//! ```
//!
//! - The header carries the format's version and the scene's own [`AssetId`].
//! - One block per entity, headed by its stable [`SceneId`] (128-bit, hex).
//! - `parent = <id>` links the hierarchy. Children are written after their parent, in
//!   the parent's child order, so the order survives a save and load.
//! - `order = <n>` places a root among the roots ([`RootOrder`], format 3): the editor's
//!   tree shows them by it. Root blocks stay in id order, so moving a root changes its
//!   `order` line only.
//! - One line per value: `Component.path = value` ([`text`] has the grammar, shared with
//!   saves).
//!   - Components are sorted by name; fields keep declaration order.
//!   - Lists take indices, maps keys in brackets; lists and arrays of single values are
//!     written on one line (`[a, b]`).
//!   - A struct of one unnamed field is written as that field (`Health = 80`).
//!   - Asset references are written with their path, then the file's id ([`AssetRef`]).
//! - A component with no saved fields is written as `Component = {}`.
//! - Prefab instances (format 4, [`prefab`]): `[instance <id>]` places another scene, and
//!   `[override <instance>/<entity>]` changes the values of what it placed.
//!
//! Loading is tolerant of schema changes:
//! - fields missing from the file keep the component's `Default`;
//! - fields the type no longer has are reported as warnings, not errors.
//!
//! Unknown components are errors, reported with their line number. Files of formats 1 to
//! 3 load as they are (those of 1 and 2: their roots have no order).
//!
//! ```
//! use engine_ecs::{GlobalTransform, Transform, World};
//! use engine_scene::{Name, SceneId, SceneRegistry, load, save};
//!
//! let registry = SceneRegistry::with_engine_types();
//! let mut world = World::new();
//! let car = world.spawn((SceneId::new(), Name::new("Car"), Transform::from_xyz(10.0, 0.0, 0.0)));
//! let mut text = String::new();
//! save(&mut world, &registry, &mut text).unwrap();
//! assert!(text.contains("Name = \"Car\""));
//!
//! let mut copy = World::new();
//! let report = load(&mut copy, &registry, &text).unwrap();
//! let loaded = report.entities[0];
//! assert_eq!(copy.get::<Transform>(loaded), world.get::<Transform>(car));
//! assert_eq!(copy.get::<Name>(loaded).map(Name::as_str), Some("Car"));
//! assert!(copy.get::<GlobalTransform>(loaded).is_some(), "companion added");
//! ```

mod assets;
pub mod database;
pub mod prefab;
pub mod text;

pub use assets::{AssetId, AssetPaths, AssetRef};
pub use database::{
    AssetDatabase, AssetEntry, AssetKind, AssetMeta, ModelImport, ScanReport, TextureCompression,
    TextureImport,
};
pub use prefab::{KeptBlocks, PrefabFiles, PrefabInstance, PrefabPart, Prefabs};

use engine_ecs::reflected::WriteDefault;
use engine_ecs::{
    Component, ComponentId, Entity, GlobalTransform, QueryState, Transform, World, bytemuck,
};
use engine_reflect::{Primitive, Reflect, TypeInfo, TypeKind, Value};
use std::collections::HashMap;
use std::fmt::Write as _;
use text::{ReadRefs, Seg, is_asset_ref, is_entity, is_single, newtype_field, vector_fields};

/// Current format version, written in the header (4: prefab instances).
pub const VERSION: u32 = 4;

/// How written entity references find their scene ids.
type Ids<'a> = &'a dyn Fn(Entity) -> Option<SceneId>;

/// No entity has a scene id: references are written `@none`.
fn no_ids(_: Entity) -> Option<SceneId> {
    None
}

/// How written entity references are spelled.
#[derive(Clone, Copy)]
enum Refs<'a> {
    /// By the scene ids a function gives (undo's form).
    Ids(Ids<'a>),
    /// By scene id, or by path for an entity a prefab instance placed (a scene's form).
    Paths(&'a Paths<'a>),
}

impl Refs<'_> {
    /// Appends `e`'s reference after its `@`; false when it has none (`@none`).
    fn write(self, e: Entity, out: &mut String) -> bool {
        match self {
            Self::Ids(ids) => ids(e).is_some_and(|id| write!(out, "{id}").is_ok()),
            Self::Paths(paths) => paths.write(e, out),
        }
    }
}

/// The paths of a world's entities: a scene id, or for an entity a prefab instance placed,
/// its instance's path and its own id in the prefab (`7d2e…/a13f…`).
struct Paths<'a> {
    world: &'a World,
    ids: &'a HashMap<Entity, SceneId>,
    by_id: &'a HashMap<SceneId, Entity>,
}

impl Paths<'_> {
    fn write(&self, e: Entity, out: &mut String) -> bool {
        let Some(&id) = self.ids.get(&e) else {
            return false;
        };
        match self.world.get::<PrefabPart>(e) {
            Some(part) => {
                let instance = self.by_id.get(&part.instance).copied();
                if !instance.is_some_and(|i| i != e && self.write(i, out)) {
                    let _ = write!(out, "{}", part.instance);
                }
                let _ = write!(out, "/{}", part.local);
            }
            None => {
                let _ = write!(out, "{id}");
            }
        }
        true
    }

    /// `e`'s path below its outermost instance (`j/e` for `I/j/e`), for `remove = @…`.
    fn below_instance(&self, e: Entity) -> String {
        let mut full = String::new();
        self.write(e, &mut full);
        full.split_once('/')
            .map_or(String::new(), |(_, rest)| rest.to_owned())
    }
}

/// 128 random bits: the OS-seeded hasher, a process-wide counter and the time.
pub(crate) fn random_u128() -> u128 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // `RandomState` is seeded from the OS once per process and varies per instance.
    let half = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(n);
        h.write_u128(time);
        h.finish()
    };
    (u128::from(half()) << 64) | u128::from(half())
}

/// Stable identity of an entity in scene files (random 128 bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(C)]
pub struct SceneId {
    hi: u64,
    lo: u64,
}

// SAFETY: two `u64`s, no padding; every bit pattern is valid.
unsafe impl bytemuck::Zeroable for SceneId {}
// SAFETY: as above; `Copy` and `repr(C)`.
unsafe impl bytemuck::Pod for SceneId {}
engine_ecs::pod_component!(SceneId);

impl SceneId {
    /// A new random id.
    pub fn new() -> Self {
        Self::from_u128(random_u128())
    }

    /// The id a name always gives (FNV-1a, 128 bits): a game's long-lived entities (the
    /// player's character, a quest giver) named once, so saves match them on load and
    /// they keep what saves do not hold (their body, their renderer).
    pub const fn named(name: &str) -> Self {
        let bytes = name.as_bytes();
        let mut h: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
        let mut i = 0;
        while i < bytes.len() {
            h ^= bytes[i] as u128;
            h = h.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
            i += 1;
        }
        Self::from_u128(h)
    }

    /// An id from its 128-bit value.
    pub const fn from_u128(v: u128) -> Self {
        Self {
            hi: (v >> 64) as u64,
            lo: v as u64,
        }
    }

    /// The 128-bit value.
    pub const fn as_u128(self) -> u128 {
        ((self.hi as u128) << 64) | self.lo as u128
    }

    /// An id from its 32 hex digits.
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() != 32 {
            return None;
        }
        u128::from_str_radix(s, 16).ok().map(Self::from_u128)
    }

    /// The id of entity `local` (its id in a prefab's file) of the prefab placed as
    /// instance `instance` (ADR 0108's E5): the same at every load, so references into an
    /// instance and undo find it.
    pub fn derive(instance: Self, local: Self) -> Self {
        let mut h = engine_core::hash::StableHasher::new();
        h.write_str("prefab part");
        h.write(&instance.as_u128().to_le_bytes());
        h.write(&local.as_u128().to_le_bytes());
        Self::from_u128(h.finish())
    }

    /// The id a path of ids names (`instance/…/entity`): the last one, placed by each
    /// instance before it ([`Self::derive`]); a path of one id is that id.
    pub fn of_path(path: &[Self]) -> Self {
        let Some((&last, instances)) = path.split_last() else {
            return Self::from_u128(0);
        };
        instances
            .iter()
            .rev()
            .fold(last, |id, &instance| Self::derive(instance, id))
    }

    /// A path of ids from its text: ids of 32 hex digits apart by `/`.
    pub fn parse_path(s: &str) -> Option<Vec<Self>> {
        s.split('/').map(|p| Self::parse(p.trim())).collect()
    }
}

impl Default for SceneId {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a root entity comes among a scene's roots, as the editor's tree shows them (team
/// decision 2026-10-20): written `order = <n>` in the root's block, read back on load.
/// Values are spaced by [`RootOrder::STEP`], so a root placed between two others takes a
/// value in the gap and only its own line changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(transparent)]
pub struct RootOrder(pub u32);

// SAFETY: one `u32`; every bit pattern is valid.
unsafe impl bytemuck::Zeroable for RootOrder {}
// SAFETY: as above; `Copy` and `repr(transparent)`.
unsafe impl bytemuck::Pod for RootOrder {}
engine_ecs::pod_component!(RootOrder);

impl RootOrder {
    /// The gap between two roots placed one after the other.
    pub const STEP: u32 = 1024;
}

impl core::fmt::Display for SceneId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:032x}", self.as_u128())
    }
}

/// An entity's name, as the editor's tree shows it (ADR 0108). Names need not be unique:
/// [`SceneId`]s identify entities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Reflect)]
pub struct Name(pub String);
engine_ecs::reflect_component!(Name);

impl Name {
    /// A name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The entity named `name` (its [`Name`]): what a game finds in its loaded scene ("Cube",
/// "Camera"). With several of that name, one of them; `None` for none.
///
/// ```
/// use engine_ecs::World;
/// use engine_scene::{Name, find_named};
///
/// let mut world = World::new();
/// let door = world.spawn((Name::new("Door"),));
/// assert_eq!(find_named(&mut world, "Door"), Some(door));
/// assert_eq!(find_named(&mut world, "Gate"), None);
/// ```
pub fn find_named(world: &mut World, name: &str) -> Option<Entity> {
    let mut q = QueryState::<(Entity, &Name)>::new(world);
    q.iter_readonly(world)
        .find(|(_, n)| n.as_str() == name)
        .map(|(e, _)| e)
}

type Fill<'a> = dyn FnMut(*mut u8) + 'a;

/// How a registered type's values are reached.
enum Access {
    /// Registered with its Rust type.
    Typed {
        type_name: fn() -> &'static str,
        get: fn(&World, Entity) -> Option<*const u8>,
        insert: fn(&mut World, Entity, &mut Fill<'_>),
        remove: fn(&mut World, Entity),
    },
    /// From the program's list of reflected types (ADR 0104 step 1), by component id.
    Listed {
        id: fn(&mut World) -> ComponentId,
        default: WriteDefault,
    },
}

struct Entry {
    info: &'static TypeInfo,
    access: Access,
    companions: Vec<fn(&mut World, Entity)>,
}

/// Room for one value of a listed type while it is read, before it moves into the world.
struct Scratch {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl Scratch {
    fn new(info: &TypeInfo) -> Option<Self> {
        let layout = std::alloc::Layout::from_size_align(info.size.max(1), info.align).ok()?;
        // SAFETY: a non-zero size.
        let ptr = unsafe { std::alloc::alloc(layout) };
        (!ptr.is_null()).then_some(Self { ptr, layout })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout; the value in it was moved out or never made.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// The component types scenes may contain, by their reflected name.
#[derive(Default)]
pub struct SceneRegistry {
    entries: Vec<Entry>,
    by_name: HashMap<&'static str, usize>,
}

impl SceneRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The engine's own scene types: [`Transform`] (loaded entities also get a
    /// [`GlobalTransform`]) and [`Name`].
    pub fn with_engine_types() -> Self {
        let mut r = Self::new();
        r.register::<Transform>();
        r.require::<Transform, GlobalTransform>();
        r.register::<Name>();
        r
    }

    /// The engine's types ([`Self::with_engine_types`]) and every other reflected component
    /// linked into the program (`reflect_component!`), so a game's components need no
    /// registration. Left out: types marked `hidden` (runtime data such as
    /// `MeshRenderer` and the hierarchy's links) or `skip` (never saved), and types without
    /// `Default`, which a file cannot make.
    pub fn from_program() -> Self {
        let mut r = Self::with_engine_types();
        for t in engine_ecs::reflected_types() {
            let engine_ecs::ReflectedKind::Component { id, default } = t.kind else {
                continue;
            };
            if t.info.attrs.hidden || t.info.attrs.skip || r.by_name.contains_key(t.info.name) {
                continue;
            }
            let Some(default) = default() else {
                continue;
            };
            r.entries.push(Entry {
                info: t.info,
                access: Access::Listed { id, default },
                companions: Vec::new(),
            });
        }
        r.index();
        r
    }

    /// Makes `T` savable and loadable under its reflected name. Missing fields load as
    /// `T::default()`'s.
    ///
    /// # Panics
    ///
    /// If another type is registered under the same name.
    pub fn register<T: Component + Reflect + Default>(&mut self) -> &mut Self {
        let info = <T as Reflect>::TYPE_INFO;
        let access = Access::Typed {
            type_name: core::any::type_name::<T>,
            get: |w, e| w.get::<T>(e).map(|r| (r as *const T).cast::<u8>()),
            insert: |w, e, fill| {
                let mut value = T::default();
                fill((&raw mut value).cast::<u8>());
                w.insert(e, value);
            },
            remove: |w, e| {
                w.remove::<T>(e);
            },
        };
        if let Some(&i) = self.by_name.get(info.name) {
            let entry = &mut self.entries[i];
            match entry.access {
                Access::Typed { type_name, .. } => assert!(
                    entry.info.size == info.size && core::any::type_name::<T>() == type_name(),
                    "two scene types named `{}`",
                    info.name
                ),
                // The program's listing of the same type: reach it through its Rust type.
                Access::Listed { .. } => {
                    assert!(
                        entry.info.size == info.size,
                        "two scene types named `{}`",
                        info.name
                    );
                    entry.access = access;
                }
            }
            return self;
        }
        self.entries.push(Entry {
            info,
            access,
            companions: Vec::new(),
        });
        self.index();
        self
    }

    /// Sorts the entries by name and indexes them.
    fn index(&mut self) {
        self.entries.sort_by_key(|e| e.info.name);
        self.by_name = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.info.name, i))
            .collect();
    }

    /// Whether a type of this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// The registered types' descriptions, by name.
    pub fn types(&self) -> impl Iterator<Item = &'static TypeInfo> + '_ {
        self.entries.iter().map(|e| e.info)
    }

    /// Entities loaded with `T` also get `C::default()` if they lack a `C` (derived
    /// runtime data such as world transforms).
    ///
    /// # Panics
    ///
    /// If `T` is not registered.
    pub fn require<T: Component + Reflect, C: Component + Default>(&mut self) -> &mut Self {
        let i = self.by_name[<T as Reflect>::TYPE_INFO.name];
        self.entries[i].companions.push(|w: &mut World, e: Entity| {
            if w.get::<C>(e).is_none() {
                w.insert(e, C::default());
            }
        });
        self
    }
}

/// A scene could not be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// Two entities share a [`SceneId`].
    DuplicateId(SceneId),
}

impl core::fmt::Display for SaveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(f, "two entities have the scene id {id}"),
        }
    }
}

impl std::error::Error for SaveError {}

/// A scene could not be loaded: what went wrong, and on which line (1-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// Line number, 1-based (0: the whole file).
    pub line: usize,
    /// What went wrong.
    pub message: String,
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for LoadError {}

/// What [`load`] did.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// The loaded entities, in file order.
    pub entities: Vec<Entity>,
    /// Lines that were skipped (fields the types no longer have, unresolved entity
    /// references), with their line numbers.
    pub warnings: Vec<(usize, String)>,
    /// The scene's own id, from its header (none in format 1).
    pub id: Option<AssetId>,
    /// The format version the file was written in.
    pub version: u32,
    /// The paths the file's asset references were written with: give them back to
    /// [`save_with`] to write the references the same way.
    pub assets: AssetPaths,
    /// The prefabs its instances placed (and theirs), by id: what to watch.
    pub prefabs: Vec<AssetId>,
}

/// How [`save_with`] writes a scene.
#[derive(Clone, Copy, Default)]
pub struct SaveOptions<'a> {
    /// The scene's own id, written in the header.
    pub id: Option<AssetId>,
    /// The paths asset references are written with. A reference without one is written
    /// with its id alone (and its part's hash).
    pub assets: Option<&'a AssetPaths>,
    /// Where the prefabs its instances place come from: each instance is written as what
    /// its entities change from its prefab's. None (or a prefab that cannot be read):
    /// every line of an instance's entities is written as an override.
    pub prefabs: Option<&'a dyn Prefabs>,
}

impl core::fmt::Debug for SaveOptions<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SaveOptions")
            .field("id", &self.id)
            .field("assets", &self.assets)
            .field("prefabs", &self.prefabs.is_some())
            .finish()
    }
}

/// Gives every entity without a [`SceneId`] a new one. Returns how many were added.
pub fn assign_ids(world: &mut World) -> usize {
    let mut q = QueryState::<Entity, engine_ecs::Without<SceneId>>::new(world);
    let missing: Vec<Entity> = q.iter(world).collect();
    for &e in &missing {
        world.insert(e, SceneId::new());
    }
    missing.len()
}

/// Writes every entity that has a [`SceneId`] to `out` (appended), with the registered
/// components it has. Entities without an id are runtime-only and skipped; their
/// children with ids are written as roots. Returns the number of entities written.
pub fn save(
    world: &mut World,
    registry: &SceneRegistry,
    out: &mut String,
) -> Result<usize, SaveError> {
    save_with(world, registry, &SaveOptions::default(), out)
}

/// [`save`] with the scene's id, the paths of its asset references, and the prefabs its
/// instances place: each instance is written as the lines where its entities differ from
/// its prefab's ([`prefab`]).
pub fn save_with(
    world: &mut World,
    registry: &SceneRegistry,
    options: &SaveOptions<'_>,
    out: &mut String,
) -> Result<usize, SaveError> {
    save_from(world, registry, options, None, out)
}

/// Writes entity `root` and its descendants as a scene of their own (what a prefab made
/// of them holds, E5): `root` a root without a parent or an order; references to entities
/// outside it written as they are (a file that loads them alone warns of them).
pub fn save_subtree(
    world: &mut World,
    registry: &SceneRegistry,
    options: &SaveOptions<'_>,
    root: Entity,
    out: &mut String,
) -> Result<usize, SaveError> {
    save_from(world, registry, options, Some(root), out)
}

fn save_from(
    world: &mut World,
    registry: &SceneRegistry,
    options: &SaveOptions<'_>,
    only: Option<Entity>,
    out: &mut String,
) -> Result<usize, SaveError> {
    engine_profiler::zone!("scene::save");
    let mut q = QueryState::<(Entity, &SceneId)>::new(world);
    let mut ids: HashMap<Entity, SceneId> = HashMap::new();
    let mut roots: Vec<(SceneId, Entity)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (e, &id) in q.iter_readonly(world) {
        if !seen.insert(id) {
            return Err(SaveError::DuplicateId(id));
        }
        ids.insert(e, id);
    }
    match only {
        Some(root) => roots.extend(ids.get(&root).map(|&id| (id, root))),
        None => {
            for (&e, &id) in &ids {
                if world.parent(e).is_none_or(|p| !ids.contains_key(&p)) {
                    roots.push((id, e));
                }
            }
        }
    }
    roots.sort_unstable();
    // Instances: what their entities' paths and their prefabs' values need. A scene
    // without any is written as before, an entity's lines straight to `out`.
    let mut instances = QueryState::<(Entity, &PrefabInstance)>::new(world);
    let mut roots_of: Vec<(Entity, PrefabInstance)> = instances
        .iter_readonly(world)
        .map(|(e, i)| (e, *i))
        .collect();
    let mut parts = QueryState::<&PrefabPart>::new(world);
    let prefabs = !roots_of.is_empty() || parts.iter_readonly(world).next().is_some();
    let by_id: HashMap<SceneId, Entity> = if prefabs {
        ids.iter().map(|(&e, &id)| (id, e)).collect()
    } else {
        HashMap::new()
    };
    // Each outermost instance's prefab placed alone: what its entities are without the
    // scene's overrides.
    let mut bases: HashMap<Entity, Base> = HashMap::new();
    roots_of.retain(|&(e, i)| !i.is_missing() && world.get::<PrefabPart>(e).is_none());
    for (e, instance) in roots_of {
        if let Some(&id) = ids.get(&e)
            && let Some(base) = Base::of(registry, options, id, instance.prefab)
        {
            bases.insert(e, base);
        }
    }
    match options.id {
        Some(id) => {
            let _ = writeln!(out, "[scene version={VERSION} id={id}]");
        }
        None => {
            let _ = writeln!(out, "[scene version={VERSION}]");
        }
    }
    let getters = getters_of(registry, world);
    let paths = Paths {
        world,
        ids: &ids,
        by_id: &by_id,
    };
    let refs = Refs::Paths(&paths);
    let mut count = 0;
    // Depth first; an outermost instance's kept blocks after its whole subtree.
    let mut stack: Vec<(Entity, bool)> = roots.iter().rev().map(|&(_, e)| (e, true)).collect();
    let mut children: Vec<Entity> = Vec::new();
    // The writer's path, its room kept from one entity to the next.
    let mut path = String::new();
    while let Some((e, enter)) = stack.pop() {
        let (instance, part) = if prefabs {
            (
                world.get::<PrefabInstance>(e).copied(),
                world.get::<PrefabPart>(e).copied(),
            )
        } else {
            (None, None)
        };
        if !enter {
            if let Some(kept) = world.get::<KeptBlocks>(e)
                && !kept.blocks.trim().is_empty()
            {
                out.push('\n');
                out.push_str(kept.blocks.trim_end());
                out.push('\n');
            }
            continue;
        }
        let id = ids[&e];
        // Its parent: by path inside an instance, else by id; none for the subtree's root.
        let parent = world.parent(e).filter(|_| Some(e) != only).and_then(|p| {
            let &id = ids.get(&p)?;
            Some(if prefabs && world.get::<PrefabPart>(p).is_some() {
                ParentRef::Path(p)
            } else {
                ParentRef::Id(id)
            })
        });
        let lines = if prefabs && (part.is_some() || instance.is_some()) {
            component_lines(world, registry, &getters, e, refs, options.assets)
        } else {
            Vec::new()
        };
        match (part, instance) {
            (Some(_), _) => {
                // Inside an instance: an override where it differs from the prefab's.
                let outermost = outermost(world, &by_id, e);
                let base = outermost.and_then(|r| bases.get(&r));
                let (changed, removed) = match base.and_then(|b| b.lines(registry, id, options)) {
                    Some(prefab) => diff(&lines, &prefab),
                    None => (lines.iter().map(|(_, t)| t.as_str()).collect(), Vec::new()),
                };
                if !changed.is_empty() || !removed.is_empty() {
                    out.push_str("\n[override ");
                    paths.write(e, out);
                    out.push_str("]\n");
                    out.push_str(&changed);
                    for component in removed {
                        let _ = writeln!(out, "remove = {component}");
                    }
                }
            }
            (None, Some(instance)) => {
                let _ = write!(out, "\n[instance {id}]\nprefab = ");
                instance.prefab.write(options.assets, out);
                out.push('\n');
                match parent {
                    Some(p) => p.write(&paths, out),
                    None if only.is_none() => {
                        if let Some(order) = world.get::<RootOrder>(e) {
                            let _ = writeln!(out, "order = {}", order.0);
                        }
                    }
                    None => {}
                }
                if !instance.is_missing() {
                    let base = bases.get(&e);
                    match base.and_then(|b| b.lines(registry, id, options)) {
                        Some(prefab) => {
                            let (changed, removed) = diff(&lines, &prefab);
                            out.push_str(&changed);
                            for component in removed {
                                let _ = writeln!(out, "remove = {component}");
                            }
                        }
                        None => {
                            for (_, text) in &lines {
                                out.push_str(text);
                            }
                        }
                    }
                    // The prefab's entities this scene has deleted (the topmost of each
                    // deleted subtree).
                    if let Some(base) = base {
                        for removed in base.removed(&by_id) {
                            let _ = writeln!(out, "remove = @{removed}");
                        }
                    }
                }
                if let Some(kept) = world.get::<KeptBlocks>(e) {
                    out.push_str(&kept.lines);
                }
                stack.push((e, false));
            }
            (None, None) => {
                let _ = writeln!(out, "\n[entity {id}]");
                match parent {
                    Some(p) => p.write(&paths, out),
                    None if only.is_none() => {
                        if let Some(order) = world.get::<RootOrder>(e) {
                            let _ = writeln!(out, "order = {}", order.0);
                        }
                    }
                    None => {}
                }
                let writer = Writer {
                    out,
                    path: core::mem::take(&mut path),
                    refs,
                    assets: options.assets,
                };
                path = write_components(world, registry, &getters, e, writer);
            }
        }
        count += 1;
        children.clear();
        children.extend(world.children(e).filter(|c| ids.contains_key(c)));
        stack.extend(children.iter().rev().map(|&c| (c, true)));
    }
    Ok(count)
}

/// The outermost instance `e` is inside: its instance's root, then that one's, until an
/// entity no instance placed.
fn outermost(world: &World, by_id: &HashMap<SceneId, Entity>, e: Entity) -> Option<Entity> {
    let mut at = e;
    for _ in 0..64 {
        let part = world.get::<PrefabPart>(at)?;
        let instance = *by_id.get(&part.instance)?;
        if world.get::<PrefabPart>(instance).is_none() {
            return Some(instance);
        }
        at = instance;
    }
    None
}

/// How each registered type is read, its id found once for `world`.
fn getters_of(registry: &SceneRegistry, world: &mut World) -> Vec<Getter> {
    registry
        .entries
        .iter()
        .map(|entry| match entry.access {
            Access::Typed { get, .. } => Getter::Typed(get),
            Access::Listed { id, .. } => Getter::Id(id(world)),
        })
        .collect()
}

/// How a block names its parent: by id, or by path for one an instance placed.
#[derive(Clone, Copy)]
enum ParentRef {
    Id(SceneId),
    Path(Entity),
}

impl ParentRef {
    /// Writes `parent = …`.
    fn write(self, paths: &Paths<'_>, out: &mut String) {
        match self {
            Self::Id(id) => {
                let _ = writeln!(out, "parent = {id}");
            }
            Self::Path(p) => {
                out.push_str("parent = ");
                paths.write(p, out);
                out.push('\n');
            }
        }
    }
}

/// Writes the lines of the registered components `e` has through `writer`
/// (`Component = {}` for one without); gives its path's room back.
fn write_components(
    world: &World,
    registry: &SceneRegistry,
    getters: &[Getter],
    e: Entity,
    mut writer: Writer<'_>,
) -> String {
    for (entry, getter) in registry.entries.iter().zip(getters) {
        let ptr = match *getter {
            Getter::Typed(get) => get(world, e),
            Getter::Id(id) => world.get_raw(e, id).map(|p| p.as_ptr().cast_const()),
        };
        let Some(ptr) = ptr else {
            continue;
        };
        writer.path.clear();
        writer.path.push_str(entry.info.name);
        let before = writer.out.len();
        // SAFETY: `ptr` points to a live value of the registered type.
        unsafe { writer.value(entry.info, ptr) };
        if writer.out.len() == before {
            let _ = writeln!(writer.out, "{} = {{}}", entry.info.name);
        }
    }
    writer.path
}

/// The registered components `e` has, each with its lines (`Component = {}` for one
/// without).
fn component_lines(
    world: &World,
    registry: &SceneRegistry,
    getters: &[Getter],
    e: Entity,
    refs: Refs<'_>,
    assets: Option<&AssetPaths>,
) -> Vec<(&'static str, String)> {
    let mut lines = Vec::new();
    for (entry, getter) in registry.entries.iter().zip(getters) {
        let ptr = match *getter {
            Getter::Typed(get) => get(world, e),
            Getter::Id(id) => world.get_raw(e, id).map(|p| p.as_ptr().cast_const()),
        };
        let Some(ptr) = ptr else {
            continue;
        };
        let mut text = String::new();
        let mut writer = Writer {
            out: &mut text,
            path: String::from(entry.info.name),
            refs,
            assets,
        };
        // SAFETY: `ptr` points to a live value of the registered type.
        unsafe { writer.value(entry.info, ptr) };
        if text.is_empty() {
            let _ = writeln!(text, "{} = {{}}", entry.info.name);
        }
        lines.push((entry.info.name, text));
    }
    lines
}

/// What `lines` (an entity's) change from `prefab`'s (the same entity as its prefab places
/// it): their lines that differ, a component's whole after `Component = {}` when its lines
/// name other fields (a list grown or shrunk), and the components the prefab's has that it
/// has not.
fn diff(
    lines: &[(&'static str, String)],
    prefab: &[(&'static str, String)],
) -> (String, Vec<&'static str>) {
    let fields = |text: &str| -> Vec<String> {
        text.lines()
            .map(|l| l.split_once(" = ").map_or(l, |(p, _)| p).to_owned())
            .collect()
    };
    let mut out = String::new();
    for (name, text) in lines {
        match prefab.iter().find(|(n, _)| n == name) {
            None => out.push_str(text),
            Some((_, theirs)) if theirs == text => {}
            Some((_, theirs)) if fields(text) == fields(theirs) => {
                let same: std::collections::HashSet<&str> = theirs.lines().collect();
                for line in text.lines().filter(|l| !same.contains(l)) {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Some(_) => {
                let _ = writeln!(out, "{name} = {{}}");
                let empty = format!("{name} = {{}}");
                for line in text.lines().filter(|&l| l != empty) {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    let removed = prefab
        .iter()
        .filter(|(n, _)| !lines.iter().any(|(m, _)| m == n))
        .map(|&(n, _)| n)
        .collect();
    (out, removed)
}

/// An outermost instance's prefab placed alone, under the same ids: what its entities
/// are without the scene's overrides. Saving compares an instance with it; an editor shows
/// which values are overrides and puts them back from it ([`Self::component`]).
pub struct PrefabBase {
    world: World,
    ids: HashMap<Entity, SceneId>,
    by_id: HashMap<SceneId, Entity>,
    getters: Vec<Getter>,
    /// The instance's own id.
    instance: SceneId,
}

/// The name [`SaveOptions`] keeps for it.
type Base = PrefabBase;

impl PrefabBase {
    /// Prefab `prefab` placed alone as instance `instance`, through `options`' prefabs;
    /// none when it cannot be read now.
    fn of(
        registry: &SceneRegistry,
        options: &SaveOptions<'_>,
        instance: SceneId,
        prefab: AssetRef,
    ) -> Option<Self> {
        Self::new(registry, options.prefabs?, options.assets, instance, prefab)
    }

    /// Prefab `prefab` placed alone, read through `prefabs` (its path from `assets`, or
    /// the one `prefabs` finds by id), as instance `instance`: under the ids the instance's
    /// entities have. None when it cannot be read or placed now.
    pub fn new(
        registry: &SceneRegistry,
        prefabs: &dyn Prefabs,
        assets: Option<&AssetPaths>,
        instance: SceneId,
        prefab: AssetRef,
    ) -> Option<Self> {
        let mut text = format!("[scene version={VERSION}]\n\n[instance {instance}]\nprefab = ");
        prefab.write(assets, &mut text);
        text.push('\n');
        let mut world = World::new();
        let load = LoadOptions {
            prefabs: Some(prefabs),
        };
        let report = load_with(&mut world, registry, &text, &load).ok()?;
        let mut ids = HashMap::with_capacity(report.entities.len());
        for &e in &report.entities {
            if let Some(&id) = world.get::<SceneId>(e) {
                ids.insert(e, id);
            }
        }
        let by_id: HashMap<SceneId, Entity> = ids.iter().map(|(&e, &id)| (id, e)).collect();
        let root = *by_id.get(&instance)?;
        if world
            .get::<PrefabInstance>(root)
            .is_none_or(PrefabInstance::is_missing)
        {
            return None;
        }
        let getters = getters_of(registry, &mut world);
        Some(Self {
            world,
            ids,
            by_id,
            getters,
            instance,
        })
    }

    /// The world it is placed in.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// The entity with scene id `id`, as the prefab places it.
    pub fn entity(&self, id: SceneId) -> Option<Entity> {
        self.by_id.get(&id).copied()
    }

    /// The value of the component named `name` (its type's name, as
    /// `engine_ecs::Components::name` gives it) on entity `id`, as the prefab places it;
    /// none when the prefab places no such entity, or it has no such component.
    pub fn component(&self, id: SceneId, name: &str) -> Option<core::ptr::NonNull<u8>> {
        let e = self.entity(id)?;
        let components = self.world.components();
        let found = self
            .world
            .component_ids_of(e)?
            .iter()
            .copied()
            .find(|&c| components.name(c) == Some(name))?;
        self.world.get_raw(e, found)
    }

    /// The names of the components entity `id` has as the prefab places it.
    pub fn component_names(&self, id: SceneId) -> Vec<&str> {
        let components = self.world.components();
        self.entity(id)
            .and_then(|e| self.world.component_ids_of(e))
            .map_or_else(Vec::new, |ids| {
                ids.iter().filter_map(|&c| components.name(c)).collect()
            })
    }

    /// The lines of entity `id` as the prefab places it; none when it places no such
    /// entity.
    fn lines(
        &self,
        registry: &SceneRegistry,
        id: SceneId,
        options: &SaveOptions<'_>,
    ) -> Option<Vec<(&'static str, String)>> {
        let e = *self.by_id.get(&id)?;
        let paths = Paths {
            world: &self.world,
            ids: &self.ids,
            by_id: &self.by_id,
        };
        Some(component_lines(
            &self.world,
            registry,
            &self.getters,
            e,
            Refs::Paths(&paths),
            options.assets,
        ))
    }

    /// The paths below the instance of the entities the prefab places that the scene
    /// (`by_id`) no longer has: the topmost of each such subtree.
    fn removed(&self, by_id: &HashMap<SceneId, Entity>) -> Vec<String> {
        let paths = Paths {
            world: &self.world,
            ids: &self.ids,
            by_id: &self.by_id,
        };
        let mut out: Vec<(SceneId, String)> = Vec::new();
        for (&e, &id) in &self.ids {
            if id == self.instance || by_id.contains_key(&id) {
                continue;
            }
            // Under one also removed: that one's line covers it.
            let parent_gone = self
                .world
                .parent(e)
                .and_then(|p| self.ids.get(&p))
                .is_some_and(|&p| p != self.instance && !by_id.contains_key(&p));
            if !parent_gone {
                out.push((id, paths.below_instance(e)));
            }
        }
        out.sort_unstable();
        out.into_iter().map(|(_, path)| path).collect()
    }
}

/// How [`save_with`] reads a registered type's value.
#[derive(Clone, Copy)]
enum Getter {
    Typed(fn(&World, Entity) -> Option<*const u8>),
    Id(ComponentId),
}

/// Writes the lines of a scene.
struct Writer<'a> {
    out: &'a mut String,
    /// The path of the value being written, `Component.field…`.
    path: String,
    refs: Refs<'a>,
    assets: Option<&'a AssetPaths>,
}

impl Writer<'_> {
    /// Writes `path = text`.
    fn line(&mut self, text: &str) {
        let _ = writeln!(self.out, "{} = {text}", self.path);
    }

    /// Writes the lines of the value at `ptr` (type `ty`) under the current path.
    ///
    /// # Safety
    ///
    /// `ptr` points to a valid value of `ty`.
    unsafe fn value(&mut self, ty: &'static TypeInfo, ptr: *const u8) {
        // SAFETY: for the block, every step stays inside the value its description lays
        // out; containers are read through their own functions.
        unsafe {
            if is_single(ty) {
                let _ = write!(self.out, "{} = ", self.path);
                single(self.out, ty, ptr, self.refs, self.assets);
                self.out.push('\n');
                return;
            }
            match ty.kind {
                TypeKind::Struct(fields) => {
                    if let Some(f) = newtype_field(ty) {
                        self.value(f.ty, ptr.add(f.offset));
                        return;
                    }
                    for f in fields {
                        if f.attrs.skip {
                            continue;
                        }
                        let len = self.path.len();
                        if len > 0 {
                            self.path.push('.');
                        }
                        self.path.push_str(f.name);
                        self.value(f.ty, ptr.add(f.offset));
                        self.path.truncate(len);
                    }
                }
                TypeKind::Array { element, len } => {
                    if is_single(element) {
                        self.items(element, ptr, len);
                        return;
                    }
                    for i in 0..len {
                        self.index(i, element, ptr.add(i * element.size));
                    }
                }
                TypeKind::List(list) => {
                    let len = (list.len)(ptr);
                    let items = (list.items)(ptr);
                    if is_single(list.element) || len == 0 {
                        self.items(list.element, items, len);
                        return;
                    }
                    for i in 0..len {
                        self.index(i, list.element, items.add(i * list.element.size));
                    }
                }
                TypeKind::Option(option) => {
                    let inner = (option.get)(ptr);
                    if inner.is_null() {
                        self.line("none");
                        return;
                    }
                    let before = self.out.len();
                    self.value(option.inner, inner);
                    if self.out.len() == before {
                        self.line("some");
                    }
                }
                TypeKind::Map(map) => {
                    if (map.len)(ptr) == 0 {
                        self.line("{}");
                        return;
                    }
                    if !is_single(map.key) {
                        return;
                    }
                    let mut entries: Vec<(String, *const u8)> = Vec::with_capacity((map.len)(ptr));
                    (map.for_each)(ptr, &mut |k, v| {
                        let mut key = String::new();
                        single(&mut key, map.key, k, self.refs, self.assets);
                        entries.push((key, v));
                    });
                    if !map.ordered {
                        entries.sort_by(|a, b| a.0.cmp(&b.0));
                    }
                    for (key, v) in entries {
                        let len = self.path.len();
                        let _ = write!(self.path, "[{key}]");
                        self.value(map.value, v);
                        self.path.truncate(len);
                    }
                }
                TypeKind::Primitive(_)
                | TypeKind::Enum { .. }
                | TypeKind::String
                | TypeKind::Opaque => {}
            }
        }
    }

    /// Writes `path.i`'s lines.
    ///
    /// # Safety
    ///
    /// `ptr` points to a valid value of `ty`.
    unsafe fn index(&mut self, i: usize, ty: &'static TypeInfo, ptr: *const u8) {
        let len = self.path.len();
        let _ = write!(self.path, ".{i}");
        // SAFETY: as the caller guarantees.
        unsafe { self.value(ty, ptr) };
        self.path.truncate(len);
    }

    /// Writes `path = [a, b, …]` for `len` single values from `first`.
    ///
    /// # Safety
    ///
    /// `first` points to `len` valid values of `ty`, one after another.
    unsafe fn items(&mut self, ty: &'static TypeInfo, first: *const u8, len: usize) {
        let _ = write!(self.out, "{} = [", self.path);
        for i in 0..len {
            if i > 0 {
                self.out.push_str(", ");
            }
            // SAFETY: element `i` of the run.
            unsafe { single(self.out, ty, first.add(i * ty.size), self.refs, self.assets) };
        }
        self.out.push_str("]\n");
    }
}

/// Appends a single value's text (see [`is_single`]).
///
/// # Safety
///
/// `ptr` points to a valid value of `ty`, which [`is_single`] accepts.
unsafe fn single(
    out: &mut String,
    ty: &'static TypeInfo,
    ptr: *const u8,
    refs: Refs<'_>,
    assets: Option<&AssetPaths>,
) {
    // SAFETY: for the block, as the caller guarantees.
    unsafe {
        if is_entity(ty) {
            let e = ptr.cast::<Entity>().read_unaligned();
            out.push('@');
            if !refs.write(e, out) {
                out.push_str("none");
            }
            return;
        }
        if is_asset_ref(ty) {
            ptr.cast::<AssetRef>().read_unaligned().write(assets, out);
            return;
        }
        if let Some(fields) = vector_fields(ty) {
            out.push('(');
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_primitive(out, f.ty, ptr.add(f.offset));
            }
            out.push(')');
            return;
        }
        if let Some(f) = newtype_field(ty) {
            single(out, f.ty, ptr.add(f.offset), refs, assets);
            return;
        }
        match ty.kind {
            TypeKind::String => text::quote_into(out, engine_reflect::read_str(ptr)),
            _ => write_primitive(out, ty, ptr),
        }
    }
}

/// # Safety
///
/// `ptr` points to a valid primitive or enum of `ty`.
unsafe fn write_primitive(out: &mut String, ty: &TypeInfo, ptr: *const u8) {
    // SAFETY: as the caller guarantees; floats are read at their own width so they print
    // in their shortest round-trip form.
    unsafe {
        match ty.kind {
            TypeKind::Primitive(Primitive::F32) => {
                let _ = write!(out, "{}", ptr.cast::<f32>().read_unaligned());
            }
            TypeKind::Primitive(Primitive::F64) => {
                let _ = write!(out, "{}", ptr.cast::<f64>().read_unaligned());
            }
            TypeKind::Enum { variants, .. } => {
                let v = engine_reflect::read(ptr, ty);
                let raw = match v {
                    Some(Value::Int(i)) => i,
                    Some(Value::UInt(u)) => u as i64,
                    _ => 0,
                };
                match variants.iter().find(|var| var.value == raw) {
                    Some(var) => out.push_str(var.name),
                    None => {
                        let _ = write!(out, "{raw}");
                    }
                }
            }
            _ => match engine_reflect::read(ptr, ty) {
                Some(Value::Bool(b)) => out.push_str(if b { "true" } else { "false" }),
                Some(Value::Int(i)) => {
                    let _ = write!(out, "{i}");
                }
                Some(Value::UInt(u)) => {
                    let _ = write!(out, "{u}");
                }
                Some(Value::Float(f)) => {
                    let _ = write!(out, "{f}");
                }
                None => {}
            },
        }
    }
}

/// What a block of a scene file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `[entity <id>]`: an entity of the scene.
    Entity,
    /// `[instance <id>]`: a prefab placed ([`prefab`]).
    Instance,
    /// `[override <instance>/<entity>…]`: lines for an entity an instance placed.
    Override,
}

pub(crate) struct Block<'a> {
    pub(crate) line: usize,
    pub(crate) kind: Kind,
    /// The entity's id: its own, the instance's, or an override's target's
    /// ([`SceneId::of_path`] of its target).
    pub(crate) id: SceneId,
    pub(crate) parent: Option<(usize, SceneId)>,
    /// `order = <n>`: the root's place among the roots.
    pub(crate) order: Option<(usize, u32)>,
    /// (line, component, path after the component, value)
    pub(crate) fields: Vec<(usize, &'a str, &'a str, &'a str)>,
    /// What instances and overrides have besides (none for an entity's).
    more: Option<Box<BlockMore<'a>>>,
}

/// What an instance's or an override's block has besides an entity's.
#[derive(Default)]
struct BlockMore<'a> {
    /// An override's target as written, its instance first.
    target: Vec<SceneId>,
    /// An instance's `prefab = asset(…)` value.
    prefab: Option<(usize, &'a str)>,
    /// An instance's `remove = @<entity>` lines, an override's `remove = <Component>`.
    removes: Vec<(usize, &'a str)>,
    /// The block as written, from its header to the next one.
    text: &'a str,
    /// An instance's lines but its header, prefab, parent and order, as written.
    body: String,
}

impl<'a> Block<'a> {
    fn new(line: usize, kind: Kind, id: SceneId) -> Self {
        Self {
            line,
            kind,
            id,
            parent: None,
            order: None,
            fields: Vec::new(),
            more: (kind != Kind::Entity).then(Box::default),
        }
    }

    /// An override's target as written, its instance first.
    pub(crate) fn target(&self) -> &[SceneId] {
        self.more.as_ref().map_or(&[], |m| &m.target)
    }

    /// An instance's `prefab = asset(…)` value.
    pub(crate) fn prefab(&self) -> Option<(usize, &'a str)> {
        self.more.as_ref().and_then(|m| m.prefab)
    }

    /// An instance's `remove = @<entity>` lines, an override's `remove = <Component>`.
    pub(crate) fn removes(&self) -> &[(usize, &'a str)] {
        self.more.as_ref().map_or(&[], |m| &m.removes)
    }

    /// The block as written (an instance's or an override's).
    pub(crate) fn text(&self) -> &'a str {
        self.more.as_ref().map_or("", |m| m.text)
    }

    /// An instance's lines but its header, prefab, parent and order, as written.
    pub(crate) fn body(&self) -> &str {
        self.more.as_ref().map_or("", |m| &m.body)
    }
}

/// A parsed file: its header and its blocks.
pub(crate) struct Parsed<'a> {
    pub(crate) version: u32,
    pub(crate) id: Option<AssetId>,
    pub(crate) blocks: Vec<Block<'a>>,
}

pub(crate) fn err(line: usize, message: impl Into<String>) -> LoadError {
    LoadError {
        line,
        message: message.into(),
    }
}

/// A header's version and id: `[<kind> version=2 id=<32 hex digits>]`.
fn header(
    kind: &str,
    newest: u32,
    line: usize,
    s: &str,
) -> Result<(u32, Option<AssetId>), LoadError> {
    let expected = || {
        err(
            line,
            format!("expected the header `[{kind} version={newest}]`"),
        )
    };
    let inner = s
        .strip_prefix('[')
        .and_then(|r| r.strip_prefix(kind))
        .and_then(|r| r.strip_prefix(' '))
        .and_then(|r| r.strip_suffix(']'))
        .ok_or_else(expected)?;
    let (mut version, mut id) = (None, None);
    for item in inner.split_whitespace() {
        match item.split_once('=') {
            Some(("version", v)) => version = Some(v.parse::<u32>().map_err(|_| expected())?),
            Some(("id", v)) => {
                id = Some(AssetId::parse(v).ok_or_else(|| {
                    err(line, format!("`{v}` is not a scene id (32 hex digits)"))
                })?);
            }
            _ => return Err(expected()),
        }
    }
    let version = version.ok_or_else(expected)?;
    if version > newest {
        return Err(err(
            line,
            format!("{kind} version {version} is newer than this engine ({newest})"),
        ));
    }
    Ok((version, id))
}

/// The ids of a scene's roots (its entity and instance blocks without a parent), in the
/// file's order: what a prefab's single root is written as.
///
/// # Errors
///
/// It is not a scene (with the line).
pub fn scene_roots(text: &str) -> Result<Vec<SceneId>, LoadError> {
    Ok(parse(text)?
        .blocks
        .iter()
        .filter(|b| b.kind != Kind::Override && b.parent.is_none())
        .map(|b| b.id)
        .collect())
}

/// A scene file's version and own id, from its header, without loading it: what an
/// editor keeps to save the scene back under the same id.
///
/// # Errors
///
/// When the first line that is not empty or a comment is not a scene header.
pub fn scene_header(text: &str) -> Result<(u32, Option<AssetId>), LoadError> {
    for (i, raw) in text.lines().enumerate() {
        let s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        return header("scene", VERSION, i + 1, s);
    }
    Err(err(
        0,
        format!("empty file: expected the header `[scene version={VERSION}]`"),
    ))
}

pub(crate) fn parse(text: &str) -> Result<Parsed<'_>, LoadError> {
    let mut blocks: Vec<Block<'_>> = Vec::new();
    let mut head = None;
    // Where the last block began, in bytes.
    let mut start = 0;
    let mut offset = 0;
    for (i, raw) in text.split_inclusive('\n').enumerate() {
        let line = i + 1;
        let at = offset;
        offset += raw.len();
        let raw = raw.trim_end_matches(['\n', '\r']);
        let s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        if head.is_none() {
            head = Some(header("scene", VERSION, line, s)?);
            continue;
        }
        if s.starts_with('[') {
            let block = if let Some(rest) = s.strip_prefix("[entity ") {
                let id = rest
                    .strip_suffix(']')
                    .and_then(|h| SceneId::parse(h.trim()))
                    .ok_or_else(|| err(line, "expected `[entity <32 hex digits>]`"))?;
                Block::new(line, Kind::Entity, id)
            } else if let Some(rest) = s.strip_prefix("[instance ") {
                let id = rest
                    .strip_suffix(']')
                    .and_then(|h| SceneId::parse(h.trim()))
                    .ok_or_else(|| err(line, "expected `[instance <32 hex digits>]`"))?;
                Block::new(line, Kind::Instance, id)
            } else if let Some(rest) = s.strip_prefix("[override ") {
                let target = rest
                    .strip_suffix(']')
                    .and_then(|h| SceneId::parse_path(h.trim()))
                    .filter(|path| path.len() > 1)
                    .ok_or_else(|| err(line, "expected `[override <instance id>/<entity id>…]`"))?;
                let mut block = Block::new(line, Kind::Override, SceneId::of_path(&target));
                if let Some(more) = &mut block.more {
                    more.target = target;
                }
                block
            } else {
                return Err(err(
                    line,
                    "expected `[entity <id>]`, `[instance <id>]` or `[override <path>]`",
                ));
            };
            if let Some(more) = blocks.last_mut().and_then(|b| b.more.as_mut()) {
                more.text = &text[start..at];
            }
            start = at;
            blocks.push(block);
            continue;
        }
        let block = blocks
            .last_mut()
            .ok_or_else(|| err(line, "a field before the first `[entity ...]`"))?;
        let (key, value) =
            text::split_assignment(s).ok_or_else(|| err(line, "expected `key = value`"))?;
        let kind = block.kind;
        match key {
            "parent" if kind != Kind::Override => {
                let path = SceneId::parse_path(value)
                    .ok_or_else(|| err(line, "expected a scene id, or a path of them"))?;
                block.parent = Some((line, SceneId::of_path(&path)));
                continue;
            }
            "order" if kind != Kind::Override => {
                let n = value
                    .parse::<u32>()
                    .map_err(|_| err(line, "expected a whole number from 0 to 4294967295"))?;
                block.order = Some((line, n));
                continue;
            }
            "prefab" if kind == Kind::Instance => {
                if let Some(more) = &mut block.more {
                    more.prefab = Some((line, value));
                }
                continue;
            }
            "remove" if kind != Kind::Entity => {
                if let Some(more) = &mut block.more {
                    more.removes.push((line, value));
                }
            }
            _ => {
                let (component, path) = text::component_of(key);
                block.fields.push((line, component, path, value));
            }
        }
        if kind == Kind::Instance
            && let Some(more) = &mut block.more
        {
            more.body.push_str(raw);
            more.body.push('\n');
        }
    }
    if let Some(more) = blocks.last_mut().and_then(|b| b.more.as_mut()) {
        more.text = &text[start..];
    }
    let (version, id) = head.ok_or_else(|| {
        err(
            0,
            format!("empty file: expected the header `[scene version={VERSION}]`"),
        )
    })?;
    Ok(Parsed {
        version,
        id,
        blocks,
    })
}

/// How a scene's values name entities and assets.
struct SceneRefs<'a> {
    index: &'a HashMap<SceneId, usize>,
    /// The entities placed, by their index in the plan (none: left out).
    entities: &'a [Option<Entity>],
    /// The namespace the current line's references are read in.
    chain: &'a [prefab::Step],
    /// An entity reference not in the scene, on the current line.
    warning: Option<String>,
    assets: &'a mut AssetPaths,
}

impl ReadRefs for SceneRefs<'_> {
    fn entity(&mut self, label: &str) -> Result<Entity, String> {
        if label == "none" {
            return Ok(Entity::NULL);
        }
        let path = SceneId::parse_path(label).ok_or_else(|| {
            format!("expected `@<scene id>`, a path of them, or `@none`, found `@{label}`")
        })?;
        let id = prefab::map(self.chain, SceneId::of_path(&path));
        match self.index.get(&id).and_then(|&i| self.entities[i]) {
            Some(e) => Ok(e),
            None => {
                self.warning = Some(format!("entity {label} is not in this scene"));
                Ok(Entity::NULL)
            }
        }
    }

    fn asset(&mut self, asset: AssetRef, path: &str) {
        self.assets.insert(asset, path);
    }
}

/// Whether `Component = value` sets a whole component (one written as a single value).
pub(crate) fn whole_value(info: &TypeInfo) -> bool {
    is_single(info) || newtype_field(info).is_some()
}

/// How [`load_with`] loads a scene.
#[derive(Clone, Copy, Default)]
pub struct LoadOptions<'a> {
    /// Where the prefabs its instances place come from ([`PrefabFiles`] reads them from a
    /// folder). None: each instance is kept as written, its prefab not placed, with a
    /// warning.
    pub prefabs: Option<&'a dyn Prefabs>,
}

/// Loads a scene into `world`: spawns its entities (with their [`SceneId`]s), their
/// components and the hierarchy. On error nothing is kept. Its prefab instances are kept
/// as written, their prefabs not placed: [`load_with`] places them.
pub fn load(
    world: &mut World,
    registry: &SceneRegistry,
    text: &str,
) -> Result<LoadReport, LoadError> {
    load_with(world, registry, text, &LoadOptions::default())
}

/// [`load`], the prefabs its instances place read through `options` ([`prefab`]).
pub fn load_with(
    world: &mut World,
    registry: &SceneRegistry,
    text: &str,
    options: &LoadOptions<'_>,
) -> Result<LoadReport, LoadError> {
    engine_profiler::zone!("scene::load");
    let parsed = parse(text)?;
    let texts = prefab::read_texts(&parsed, options.prefabs);
    let prefabs = prefab::parse_texts(&texts, registry);
    let mut plan = prefab::Plan::new();
    let mut planning = prefab::Planning {
        prefabs: &prefabs,
        stack: Vec::new(),
    };
    let place = prefab::Place {
        chain: 0,
        at: None,
        file: None,
    };
    prefab::plan_scene(&mut plan, &mut planning, &parsed, place)?;
    spawn(world, registry, plan, parsed.id, parsed.version)
}

/// Spawns what `plan` places: its entities, their components (a prefab's lines first,
/// then the overrides), the hierarchy. On error nothing is kept.
fn spawn(
    world: &mut World,
    registry: &SceneRegistry,
    plan: prefab::Plan<'_, '_>,
    id: Option<AssetId>,
    version: u32,
) -> Result<LoadReport, LoadError> {
    let prefab::Plan {
        entities: planned,
        chains,
        files,
        mut warnings,
        prefabs,
        paths,
        index,
    } = plan;
    // What is left out: an entity gone, and everything under one (nothing, mostly).
    let any_gone = planned.iter().any(|p| p.gone);
    let mut gone: Vec<Option<bool>> =
        vec![if any_gone { None } else { Some(false) }; planned.len()];
    let mut chain = Vec::new();
    for i in 0..if any_gone { planned.len() } else { 0 } {
        chain.clear();
        let mut at = i;
        let out = loop {
            if let Some(known) = gone[at] {
                break known;
            }
            chain.push(at);
            if planned[at].gone {
                break true;
            }
            match planned[at].parent.and_then(|(_, p)| index.get(&p).copied()) {
                // A loop of parents ends at the entity it started from.
                Some(p) if !chain.contains(&p) => at = p,
                _ => break false,
            }
        };
        for &k in &chain {
            gone[k] = Some(out);
        }
    }
    let gone: Vec<bool> = gone.into_iter().map(|g| g == Some(true)).collect();
    // Parents: the scene's own entities' must be there; a prefab's go on as roots.
    for (p, &g) in planned.iter().zip(&gone) {
        if g {
            continue;
        }
        if let Some((line, parent)) = p.parent
            && index.get(&parent).is_none_or(|&i| gone[i])
        {
            if p.own && !index.contains_key(&parent) {
                return Err(err(line, format!("parent {parent} is not in this scene")));
            }
            warnings.push((
                line,
                format!("entity {}: its parent is not in this scene", p.id),
            ));
        }
    }
    let entities: Vec<Option<Entity>> = planned
        .iter()
        .zip(&gone)
        .map(|(p, &g)| (!g).then(|| world.spawn((p.id,))))
        .collect();
    let spawned: Vec<Entity> = entities.iter().flatten().copied().collect();
    let mut assets = AssetPaths::new();
    for (prefab, path) in &paths {
        assets.insert(AssetRef::file(*prefab), path);
    }
    let mut first_error: Option<LoadError> = None;
    let mut segs: Vec<Seg<'_>> = Vec::new();
    let mut scratches: Vec<Option<Scratch>> = registry.entries.iter().map(|_| None).collect();
    let mut lines: Vec<prefab::Line<'_>> = Vec::new();
    for (i, p) in planned.iter().enumerate() {
        let Some(e) = entities[i] else {
            continue;
        };
        // Each component's lines together, in the order they come: the prefab's, then
        // the overrides, sorted together; `Component = {}` starts it again from its
        // default. A block's own lines alone come grouped (as a scene writes them) and are
        // read where they are.
        let (fields, place) = p.block.unwrap_or((&[], prefab::Place::SCENE));
        let more = p.more.as_ref().filter(|m| !m.lines.is_empty());
        lines.clear();
        if let Some(more) = more {
            lines.extend(fields.iter().map(|&f| place.line(f)));
            lines.extend(more.lines.iter().copied());
            lines.sort_by_key(|l| l.component);
        }
        let count = if more.is_some() {
            lines.len()
        } else {
            fields.len()
        };
        let line = |k: usize| {
            if more.is_some() {
                lines[k]
            } else {
                place.line(fields[k])
            }
        };
        let mut start = 0;
        while start < count {
            let component = line(start).component;
            let mut end = start + 1;
            while end < count && line(end).component == component {
                end += 1;
            }
            let from = (start..end)
                .rev()
                .find(|&k| {
                    let l = line(k);
                    l.path.is_empty() && l.value == "{}"
                })
                .unwrap_or(start);
            let Some(&entry_index) = registry.by_name.get(component) else {
                // The scene's own unknown component fails the load; a prefab's was refused
                // before it was placed.
                let l = line(from);
                let message = format!("unknown component `{component}`");
                if l.from.is_none() {
                    first_error.get_or_insert_with(|| err(l.at, message));
                } else {
                    warnings.push((l.at, message));
                }
                start = end;
                continue;
            };
            let entry = &registry.entries[entry_index];
            let mut fill = |ptr: *mut u8| {
                for k in from..end {
                    let l = line(k);
                    if l.path.is_empty() && l.value == "{}" {
                        continue;
                    }
                    let mut refs = SceneRefs {
                        index: &index,
                        entities: &entities,
                        chain: &chains[l.chain as usize],
                        warning: None,
                        assets: &mut assets,
                    };
                    let path = l.path;
                    let value = l.value;
                    let result = if path.is_empty() && !whole_value(entry.info) {
                        Err(format!(
                            "expected `{component}.<field> = value` or `{component} = {{}}`"
                        ))
                    } else {
                        text::segments_into(path, &mut segs).and_then(|()| {
                            // SAFETY: `ptr` is a valid value of the component, described
                            // by its registered type.
                            let target =
                                unsafe { text::resolve(&segs, entry.info, ptr, &mut refs) }?;
                            match target {
                                None => Ok(Some(format!("`{component}` has no field `{path}`"))),
                                Some((ty, attrs, at)) => {
                                    // SAFETY: `at` is a valid, writable value of `ty`.
                                    unsafe { text::set_text(ty, &attrs, at, value, &mut refs) }
                                        .map(|()| None)
                                }
                            }
                        })
                    };
                    // A prefab's line is named by its file and line.
                    let named = |m: String| match l.from {
                        Some((f, line)) => format!("{}:{line}: {m}", files[f as usize]),
                        None => m,
                    };
                    if let Some(w) = refs.warning.take() {
                        warnings.push((l.at, named(w)));
                    }
                    match result {
                        Ok(Some(w)) => warnings.push((l.at, named(w))),
                        Ok(None) => {}
                        // The scene's own bad value fails the load; a prefab's is its own
                        // problem, said.
                        Err(m) if l.from.is_some() => warnings.push((l.at, named(m))),
                        Err(m) => {
                            if first_error.is_none() {
                                first_error = Some(err(l.at, m));
                            }
                        }
                    }
                }
            };
            match entry.access {
                Access::Typed { insert, .. } => insert(world, e, &mut fill),
                Access::Listed { id, default } => {
                    let id = id(world);
                    if scratches[entry_index].is_none() {
                        scratches[entry_index] = Scratch::new(entry.info);
                    }
                    if let Some(scratch) = &scratches[entry_index] {
                        // SAFETY: the scratch is room for one value of the type; its default
                        // makes it valid, reading keeps it valid, and `insert_raw` moves it
                        // into the world (the entity was just spawned), leaving the room
                        // free again.
                        unsafe {
                            default(scratch.ptr);
                            fill(scratch.ptr);
                            world.insert_raw(e, id, scratch.ptr);
                        }
                    }
                }
            }
            for companion in &entry.companions {
                companion(world, e);
            }
            start = end;
        }
        let Some(more) = &p.more else {
            continue;
        };
        // Components an override takes away.
        for &(line, component) in &more.removed {
            match registry.by_name.get(component.trim()) {
                Some(&k) => match registry.entries[k].access {
                    Access::Typed { remove, .. } => remove(world, e),
                    Access::Listed { id, .. } => {
                        let id = id(world);
                        world.remove_raw(e, id);
                    }
                },
                None => warnings.push((line, format!("unknown component `{component}`"))),
            }
        }
        if let Some(instance) = more.instance {
            world.insert(e, instance);
            if p.own && !(more.kept.lines.is_empty() && more.kept.blocks.is_empty()) {
                world.insert(e, more.kept.clone());
            }
        }
        if let Some(part) = more.part {
            world.insert(e, part);
        }
    }
    if let Some(e) = first_error {
        for &entity in &spawned {
            world.despawn(entity);
        }
        return Err(e);
    }
    for (i, p) in planned.iter().enumerate() {
        let (Some(child), Some((line, parent))) = (entities[i], p.parent) else {
            continue;
        };
        let Some(parent) = index.get(&parent).and_then(|&k| entities[k]) else {
            continue;
        };
        if let Err(e) = world.set_parent(child, Some(parent)) {
            for &entity in &spawned {
                world.despawn(entity);
            }
            return Err(err(line, e.to_string()));
        }
    }
    // The roots' order; a child's is meaningless.
    for (i, p) in planned.iter().enumerate() {
        let Some(e) = entities[i] else {
            continue;
        };
        match (p.order, p.parent) {
            (Some((_, n)), None) => {
                world.insert(e, RootOrder(n));
            }
            (Some((line, _)), Some(_)) if p.own => warnings.push((
                line,
                String::from("`order` places roots; this entity has a parent"),
            )),
            _ => {}
        }
    }
    Ok(LoadReport {
        entities: spawned,
        warnings,
        id,
        version,
        assets,
        prefabs,
    })
}

/// What [`read_value`] found besides the value.
#[derive(Debug, Default)]
pub struct ValueReport {
    /// The format version the file was written in.
    pub version: u32,
    /// The file's own id, from its header.
    pub id: Option<AssetId>,
    /// Lines that were skipped (fields the type no longer has), with their line numbers.
    pub warnings: Vec<(usize, String)>,
    /// The paths the file's asset references were written with.
    pub assets: AssetPaths,
}

/// Writes a file holding one reflected struct (a material, settings; ADR 0108): the
/// header `[<kind> version=<version> id=<id>]`, then one line per value, as a scene
/// writes a component's but without its name (`base_color = [0.8, 0.1, 0.1, 1]`).
pub fn write_value<T: Reflect>(
    kind: &str,
    version: u32,
    id: Option<AssetId>,
    value: &T,
    assets: Option<&AssetPaths>,
    out: &mut String,
) {
    match id {
        Some(id) => {
            let _ = writeln!(out, "[{kind} version={version} id={id}]");
        }
        None => {
            let _ = writeln!(out, "[{kind} version={version}]");
        }
    }
    out.push('\n');
    let mut writer = Writer {
        out,
        path: String::new(),
        refs: Refs::Ids(&no_ids),
        assets,
    };
    // SAFETY: `value` is a live `T`, which its description lays out.
    unsafe { writer.value(T::TYPE_INFO, (value as *const T).cast::<u8>()) };
}

/// How a file of one value names assets; it names no entities.
struct ValueRefs<'a> {
    assets: &'a mut AssetPaths,
}

impl ReadRefs for ValueRefs<'_> {
    fn entity(&mut self, label: &str) -> Result<Entity, String> {
        Err(format!("`@{label}`: a file of one value names no entities"))
    }

    fn asset(&mut self, asset: AssetRef, path: &str) {
        self.assets.insert(asset, path);
    }
}

/// Reads a file [`write_value`] wrote into `value`: fields the file does not mention keep
/// theirs; fields the type no longer has are warnings.
///
/// # Errors
///
/// A header of another kind or a newer version, or a bad line or value, with its line. On
/// error `value` may be partly read: read into a fresh value.
pub fn read_value<T: Reflect>(
    kind: &str,
    version: u32,
    text: &str,
    value: &mut T,
) -> Result<ValueReport, LoadError> {
    let mut assets = AssetPaths::new();
    let mut warnings = Vec::new();
    let mut head = None;
    let mut segs: Vec<Seg<'_>> = Vec::new();
    let ptr = (value as *mut T).cast::<u8>();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        if head.is_none() {
            head = Some(header(kind, version, line, s)?);
            continue;
        }
        let (path, value_text) =
            text::split_assignment(s).ok_or_else(|| err(line, "expected `field = value`"))?;
        text::segments_into(path, &mut segs).map_err(|m| err(line, m))?;
        let mut refs = ValueRefs {
            assets: &mut assets,
        };
        // SAFETY: `ptr` is the caller's live `T`, described by its type.
        let target = unsafe { text::resolve(&segs, T::TYPE_INFO, ptr, &mut refs) }
            .map_err(|m| err(line, m))?;
        match target {
            None => warnings.push((
                line,
                format!("`{}` has no field `{path}`", T::TYPE_INFO.name),
            )),
            Some((ty, attrs, at)) => {
                // SAFETY: `at` is a valid, writable value of `ty`.
                unsafe { text::set_text(ty, &attrs, at, value_text, &mut refs) }
                    .map_err(|m| err(line, m))?;
            }
        }
    }
    let (version, id) = head.ok_or_else(|| {
        err(
            0,
            format!("empty file: expected the header `[{kind} version={version}]`"),
        )
    })?;
    Ok(ValueReport {
        version,
        id,
        warnings,
        assets,
    })
}

/// The lines a scene writes for the value at `ptr` (type `info`), without a component's
/// name: `translation = (0, 0, 0)`, or ` = "Door"` for a value written whole. What the
/// editor shows of a component before its inspector has widgets for every kind. Entity
/// references are written `@none`; asset references with their paths from `assets`.
///
/// # Safety
///
/// `ptr` points to a valid value of `info`.
pub unsafe fn write_lines(
    info: &'static TypeInfo,
    ptr: *const u8,
    assets: Option<&AssetPaths>,
    out: &mut String,
) {
    // SAFETY: as the caller guarantees.
    unsafe { write_lines_with(info, ptr, &no_ids, assets, out) };
}

/// [`write_lines`] with entity references written by the scene ids `ids` gives them
/// (`@<id>`, or `@none`): what undo keeps of a component that is not plain data, read back
/// by [`read_lines`].
///
/// # Safety
///
/// `ptr` points to a valid value of `info`.
pub unsafe fn write_lines_with(
    info: &'static TypeInfo,
    ptr: *const u8,
    ids: &dyn Fn(Entity) -> Option<SceneId>,
    assets: Option<&AssetPaths>,
    out: &mut String,
) {
    let mut writer = Writer {
        out,
        path: String::new(),
        refs: Refs::Ids(ids),
        assets,
    };
    // SAFETY: as the caller guarantees.
    unsafe { writer.value(info, ptr) };
}

/// Reads lines [`write_lines`] or [`write_lines_with`] wrote into the value at `ptr` (type
/// `info`); entity references go through `refs`. Fields the lines leave out keep theirs.
/// Lines naming fields the type no longer has are skipped and returned as warnings, with
/// their line numbers (1-based).
///
/// # Errors
///
/// A bad line or value, with its line. The value stays valid, read up to that line.
///
/// # Safety
///
/// `ptr` points to a valid, writable value of `info`.
pub unsafe fn read_lines(
    info: &'static TypeInfo,
    ptr: *mut u8,
    text: &str,
    refs: &mut dyn ReadRefs,
) -> Result<Vec<(usize, String)>, LoadError> {
    let mut warnings = Vec::new();
    let mut segs: Vec<Seg<'_>> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let (path, value) =
            text::split_assignment(s).ok_or_else(|| err(line, "expected `field = value`"))?;
        text::segments_into(path, &mut segs).map_err(|m| err(line, m))?;
        // SAFETY: `ptr` is the caller's valid value of `info`.
        let target = unsafe { text::resolve(&segs, info, ptr, refs) }.map_err(|m| err(line, m))?;
        match target {
            None => warnings.push((line, format!("`{}` has no field `{path}`", info.name))),
            Some((ty, attrs, at)) => {
                // SAFETY: `at` is a valid, writable value of `ty`.
                unsafe { text::set_text(ty, &attrs, at, value, refs) }.map_err(|m| err(line, m))?;
            }
        }
    }
    Ok(warnings)
}
