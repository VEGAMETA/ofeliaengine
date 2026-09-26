//! Text scenes (ADR 0029): a line-oriented format for entities and their reflected
//! components, built to diff and merge well in version control.
//!
//! ```text
//! [scene version=1]
//!
//! [entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
//! Transform.rotation = (0, 0, 0, 1)
//! Transform.translation = (10, 0, 0)
//! Transform.scale = (1, 1, 1)
//!
//! [entity 0b12e4f8c3a94d5e9f7a6b1c2d3e4f50]
//! parent = 6c1f0a3e9d2b47c88e51f0d4a7b3c912
//! Transform.translation = (1, -0.5, 0)
//! Health.current = 80
//! ```
//!
//! - One block per entity, headed by its stable [`SceneId`] (128-bit, hex).
//! - `parent = <id>` links the hierarchy. Children are written after their parent, in
//!   the parent's child order, so the order survives a save and load.
//! - One line per field: `Component.path = value`.
//!   - Components are sorted by name; fields keep declaration order.
//!   - Vector-like structs (all `f32` fields named from `x y z w`) are written as
//!     tuples `(1, 2, 3)`; arrays of numbers as `[1, 2, 3]`.
//!   - Other nested structs use dotted paths.
//!   - Enums are written by variant name. Entity references are written as `@<id>` or
//!     `@none`.
//! - A component with no saved fields is written as `Component = {}`.
//!
//! Loading is tolerant of schema changes:
//! - fields missing from the file keep the component's `Default`;
//! - fields the type no longer has are reported as warnings, not errors.
//!
//! Unknown components are errors, reported with their line number.
//!
//! ```
//! use engine_ecs::{GlobalTransform, Transform, World};
//! use engine_scene::{SceneId, SceneRegistry, load, save};
//!
//! let registry = SceneRegistry::with_engine_types();
//! let mut world = World::new();
//! let car = world.spawn((SceneId::new(), Transform::from_xyz(10.0, 0.0, 0.0), GlobalTransform::IDENTITY));
//! let mut text = String::new();
//! save(&mut world, &registry, &mut text).unwrap();
//!
//! let mut copy = World::new();
//! let report = load(&mut copy, &registry, &text).unwrap();
//! let loaded = report.entities[0];
//! assert_eq!(copy.get::<Transform>(loaded), world.get::<Transform>(car));
//! assert!(copy.get::<GlobalTransform>(loaded).is_some(), "companion added");
//! ```

use engine_ecs::{Component, Entity, GlobalTransform, QueryState, Transform, World, bytemuck};
use engine_reflect::{Attrs, Primitive, Reflect, TypeInfo, TypeKind, Value};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Current format version, written in the header.
pub const VERSION: u32 = 1;

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
        Self {
            hi: half(),
            lo: half(),
        }
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

    fn parse(s: &str) -> Option<Self> {
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
    /// [`GlobalTransform`]).
    pub fn with_engine_types() -> Self {
        let mut r = Self::new();
        r.register::<Transform>();
        r.require::<Transform, GlobalTransform>();
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
    let _ = writeln!(out, "[scene version={VERSION}]");
    let mut count = 0;
    let mut stack: Vec<Entity> = roots.iter().rev().map(|&(_, e)| e).collect();
    let mut children: Vec<Entity> = Vec::new();
    while let Some(e) = stack.pop() {
        write_entity(world, registry, &ids, e, out);
        count += 1;
        children.clear();
        children.extend(world.children(e).filter(|c| ids.contains_key(c)));
        stack.extend(children.iter().rev());
    }
    Ok(count)
}

fn write_entity(
    world: &World,
    registry: &SceneRegistry,
    ids: &HashMap<Entity, SceneId>,
    e: Entity,
    out: &mut String,
) {
    let _ = writeln!(out, "\n[entity {}]", ids[&e]);
    if let Some(p) = world.parent(e).and_then(|p| ids.get(&p)) {
        let _ = writeln!(out, "parent = {p}");
    }
    for entry in &registry.entries {
        let Some(ptr) = (entry.get)(world, e) else {
            continue;
        };
        let mut path = String::from(entry.info.name);
        let before = out.len();
        // SAFETY: `ptr` points to a live value of the registered type.
        unsafe { write_fields(out, &mut path, entry.info, ptr, ids) };
        if out.len() == before {
            let _ = writeln!(out, "{} = {{}}", entry.info.name);
        }
    }
}

/// Entity references are recognized by name and layout: `&'static` constants are not
/// guaranteed to have one address across crates, so pointer comparison is unreliable.
fn is_entity(ty: &TypeInfo) -> bool {
    let e = <Entity as Reflect>::TYPE_INFO;
    ty.name == e.name && ty.size == e.size && ty.align == e.align
}

fn is_vector_like(ty: &TypeInfo) -> Option<&'static [engine_reflect::Field]> {
    let TypeKind::Struct(fields) = ty.kind else {
        return None;
    };
    let ok = !fields.is_empty()
        && fields.len() <= 4
        && fields
            .iter()
            .zip(["x", "y", "z", "w"])
            .all(|(f, n)| f.name == n && matches!(f.ty.kind, TypeKind::Primitive(Primitive::F32)));
    ok.then_some(fields)
}

fn primitive_of(ty: &TypeInfo) -> Option<Primitive> {
    match ty.kind {
        TypeKind::Primitive(p) => Some(p),
        _ => None,
    }
}

/// Writes one line per field of the value at `ptr` (type `ty`) under `path`.
///
/// # Safety
///
/// `ptr` points to a valid value of `ty`.
unsafe fn write_fields(
    out: &mut String,
    path: &mut String,
    ty: &'static TypeInfo,
    ptr: *const u8,
    ids: &HashMap<Entity, SceneId>,
) {
    if let TypeKind::Struct(fields) = ty.kind
        && !is_entity(ty)
        && is_vector_like(ty).is_none()
    {
        for f in fields {
            if f.attrs.skip {
                continue;
            }
            let len = path.len();
            path.push('.');
            path.push_str(f.name);
            // SAFETY: the field lies inside the value at its offset.
            unsafe { write_fields(out, path, f.ty, ptr.add(f.offset), ids) };
            path.truncate(len);
        }
        return;
    }
    if let TypeKind::Array { element, len } = ty.kind
        && primitive_of(element).is_none()
    {
        for i in 0..len {
            let old = path.len();
            let _ = write!(path, ".{i}");
            // SAFETY: element `i` lies inside the array.
            unsafe { write_fields(out, path, element, ptr.add(i * element.size), ids) };
            path.truncate(old);
        }
        return;
    }
    let mark = out.len();
    let _ = write!(out, "{path} = ");
    // SAFETY: as the caller guarantees.
    if unsafe { write_value(out, ty, ptr, ids) } {
        out.push('\n');
    } else {
        out.truncate(mark);
    }
}

/// Writes a single value (a leaf, a tuple or a list). False for opaque values.
///
/// # Safety
///
/// `ptr` points to a valid value of `ty`.
unsafe fn write_value(
    out: &mut String,
    ty: &'static TypeInfo,
    ptr: *const u8,
    ids: &HashMap<Entity, SceneId>,
) -> bool {
    if is_entity(ty) {
        // SAFETY: an `Entity` value.
        let e = unsafe { ptr.cast::<Entity>().read_unaligned() };
        match ids.get(&e) {
            Some(id) => {
                let _ = write!(out, "@{id}");
            }
            None => out.push_str("@none"),
        }
        return true;
    }
    if let Some(fields) = is_vector_like(ty) {
        out.push('(');
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            // SAFETY: an `f32` field inside the value.
            unsafe { write_primitive(out, f.ty, ptr.add(f.offset)) };
        }
        out.push(')');
        return true;
    }
    match ty.kind {
        TypeKind::Array { element, len } => {
            out.push('[');
            for i in 0..len {
                if i > 0 {
                    out.push_str(", ");
                }
                // SAFETY: a primitive element inside the array.
                unsafe { write_primitive(out, element, ptr.add(i * element.size)) };
            }
            out.push(']');
            true
        }
        TypeKind::Primitive(_) | TypeKind::Enum { .. } => {
            // SAFETY: as the caller guarantees.
            unsafe { write_primitive(out, ty, ptr) };
            true
        }
        TypeKind::Struct(_) | TypeKind::Opaque => false,
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

fn err(line: usize, message: impl Into<String>) -> LoadError {
    LoadError {
        line,
        message: message.into(),
    }
}

fn parse(text: &str) -> Result<Vec<Block<'_>>, LoadError> {
    let mut blocks: Vec<Block<'_>> = Vec::new();
    let mut header = false;
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        if !header {
            let version = s
                .strip_prefix("[scene version=")
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|v| v.parse::<u32>().ok())
                .ok_or_else(|| err(line, "expected the header `[scene version=1]`"))?;
            if version > VERSION {
                return Err(err(
                    line,
                    format!("scene version {version} is newer than this engine ({VERSION})"),
                ));
            }
            header = true;
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
        let (key, value) = s
            .split_once('=')
            .ok_or_else(|| err(line, "expected `key = value`"))?;
        let (key, value) = (key.trim(), value.trim());
        if key == "parent" {
            let id = SceneId::parse(value).ok_or_else(|| err(line, "expected a scene id"))?;
            block.parent = Some((line, id));
            continue;
        }
        let (component, path) = key.split_once('.').unwrap_or((key, ""));
        block.fields.push((line, component, path, value));
    }
    if !header {
        return Err(err(
            0,
            "empty file: expected the header `[scene version=1]`",
        ));
    }
    Ok(blocks)
}

/// Loads a scene into `world`: spawns its entities (with their [`SceneId`]s), their
/// components and the hierarchy. On error nothing is kept.
pub fn load(
    world: &mut World,
    registry: &SceneRegistry,
    text: &str,
) -> Result<LoadReport, LoadError> {
    engine_profiler::zone!("scene::load");
    let blocks = parse(text)?;
    let mut by_id: HashMap<SceneId, usize> = HashMap::with_capacity(blocks.len());
    for (i, b) in blocks.iter().enumerate() {
        if by_id.insert(b.id, i).is_some() {
            return Err(err(b.line, format!("scene id {} appears twice", b.id)));
        }
    }
    // Validate everything that can fail before touching the world.
    for b in &blocks {
        if let Some((line, p)) = b.parent
            && !by_id.contains_key(&p)
        {
            return Err(err(line, format!("parent {p} is not in this scene")));
        }
        for &(line, component, path, value) in &b.fields {
            if !registry.by_name.contains_key(component) {
                return Err(err(line, format!("unknown component `{component}`")));
            }
            if path.is_empty() && value != "{}" {
                return Err(err(
                    line,
                    format!("expected `{component}.<field> = value` or `{component} = {{}}`"),
                ));
            }
        }
    }
    let entities: Vec<Entity> = blocks.iter().map(|b| world.spawn((b.id,))).collect();
    let mut report = LoadReport {
        entities,
        warnings: Vec::new(),
    };
    let mut resolve = |id: SceneId| by_id.get(&id).map(|&i| report.entities[i]);
    let mut warnings: Vec<(usize, String)> = Vec::new();
    let mut first_error: Option<LoadError> = None;
    for (bi, b) in blocks.iter().enumerate() {
        let e = report.entities[bi];
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
                for &(line, _, path, value) in lines {
                    if path.is_empty() {
                        continue;
                    }
                    let Some((offset, ty, attrs)) = entry.info.path(path) else {
                        warnings.push((line, format!("`{component}` has no field `{path}`")));
                        continue;
                    };
                    // SAFETY: `ptr` is a valid value of the component; the path's offset
                    // and type come from its description.
                    let result =
                        unsafe { read_value(ptr.add(offset), ty, &attrs, value, &mut resolve) };
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
        for &entity in &report.entities {
            world.despawn(entity);
        }
        return Err(e);
    }
    for (bi, b) in blocks.iter().enumerate() {
        if let Some((line, p)) = b.parent {
            let child = report.entities[bi];
            let parent = report.entities[by_id[&p]];
            if let Err(e) = world.set_parent(child, Some(parent)) {
                for &entity in &report.entities {
                    world.despawn(entity);
                }
                return Err(err(line, e.to_string()));
            }
        }
    }
    report.warnings = warnings;
    Ok(report)
}

/// Parses `text` into the value at `ptr`. `Ok(Some(warning))` for recoverable problems.
///
/// # Safety
///
/// `ptr` points to a valid, writable value of `ty`.
unsafe fn read_value(
    ptr: *mut u8,
    ty: &'static TypeInfo,
    attrs: &Attrs,
    text: &str,
    resolve: &mut impl FnMut(SceneId) -> Option<Entity>,
) -> Result<Option<String>, String> {
    if is_entity(ty) {
        let target = match text {
            "@none" => Entity::NULL,
            _ => {
                let id = text
                    .strip_prefix('@')
                    .and_then(SceneId::parse)
                    .ok_or_else(|| format!("expected `@<scene id>` or `@none`, found `{text}`"))?;
                match resolve(id) {
                    Some(e) => e,
                    None => {
                        // SAFETY: an `Entity` value.
                        unsafe { ptr.cast::<Entity>().write_unaligned(Entity::NULL) };
                        return Ok(Some(format!("entity {id} is not in this scene")));
                    }
                }
            }
        };
        // SAFETY: an `Entity` value.
        unsafe { ptr.cast::<Entity>().write_unaligned(target) };
        return Ok(None);
    }
    if let Some(fields) = is_vector_like(ty) {
        let items = list(text, '(', ')')?;
        if items.len() != fields.len() {
            return Err(format!(
                "expected {} numbers, found {}",
                fields.len(),
                items.len()
            ));
        }
        for (f, item) in fields.iter().zip(items) {
            // SAFETY: an `f32` field inside the value.
            unsafe { read_primitive(ptr.add(f.offset), f.ty, &f.attrs, item)? };
        }
        return Ok(None);
    }
    match ty.kind {
        TypeKind::Array { element, len } if primitive_of(element).is_some() => {
            let items = list(text, '[', ']')?;
            if items.len() != len {
                return Err(format!("expected {len} values, found {}", items.len()));
            }
            for (i, item) in items.into_iter().enumerate() {
                // SAFETY: element `i` inside the array.
                unsafe {
                    read_primitive(ptr.add(i * element.size), element, &element.attrs, item)?
                };
            }
            Ok(None)
        }
        TypeKind::Primitive(_) | TypeKind::Enum { .. } => {
            // SAFETY: as the caller guarantees.
            unsafe { read_primitive(ptr, ty, attrs, text)? };
            Ok(None)
        }
        _ => Err(format!(
            "`{}` is not a single value; name one of its fields",
            ty.name
        )),
    }
}

fn list(text: &str, open: char, close: char) -> Result<Vec<&str>, String> {
    let inner = text
        .strip_prefix(open)
        .and_then(|t| t.strip_suffix(close))
        .ok_or_else(|| format!("expected `{open}...{close}`, found `{text}`"))?;
    Ok(inner
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect())
}

/// # Safety
///
/// `ptr` points to a valid, writable primitive or enum of `ty`.
unsafe fn read_primitive(
    ptr: *mut u8,
    ty: &TypeInfo,
    attrs: &Attrs,
    text: &str,
) -> Result<(), String> {
    let value = match ty.kind {
        TypeKind::Enum { variants, .. } => match variants.iter().find(|v| v.name == text) {
            Some(v) => Value::Int(v.value),
            None => Value::Int(
                text.parse()
                    .map_err(|_| format!("`{text}` is not a variant of `{}`", ty.name))?,
            ),
        },
        TypeKind::Primitive(Primitive::Bool) => match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(format!("expected `true` or `false`, found `{text}`")),
        },
        // Parsed at their own width, so written values round-trip bit for bit.
        TypeKind::Primitive(Primitive::F32) => Value::Float(f64::from(
            text.parse::<f32>()
                .map_err(|_| format!("expected a number, found `{text}`"))?,
        )),
        TypeKind::Primitive(Primitive::F64) => Value::Float(
            text.parse::<f64>()
                .map_err(|_| format!("expected a number, found `{text}`"))?,
        ),
        TypeKind::Primitive(Primitive::U8 | Primitive::U16 | Primitive::U32 | Primitive::U64) => {
            Value::UInt(
                text.parse()
                    .map_err(|_| format!("expected an unsigned integer, found `{text}`"))?,
            )
        }
        TypeKind::Primitive(_) => Value::Int(
            text.parse()
                .map_err(|_| format!("expected an integer, found `{text}`"))?,
        ),
        _ => return Err(format!("`{}` is not a single value", ty.name)),
    };
    // SAFETY: as the caller guarantees.
    unsafe { engine_reflect::write(ptr, ty, attrs, value) }.map_err(|e| e.to_string())
}
