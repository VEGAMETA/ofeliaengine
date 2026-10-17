//! Text scenes (ADR 0029; format 2: ADR 0108): a line-oriented format for entities and
//! their reflected components, built to diff and merge well in version control.
//!
//! ```text
//! [scene version=2 id=9b41d07c2e8a4f6b8d3c1a5e7f902b64]
//!
//! [entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
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
//! - One line per value: `Component.path = value` ([`text`] has the grammar, shared with
//!   saves).
//!   - Components are sorted by name; fields keep declaration order.
//!   - Lists take indices, maps keys in brackets; lists and arrays of single values are
//!     written on one line (`[a, b]`).
//!   - A struct of one unnamed field is written as that field (`Health = 80`).
//!   - Asset references are written with their path, then the file's id ([`AssetRef`]).
//! - A component with no saved fields is written as `Component = {}`.
//!
//! Loading is tolerant of schema changes:
//! - fields missing from the file keep the component's `Default`;
//! - fields the type no longer has are reported as warnings, not errors.
//!
//! Unknown components are errors, reported with their line number. Files of format 1 load
//! as they are.
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
pub mod text;

pub use assets::{AssetId, AssetPaths, AssetRef};

use engine_ecs::{Component, Entity, GlobalTransform, QueryState, Transform, World, bytemuck};
use engine_reflect::{Primitive, Reflect, TypeInfo, TypeKind, Value};
use std::collections::HashMap;
use std::fmt::Write as _;
use text::{ReadRefs, Seg, is_asset_ref, is_entity, is_single, newtype_field, vector_fields};

/// Current format version, written in the header.
pub const VERSION: u32 = 2;

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
}

impl Default for SceneId {
    fn default() -> Self {
        Self::new()
    }
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

type Fill<'a> = dyn FnMut(*mut u8) + 'a;

struct Entry {
    info: &'static TypeInfo,
    type_name: fn() -> &'static str,
    get: fn(&World, Entity) -> Option<*const u8>,
    insert: fn(&mut World, Entity, &mut Fill<'_>),
    companions: Vec<fn(&mut World, Entity)>,
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

    /// Makes `T` savable and loadable under its reflected name. Missing fields load as
    /// `T::default()`'s.
    ///
    /// # Panics
    ///
    /// If another type is registered under the same name.
    pub fn register<T: Component + Reflect + Default>(&mut self) -> &mut Self {
        let info = <T as Reflect>::TYPE_INFO;
        if let Some(&i) = self.by_name.get(info.name) {
            assert!(
                self.entries[i].info.size == info.size
                    && core::any::type_name::<T>() == (self.entries[i].type_name)(),
                "two scene types named `{}`",
                info.name
            );
            return self;
        }
        self.entries.push(Entry {
            info,
            type_name: core::any::type_name::<T>,
            get: |w, e| w.get::<T>(e).map(|r| (r as *const T).cast::<u8>()),
            insert: |w, e, fill| {
                let mut value = T::default();
                fill((&raw mut value).cast::<u8>());
                w.insert(e, value);
            },
            companions: Vec::new(),
        });
        self.entries.sort_by_key(|e| e.info.name);
        self.by_name = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.info.name, i))
            .collect();
        self
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
}

/// How [`save_with`] writes a scene.
#[derive(Debug, Clone, Copy, Default)]
pub struct SaveOptions<'a> {
    /// The scene's own id, written in the header.
    pub id: Option<AssetId>,
    /// The paths asset references are written with. A reference without one is written
    /// with its id alone (and its part's hash).
    pub assets: Option<&'a AssetPaths>,
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

/// [`save`] with the scene's id and the paths of its asset references.
pub fn save_with(
    world: &mut World,
    registry: &SceneRegistry,
    options: &SaveOptions<'_>,
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
    for (&e, &id) in &ids {
        if world.parent(e).is_none_or(|p| !ids.contains_key(&p)) {
            roots.push((id, e));
        }
    }
    roots.sort_unstable();
    match options.id {
        Some(id) => {
            let _ = writeln!(out, "[scene version={VERSION} id={id}]");
        }
        None => {
            let _ = writeln!(out, "[scene version={VERSION}]");
        }
    }
    let mut writer = Writer {
        out,
        path: String::new(),
        ids: &ids,
        assets: options.assets,
    };
    let mut count = 0;
    let mut stack: Vec<Entity> = roots.iter().rev().map(|&(_, e)| e).collect();
    let mut children: Vec<Entity> = Vec::new();
    while let Some(e) = stack.pop() {
        writer.entity(world, registry, e);
        count += 1;
        children.clear();
        children.extend(world.children(e).filter(|c| ids.contains_key(c)));
        stack.extend(children.iter().rev());
    }
    Ok(count)
}

/// Writes the lines of a scene.
struct Writer<'a> {
    out: &'a mut String,
    /// The path of the value being written, `Component.field…`.
    path: String,
    ids: &'a HashMap<Entity, SceneId>,
    assets: Option<&'a AssetPaths>,
}

impl Writer<'_> {
    fn entity(&mut self, world: &World, registry: &SceneRegistry, e: Entity) {
        let _ = writeln!(self.out, "\n[entity {}]", self.ids[&e]);
        if let Some(p) = world.parent(e).and_then(|p| self.ids.get(&p)) {
            let _ = writeln!(self.out, "parent = {p}");
        }
        for entry in &registry.entries {
            let Some(ptr) = (entry.get)(world, e) else {
                continue;
            };
            self.path.clear();
            self.path.push_str(entry.info.name);
            let before = self.out.len();
            // SAFETY: `ptr` points to a live value of the registered type.
            unsafe { self.value(entry.info, ptr) };
            if self.out.len() == before {
                let _ = writeln!(self.out, "{} = {{}}", entry.info.name);
            }
        }
    }

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
                single(self.out, ty, ptr, self.ids, self.assets);
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
                        self.path.push('.');
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
                        single(&mut key, map.key, k, self.ids, self.assets);
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
            unsafe { single(self.out, ty, first.add(i * ty.size), self.ids, self.assets) };
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
    ids: &HashMap<Entity, SceneId>,
    assets: Option<&AssetPaths>,
) {
    // SAFETY: for the block, as the caller guarantees.
    unsafe {
        if is_entity(ty) {
            let e = ptr.cast::<Entity>().read_unaligned();
            match ids.get(&e) {
                Some(id) => {
                    let _ = write!(out, "@{id}");
                }
                None => out.push_str("@none"),
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
            single(out, f.ty, ptr.add(f.offset), ids, assets);
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

struct Block<'a> {
    line: usize,
    id: SceneId,
    parent: Option<(usize, SceneId)>,
    /// (line, component, path after the component, value)
    fields: Vec<(usize, &'a str, &'a str, &'a str)>,
}

/// A parsed file: its header and its blocks.
struct Parsed<'a> {
    version: u32,
    id: Option<AssetId>,
    blocks: Vec<Block<'a>>,
}

fn err(line: usize, message: impl Into<String>) -> LoadError {
    LoadError {
        line,
        message: message.into(),
    }
}

/// The header's version and id: `[scene version=2 id=<32 hex digits>]`.
fn header(line: usize, s: &str) -> Result<(u32, Option<AssetId>), LoadError> {
    let expected = || {
        err(
            line,
            format!("expected the header `[scene version={VERSION}]`"),
        )
    };
    let inner = s
        .strip_prefix("[scene ")
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
    if version > VERSION {
        return Err(err(
            line,
            format!("scene version {version} is newer than this engine ({VERSION})"),
        ));
    }
    Ok((version, id))
}

fn parse(text: &str) -> Result<Parsed<'_>, LoadError> {
    let mut blocks: Vec<Block<'_>> = Vec::new();
    let mut head = None;
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        if head.is_none() {
            head = Some(header(line, s)?);
            continue;
        }
        if let Some(rest) = s.strip_prefix("[entity ") {
            let id = rest
                .strip_suffix(']')
                .and_then(|h| SceneId::parse(h.trim()))
                .ok_or_else(|| err(line, "expected `[entity <32 hex digits>]`"))?;
            blocks.push(Block {
                line,
                id,
                parent: None,
                fields: Vec::new(),
            });
            continue;
        }
        let block = blocks
            .last_mut()
            .ok_or_else(|| err(line, "a field before the first `[entity ...]`"))?;
        let (key, value) =
            text::split_assignment(s).ok_or_else(|| err(line, "expected `key = value`"))?;
        if key == "parent" {
            let id = SceneId::parse(value).ok_or_else(|| err(line, "expected a scene id"))?;
            block.parent = Some((line, id));
            continue;
        }
        let (component, path) = text::component_of(key);
        block.fields.push((line, component, path, value));
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
    by_id: &'a HashMap<SceneId, usize>,
    entities: &'a [Entity],
    /// An entity reference not in the scene, on the current line.
    warning: Option<String>,
    assets: &'a mut AssetPaths,
}

impl ReadRefs for SceneRefs<'_> {
    fn entity(&mut self, label: &str) -> Result<Entity, String> {
        let id = SceneId::parse(label)
            .ok_or_else(|| format!("expected `@<scene id>` or `@none`, found `@{label}`"))?;
        match self.by_id.get(&id) {
            Some(&i) => Ok(self.entities[i]),
            None => {
                self.warning = Some(format!("entity {id} is not in this scene"));
                Ok(Entity::NULL)
            }
        }
    }

    fn asset(&mut self, asset: AssetRef, path: &str) {
        self.assets.insert(asset, path);
    }
}

/// Whether `Component = value` sets a whole component (one written as a single value).
fn whole_value(info: &TypeInfo) -> bool {
    is_single(info) || newtype_field(info).is_some()
}

/// Loads a scene into `world`: spawns its entities (with their [`SceneId`]s), their
/// components and the hierarchy. On error nothing is kept.
pub fn load(
    world: &mut World,
    registry: &SceneRegistry,
    text: &str,
) -> Result<LoadReport, LoadError> {
    engine_profiler::zone!("scene::load");
    let parsed = parse(text)?;
    let blocks = &parsed.blocks;
    let mut by_id: HashMap<SceneId, usize> = HashMap::with_capacity(blocks.len());
    for (i, b) in blocks.iter().enumerate() {
        if by_id.insert(b.id, i).is_some() {
            return Err(err(b.line, format!("scene id {} appears twice", b.id)));
        }
    }
    // Validate everything that can fail before touching the world.
    for b in blocks {
        if let Some((line, p)) = b.parent
            && !by_id.contains_key(&p)
        {
            return Err(err(line, format!("parent {p} is not in this scene")));
        }
        for &(line, component, path, value) in &b.fields {
            let Some(&entry) = registry.by_name.get(component) else {
                return Err(err(line, format!("unknown component `{component}`")));
            };
            if path.is_empty() && value != "{}" && !whole_value(registry.entries[entry].info) {
                return Err(err(
                    line,
                    format!("expected `{component}.<field> = value` or `{component} = {{}}`"),
                ));
            }
        }
    }
    let entities: Vec<Entity> = blocks.iter().map(|b| world.spawn((b.id,))).collect();
    let mut assets = AssetPaths::new();
    let mut warnings: Vec<(usize, String)> = Vec::new();
    let mut first_error: Option<LoadError> = None;
    let mut segs: Vec<Seg<'_>> = Vec::new();
    for (bi, b) in blocks.iter().enumerate() {
        let e = entities[bi];
        let mut start = 0;
        while start < b.fields.len() {
            let component = b.fields[start].1;
            let end = start
                + b.fields[start..]
                    .iter()
                    .take_while(|f| f.1 == component)
                    .count();
            let entry = &registry.entries[registry.by_name[component]];
            let lines = &b.fields[start..end];
            (entry.insert)(world, e, &mut |ptr| {
                let mut refs = SceneRefs {
                    by_id: &by_id,
                    entities: &entities,
                    warning: None,
                    assets: &mut assets,
                };
                for &(line, _, path, value) in lines {
                    if path.is_empty() && value == "{}" {
                        continue;
                    }
                    let result = text::segments_into(path, &mut segs).and_then(|()| {
                        // SAFETY: `ptr` is a valid value of the component, described by
                        // its registered type.
                        let target = unsafe { text::resolve(&segs, entry.info, ptr, &mut refs) }?;
                        match target {
                            None => Ok(Some(format!("`{component}` has no field `{path}`"))),
                            Some((ty, attrs, at)) => {
                                // SAFETY: `at` is a valid, writable value of `ty`.
                                unsafe { text::set_text(ty, &attrs, at, value, &mut refs) }
                                    .map(|()| None)
                            }
                        }
                    });
                    if let Some(w) = refs.warning.take() {
                        warnings.push((line, w));
                    }
                    match result {
                        Ok(Some(w)) => warnings.push((line, w)),
                        Ok(None) => {}
                        Err(m) => {
                            if first_error.is_none() {
                                first_error = Some(err(line, m));
                            }
                        }
                    }
                }
            });
            for companion in &entry.companions {
                companion(world, e);
            }
            start = end;
        }
    }
    if let Some(e) = first_error {
        for &entity in &entities {
            world.despawn(entity);
        }
        return Err(e);
    }
    for (bi, b) in blocks.iter().enumerate() {
        if let Some((line, p)) = b.parent {
            let child = entities[bi];
            let parent = entities[by_id[&p]];
            if let Err(e) = world.set_parent(child, Some(parent)) {
                for &entity in &entities {
                    world.despawn(entity);
                }
                return Err(err(line, e.to_string()));
            }
        }
    }
    Ok(LoadReport {
        entities,
        warnings,
        id: parsed.id,
        version: parsed.version,
        assets,
    })
}
