//! `self` and components' views on Luau's own API (ADR 0114 E7-1c): the script host's hot
//! path, where a field's read or write is a call from a script.
//!
//! - **Atoms.** Luau gives each string an atom when it makes it (`lua_Callbacks::useratom`):
//!   a name-like string takes the next number from the thread's table ([`atom_of`]), and a
//!   struct's fields are matched by their names' atoms ([`Slot`]), so finding a field
//!   compares small integers. A name made before the callback was set has none and is
//!   matched by its bytes.
//! - **Tags.** `self` ([`TAG_SELF`]) and views ([`TAG_VIEW`]) are userdata of tags of
//!   their own, made with their tag's metatable, whose metamethods are the C functions
//!   here: they read the userdata, the key's atom and the world directly. Numbers,
//!   booleans, vectors and entities are read and written there, entities' names and
//!   components, `world.find`; the rest (rotations, strings, enums, errors' wording) goes
//!   to the host's own functions ([`Slow`]), called from here with the field's place when
//!   it was found, so they read no key's text (a string read through mlua allocates).
//! - **Entities are values** (ADR 0114 §3): light userdata of tag [`ENTITY_TAG`] holding
//!   the handle's bits plus one ([`entity_value`]), never allocated, equal when their
//!   handles are, usable as table keys, `typeof` `Entity`. Their metatable is the VM's
//!   one for every light userdata (no other light userdata reaches scripts).
//! - **Methods** (ADR 0114 §3, E7-5a) are `__namecall`s, found by the method's atom: an
//!   entity's (`has`, `add`, `remove`, `despawn`, `children`, `find_child`) on entities and
//!   `self`, then the functions on `self`'s component's table (`self:open()` is
//!   `Door.open(self)`); `Transform`'s (`rotate`, `look_at`, `forward`, `right`, `up`) on
//!   its views. A component is named by its table (`Transform`, a script's `component`
//!   table: its `name`) or by its name.
//! - **Queries** (E7-5c) are userdata of [`TAG_QUERY`]: `for e, a, b in q do` takes a
//!   snapshot of what matches when the loop begins ([`query_iter`]); each step goes from
//!   the entity before ([`query_next`]) and moves the query's views, one a column, to the
//!   entity, so a loop makes nothing. A query collected lets go of its views at the next
//!   stage ([`take_dead_refs`]).
//! - **No allocation per access:** a view of `self`'s entity's component is made once and
//!   kept on `self` ([`ThisData::views`]), as is `self.locals`; a view through an entity
//!   (`e.Transform`) or of a struct inside a view is kept in a table of the latest
//!   ([`VIEW_CACHE`] places) and given again; `world.find`'s index follows the names that
//!   changed ([`NameIndex`]); entities, vectors and numbers are values.
//! - **Errors** are raised with `lua_error` once nothing with a destructor is alive in the
//!   frame, as mlua's own callbacks raise theirs.

use engine_core::math::{Quat, Vec3};
use engine_ecs::{Changed, ComponentId, Entity, Parent, QueryState, Transform, World};
use engine_reflect::{Primitive, TypeInfo, TypeKind};
use engine_scene::Name;
use engine_script::mlua::{self, ffi};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::hash::BuildHasherDefault;

/// `self`'s tag (mlua's own userdata take 0 and 1).
pub(crate) const TAG_SELF: c_int = 64;
/// A view's tag.
pub(crate) const TAG_VIEW: c_int = 65;
/// Entities' light userdata tag: 0, the one mlua keeps when a value passes through its own
/// functions (it pushes light userdata back with tag 0).
pub(crate) const ENTITY_TAG: c_int = 0;

/// Places in the table of views kept ([`Tables::kept`]): a power of two.
const VIEW_CACHE: usize = 256;

/// Views of its entity's components a `self` keeps.
const VIEWS: usize = 4;

/// The longest name given an atom.
const LONGEST_ATOM: usize = 64;

thread_local! {
    /// Names' atoms: the same numbers for every VM on this thread (a VM never leaves its
    /// thread: mlua's `Lua` is not `Send` here).
    static ATOMS: RefCell<HashMap<Box<str>, i16>> = RefCell::new(HashMap::new());
    /// The host's tables, open to the C functions for the length of a stage (null
    /// outside), as the world is.
    static OPEN_TABLES: Cell<*mut Tables> = const { Cell::new(core::ptr::null_mut()) };
}

/// The atom of name `name`, given now if it has none (-1 for what is not name-like, and
/// once the 32,767 atoms are taken).
pub(crate) fn atom_of(name: &str) -> i16 {
    let bytes = name.as_bytes();
    let name_like = !bytes.is_empty()
        && bytes.len() <= LONGEST_ATOM
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'_');
    if !name_like {
        return -1;
    }
    ATOMS.with(|atoms| {
        let Ok(mut atoms) = atoms.try_borrow_mut() else {
            return -1;
        };
        if let Some(&atom) = atoms.get(name) {
            return atom;
        }
        let Ok(next) = i16::try_from(atoms.len()) else {
            return -1;
        };
        atoms.insert(name.into(), next);
        next
    })
}

/// Luau's `useratom`: the atom of a string it makes.
///
/// # Safety
///
/// Luau calls it with a string's bytes and length.
unsafe extern "C-unwind" fn useratom(
    _state: *mut ffi::lua_State,
    s: *const c_char,
    len: usize,
) -> i16 {
    if s.is_null() || len > LONGEST_ATOM {
        return -1;
    }
    // SAFETY: Luau gives the string's `len` bytes.
    let bytes = unsafe { core::slice::from_raw_parts(s.cast::<u8>(), len) };
    core::str::from_utf8(bytes).map_or(-1, atom_of)
}

/// How the fast path reads and writes a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fast {
    F32,
    F64,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Bool,
    Vec3,
    Vec3A,
    Vec2,
    /// An entity: a value ([`ENTITY_TAG`]), nil for none.
    Entity,
    /// A struct inside: read as a view of it.
    Struct,
    /// Through the host's own functions.
    Slow,
}

fn fast_of(ty: &'static TypeInfo) -> Fast {
    match ty.name {
        "Vec3" => return Fast::Vec3,
        "Vec3A" => return Fast::Vec3A,
        "Vec2" => return Fast::Vec2,
        "Entity" => return Fast::Entity,
        "Quat" => return Fast::Slow,
        _ => {}
    }
    match &ty.kind {
        TypeKind::Primitive(p) => match p {
            Primitive::Bool => Fast::Bool,
            Primitive::I8 => Fast::I8,
            Primitive::U8 => Fast::U8,
            Primitive::I16 => Fast::I16,
            Primitive::U16 => Fast::U16,
            Primitive::I32 => Fast::I32,
            Primitive::U32 => Fast::U32,
            Primitive::I64 => Fast::I64,
            Primitive::U64 => Fast::U64,
            Primitive::F32 => Fast::F32,
            Primitive::F64 => Fast::F64,
        },
        TypeKind::Struct(_) => Fast::Struct,
        _ => Fast::Slow,
    }
}

/// A struct's field as the fast path finds it: its name's atom, where it lies, how it is
/// read, and the range a write is clamped to (as the inspector clamps).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Slot {
    atom: i16,
    name: &'static str,
    offset: u32,
    fast: Fast,
    ty: &'static TypeInfo,
    min: f64,
    max: f64,
}

/// A component scripts name, by its name's atom.
#[derive(Debug, Clone, Copy)]
struct Named {
    id: ComponentId,
    info: &'static TypeInfo,
    slots: *const [Slot],
}

/// The host's own functions the C functions hand the rest to, as registry references:
/// `self`'s `__index` and `__newindex`, a view's, an entity's `add`; and `children()`'s
/// step ([`children_next`]), made once.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Slow {
    pub(crate) self_index: c_int,
    pub(crate) self_newindex: c_int,
    pub(crate) view_index: c_int,
    pub(crate) view_newindex: c_int,
    pub(crate) add: c_int,
    pub(crate) children: c_int,
    pub(crate) query_next: c_int,
}

/// The atoms of an entity's `parent` and methods, and of `Transform`'s methods.
#[derive(Debug, Clone, Copy)]
struct Methods {
    parent: c_int,
    has: c_int,
    add: c_int,
    remove: c_int,
    despawn: c_int,
    children: c_int,
    find_child: c_int,
    rotate: c_int,
    look_at: c_int,
    forward: c_int,
    right: c_int,
    up: c_int,
    with: c_int,
    without: c_int,
}

impl Methods {
    fn new() -> Self {
        let a = |name| c_int::from(atom_of(name));
        Self {
            parent: a("parent"),
            has: a("has"),
            add: a("add"),
            remove: a("remove"),
            despawn: a("despawn"),
            children: a("children"),
            find_child: a("find_child"),
            rotate: a("rotate"),
            look_at: a("look_at"),
            forward: a("forward"),
            right: a("right"),
            up: a("up"),
            with: a("with"),
            without: a("without"),
        }
    }

    /// Whether `key` (its atom `atom`) names one of an entity's methods.
    ///
    /// # Safety
    ///
    /// `key` is a live string.
    unsafe fn entity_method(&self, key: *const c_char, atom: c_int) -> bool {
        let names: [(c_int, &[u8]); 6] = [
            (self.has, b"has"),
            (self.add, b"add"),
            (self.remove, b"remove"),
            (self.despawn, b"despawn"),
            (self.children, b"children"),
            (self.find_child, b"find_child"),
        ];
        // SAFETY: as the caller says.
        names
            .iter()
            .any(|&(named, name)| unsafe { is_named(key, atom, named, name) })
    }
}

/// A view kept ([`Tables::kept`]): what it shows (its entity's bits, its type's address,
/// its component, its offset) and its registry reference (0: none).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Kept {
    entity: u64,
    info: usize,
    id: u32,
    offset: u32,
    view: c_int,
}

/// A key that is a hash already, passed through.
#[derive(Default)]
struct Hashed(u64);

impl std::hash::Hasher for Hashed {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(b);
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}

/// FNV-1a over a name's bytes: [`NameIndex`]'s key.
fn name_hash(name: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `world.find`'s index (ADR 0114 §3): for each name's hash, the first entity in the
/// entities' order with a name of that hash. Brought up to date at a stage's first find
/// from the names that came or changed since (so nothing is made while none does), and
/// checked against the entity's own name at each find: a first one renamed or gone, or two
/// names of one hash, make it again from the world.
#[derive(Default)]
struct NameIndex {
    /// The stage it was brought up to date in, the world it follows and that world's
    /// restores (a restore makes it again).
    stage: u64,
    world: (u64, u64),
    all: Option<QueryState<(Entity, &'static Name)>>,
    changed: Option<QueryState<(Entity, &'static Name), Changed<Name>>>,
    first: HashMap<u64, Entity, BuildHasherDefault<Hashed>>,
}

impl NameIndex {
    /// Notes `e`, named so, if it comes before the first of its name's hash.
    fn note(first: &mut HashMap<u64, Entity, BuildHasherDefault<Hashed>>, e: Entity, name: &str) {
        first
            .entry(name_hash(name.as_bytes()))
            .and_modify(|had| {
                if e.index() < had.index() {
                    *had = e;
                }
            })
            .or_insert(e);
    }

    /// Up to date with `world` for stage `stage`.
    fn update(&mut self, world: &mut World, stage: u64) {
        if self.stage == stage {
            return;
        }
        self.stage = stage;
        let at = (world.id(), world.resync_epoch());
        if self.world != at || self.changed.is_none() {
            if self.world.0 != at.0 {
                self.all = None;
                self.changed = None;
            }
            self.world = at;
            self.changed.get_or_insert_with(|| QueryState::new(world));
            self.rebuild(world);
        }
        let Some(changed) = self.changed.as_mut() else {
            return;
        };
        for (e, n) in changed.iter_readonly(world) {
            Self::note(&mut self.first, e, n.as_str());
        }
    }

    /// Made again from every named entity.
    fn rebuild(&mut self, world: &mut World) {
        self.first.clear();
        let all = self.all.get_or_insert_with(|| QueryState::new(world));
        for (e, n) in all.iter_readonly(world) {
            Self::note(&mut self.first, e, n.as_str());
        }
    }

    /// The first entity named `name`, if any.
    fn find(&mut self, world: &mut World, name: &str) -> Option<Entity> {
        let named =
            |world: &World, e: Entity| world.get::<Name>(e).is_some_and(|n| n.as_str() == name);
        let h = name_hash(name.as_bytes());
        // Every name that came is in the index: none of this hash means none of this name.
        let first = *self.first.get(&h)?;
        if named(world, first) {
            return Some(first);
        }
        // The first went or was renamed: made again, then a scan for two names of a hash.
        self.rebuild(world);
        let first = *self.first.get(&h)?;
        if named(world, first) {
            return Some(first);
        }
        let all = self.all.as_mut()?;
        all.iter_readonly(world)
            .filter(|(_, n)| n.as_str() == name)
            .map(|(e, _)| e)
            .min_by_key(|e| e.index())
    }
}

/// What the fast path reads besides the world: each struct's slots (by its description's
/// address, kept for the host's life), the components by their names' atoms, and the
/// slow paths.
pub(crate) struct Tables {
    slots: HashMap<usize, Box<[Slot]>>,
    named: Vec<Option<Named>>,
    pub(crate) slow: Slow,
    /// The atoms of `self`'s own names, which come before its props, and of an entity's.
    entity: c_int,
    locals: c_int,
    name: c_int,
    alive: c_int,
    /// The stage open (counted from 1), and the entities by name as `world.find` sees them
    /// in it (ADR 0114 §3: once a stage): of several of one name, the first in the
    /// entities' order.
    stage: u64,
    names: NameIndex,
    /// The views made through entities and of structs inside views, the latest of each
    /// place, found by what they show ([`push_kept_view`]).
    kept: Box<[Kept]>,
    /// The atoms of entities' `parent` and methods, and `Transform`'s.
    methods: Methods,
    /// `Transform`'s id, whose views have its methods.
    pub(crate) transform: Option<ComponentId>,
    /// Each script component's table (what its module declared, its own functions on it)
    /// by the component's index, as a registry reference: `self:open()` calls
    /// `Door.open(self)`.
    own: HashMap<u32, c_int>,
    /// The entities scripts despawned in this stage: the host despawns them, with their
    /// children, at its end (ADR 0114 §7).
    pub(crate) despawns: Vec<Entity>,
    /// `find_child`'s queue (scratch).
    search: Vec<Entity>,
}

impl Default for Tables {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
            named: Vec::new(),
            slow: Slow::default(),
            entity: c_int::from(atom_of("entity")),
            locals: c_int::from(atom_of("locals")),
            name: c_int::from(atom_of("name")),
            alive: c_int::from(atom_of("alive")),
            stage: 0,
            names: NameIndex::default(),
            kept: vec![Kept::default(); VIEW_CACHE].into_boxed_slice(),
            methods: Methods::new(),
            transform: None,
            own: HashMap::new(),
            despawns: Vec::new(),
            search: Vec::new(),
        }
    }
}

impl Tables {
    /// The slots of struct `info` (none for another kind), made the first time with those
    /// of the structs inside it.
    pub(crate) fn slots(&mut self, info: &'static TypeInfo) -> *const [Slot] {
        let key = core::ptr::from_ref(info) as usize;
        if let Some(slots) = self.slots.get(&key) {
            return core::ptr::from_ref(&**slots);
        }
        let slots: Box<[Slot]> = match &info.kind {
            TypeKind::Struct(fields) => fields
                .iter()
                .map(|f| Slot {
                    atom: atom_of(f.name),
                    name: f.name,
                    offset: f.offset as u32,
                    fast: fast_of(f.ty),
                    ty: f.ty,
                    min: f.attrs.min.unwrap_or(f64::NEG_INFINITY),
                    max: f.attrs.max.unwrap_or(f64::INFINITY),
                })
                .collect(),
            _ => Box::new([]),
        };
        let inner: Vec<&'static TypeInfo> = slots
            .iter()
            .filter(|s| s.fast == Fast::Struct)
            .map(|s| s.ty)
            .collect();
        let made = core::ptr::from_ref(&*slots);
        self.slots.insert(key, slots);
        for ty in inner {
            self.slots(ty);
        }
        made
    }

    /// Component `name` (`id`, described by `info`) is named so from now on.
    pub(crate) fn name(&mut self, name: &str, id: ComponentId, info: &'static TypeInfo) {
        let Ok(atom) = usize::try_from(atom_of(name)) else {
            return;
        };
        let slots = self.slots(info);
        if self.named.len() <= atom {
            self.named.resize(atom + 1, None);
        }
        self.named[atom] = Some(Named { id, info, slots });
    }

    /// Script component `id`'s table is the one at registry reference `table` from now on;
    /// the one before, if any, for the caller to let go.
    pub(crate) fn set_own(&mut self, id: ComponentId, table: c_int) -> Option<c_int> {
        self.own.insert(id.index(), table)
    }

    /// Component `id`'s table goes (its component no longer has one): its reference, for
    /// the caller to let go.
    pub(crate) fn forget_own(&mut self, id: ComponentId) -> Option<c_int> {
        self.own.remove(&id.index())
    }

    /// The entity named `name` in `world`, as this stage's index has it.
    fn find(&mut self, world: &mut World, name: &str) -> Option<Entity> {
        self.names.update(world, self.stage);
        self.names.find(world, name)
    }

    fn named(&self, atom: c_int) -> Option<Named> {
        usize::try_from(atom)
            .ok()
            .and_then(|a| self.named.get(a).copied().flatten())
    }
}

/// `self` in a hook: its instance's entity and component, the props' slots, and the views
/// of its entity's components made so far.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ThisData {
    pub(crate) entity: Entity,
    pub(crate) id: ComponentId,
    pub(crate) info: &'static TypeInfo,
    slots: *const [Slot],
    /// Each view's component's name's atom and its registry reference (0: none).
    views: [(c_int, c_int); VIEWS],
    /// `self.locals`'s registry reference, once made (0: not yet).
    locals: c_int,
}

/// A component's view, or a struct's inside one: its entity and component, where the
/// struct lies, and its slots.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewData {
    pub(crate) entity: Entity,
    pub(crate) id: ComponentId,
    pub(crate) info: &'static TypeInfo,
    pub(crate) offset: usize,
    slots: *const [Slot],
}

/// The tables open for this stage (null outside), for the host's slow paths.
pub(crate) fn open_tables() -> *mut Tables {
    OPEN_TABLES.get()
}

/// The tables open to the C functions until [`close`], for a stage of their own.
pub(crate) fn open(tables: &mut Tables) {
    tables.stage += 1;
    OPEN_TABLES.set(tables);
}

/// The stage is over: the tables are closed to the C functions.
pub(crate) fn close() {
    OPEN_TABLES.set(core::ptr::null_mut());
}

/// A tag's metatable: the tag, its type's name, its metamethods.
type Metatable = (
    c_int,
    &'static std::ffi::CStr,
    &'static [(&'static std::ffi::CStr, ffi::lua_CFunction)],
);

/// Gives the VM the atoms and the tags' metatables.
///
/// # Safety
///
/// `state` is the VM's main state, with room for a few values; called once per VM.
pub(crate) unsafe fn install(state: *mut ffi::lua_State) {
    // SAFETY: as the caller says; each call is Luau's own API on that state.
    unsafe {
        (*ffi::lua_callbacks(state)).useratom = Some(useratom);
        let methods: [Metatable; 3] = [
            (
                TAG_SELF,
                c"Self",
                &[
                    (c"__index", self_index),
                    (c"__newindex", self_newindex),
                    (c"__namecall", self_namecall),
                ],
            ),
            (
                TAG_VIEW,
                c"View",
                &[
                    (c"__index", view_index),
                    (c"__newindex", view_newindex),
                    (c"__namecall", view_namecall),
                ],
            ),
            (
                TAG_QUERY,
                c"Query",
                &[(c"__iter", query_iter), (c"__namecall", query_namecall)],
            ),
        ];
        for (tag, name, functions) in methods {
            ffi::lua_createtable(state, 0, 4);
            for &(key, f) in functions {
                ffi::lua_pushcfunctiond(state, f, key.as_ptr());
                ffi::lua_rawsetfield(state, -2, key.as_ptr());
            }
            ffi::lua_pushstring(state, name.as_ptr());
            ffi::lua_rawsetfield(state, -2, c"__type".as_ptr());
            ffi::lua_setreadonly(state, -1, 1);
            ffi::lua_setuserdatametatable(state, tag);
        }
        ffi::lua_setuserdatadtor(state, TAG_QUERY, Some(query_dtor));
        // Entities: the metatable of every light userdata, set through one of them.
        let entity: [(&std::ffi::CStr, ffi::lua_CFunction); 3] = [
            (c"__index", entity_index),
            (c"__namecall", entity_namecall),
            (c"__tostring", entity_tostring),
        ];
        ffi::lua_pushlightuserdatatagged(state, core::ptr::null_mut(), ENTITY_TAG);
        ffi::lua_createtable(state, 0, 3);
        for (key, f) in entity {
            ffi::lua_pushcfunctiond(state, f, key.as_ptr());
            ffi::lua_rawsetfield(state, -2, key.as_ptr());
        }
        ffi::lua_setreadonly(state, -1, 1);
        ffi::lua_setmetatable(state, -2);
        ffi::lua_pop(state, 1);
        ffi::lua_setlightuserdataname(state, ENTITY_TAG, c"Entity".as_ptr());
    }
}

/// Pushes a new `self` of `entity`'s component `id` (described by `info`).
///
/// # Safety
///
/// `state` has room for a value; the tables are those `info`'s slots come from.
pub(crate) unsafe fn push_this(
    state: *mut ffi::lua_State,
    tables: &mut Tables,
    entity: Entity,
    id: ComponentId,
    info: &'static TypeInfo,
) {
    let data = ThisData {
        entity,
        id,
        info,
        slots: tables.slots(info),
        views: [(0, 0); VIEWS],
        locals: 0,
    };
    // SAFETY: a block of the data's size, of `self`'s tag, written whole.
    unsafe {
        let p = ffi::lua_newuserdatataggedwithmetatable(state, size_of::<ThisData>(), TAG_SELF)
            .cast::<ThisData>();
        p.write(data);
    }
}

/// The light userdata of entity `e`: its bits plus one, so that none is the null pointer
/// (mlua's `Value::NULL`); the null entity wraps to it and is never pushed.
fn entity_pointer(e: Entity) -> *mut c_void {
    e.to_bits().wrapping_add(1) as usize as *mut c_void
}

/// The entity of light userdata `p` ([`entity_pointer`]), if one.
fn pointer_entity(p: *mut c_void) -> Option<Entity> {
    let bits = p as usize as u64;
    (bits != 0).then(|| Entity::from_bits(bits - 1))
}

/// Entity `e` as a script's value, for the host's own functions: nil for the null entity.
pub(crate) fn entity_value(e: Entity) -> mlua::Value {
    if e == Entity::NULL {
        mlua::Value::Nil
    } else {
        mlua::Value::LightUserData(mlua::LightUserData(entity_pointer(e)))
    }
}

/// The entity `value` is, if one.
pub(crate) fn value_entity(value: &mlua::Value) -> Option<Entity> {
    match value {
        mlua::Value::LightUserData(p) => pointer_entity(p.0),
        _ => None,
    }
}

/// Pushes entity `e` (nil for the null entity).
///
/// # Safety
///
/// `state` has room for a value.
pub(crate) unsafe fn push_entity(state: *mut ffi::lua_State, e: Entity) {
    // SAFETY: as the caller says.
    unsafe {
        if e == Entity::NULL {
            ffi::lua_pushnil(state);
        } else {
            ffi::lua_pushlightuserdatatagged(state, entity_pointer(e), ENTITY_TAG);
        }
    }
}

/// The entity at stack index `idx`, if an entity is there.
///
/// # Safety
///
/// `idx` is a valid index of `state`.
pub(crate) unsafe fn entity_at(state: *mut ffi::lua_State, idx: c_int) -> Option<Entity> {
    // SAFETY: as the caller says.
    pointer_entity(unsafe { ffi::lua_tolightuserdatatagged(state, idx, ENTITY_TAG) })
}

/// `world.find(name)`: the entity of that `Name` (the first made, of several), or nil.
pub(crate) unsafe extern "C-unwind" fn world_find(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM; the world and the tables are the stage's, used within
    // this call only.
    unsafe {
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        let mut len = 0usize;
        let name = ffi::lua_tolstring(state, 1, &mut len);
        if name.is_null() {
            raise(state, String::from("world.find takes a name"));
        }
        let bytes = core::slice::from_raw_parts(name.cast::<u8>(), len);
        let found = core::str::from_utf8(bytes)
            .ok()
            .and_then(|name| (*tables).find(&mut *world, name));
        push_entity(state, found.unwrap_or(Entity::NULL));
        1
    }
}

/// Pushes a new view.
///
/// # Safety
///
/// `state` has room for a value.
unsafe fn push_view_data(state: *mut ffi::lua_State, data: ViewData) {
    // SAFETY: a block of the data's size, of a view's tag, written whole.
    unsafe {
        let p = ffi::lua_newuserdatataggedwithmetatable(state, size_of::<ViewData>(), TAG_VIEW)
            .cast::<ViewData>();
        p.write(data);
    }
}

/// Pushes a view of what `data` shows: the one kept in its place in the table if it shows
/// that, else a new one kept there (the one before let go; a script holding it keeps it).
/// Views never change, so one shown twice is the same.
///
/// # Safety
///
/// `state` has room for a value; `tables` are the stage's, used within this call only.
unsafe fn push_kept_view(state: *mut ffi::lua_State, tables: *mut Tables, data: ViewData) {
    let key = Kept {
        entity: data.entity.to_bits(),
        info: core::ptr::from_ref(data.info) as usize,
        id: data.id.index(),
        offset: data.offset as u32,
        view: 0,
    };
    let mixed = key.entity
        ^ (key.info as u64).rotate_left(29)
        ^ (u64::from(key.id) << 40)
        ^ u64::from(key.offset).rotate_left(13);
    let place =
        (mixed.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - VIEW_CACHE.trailing_zeros())) as usize;
    // SAFETY: as the caller says; the place is inside the table (the hash's top bits).
    unsafe {
        let kept = &mut (*tables).kept[place];
        if kept.view != 0 && (Kept { view: 0, ..*kept }) == key {
            ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, kept.view);
            return;
        }
        push_view_data(state, data);
        if kept.view != 0 {
            ffi::lua_unref(state, kept.view);
        }
        *kept = Kept {
            view: ffi::lua_ref(state, -1),
            ..key
        };
    }
}

/// Pushes a view of `entity`'s component `id` (described by `info`), or of a struct of
/// type `info` at `offset` in it.
///
/// # Safety
///
/// `state` has room for a value.
pub(crate) unsafe fn push_view(
    state: *mut ffi::lua_State,
    tables: &mut Tables,
    entity: Entity,
    id: ComponentId,
    info: &'static TypeInfo,
    offset: usize,
) {
    let data = ViewData {
        entity,
        id,
        info,
        offset,
        slots: tables.slots(info),
    };
    // SAFETY: as the caller says.
    unsafe { push_view_data(state, data) }
}

/// `self`'s data behind userdata pointer `p` (from `AnyUserData::to_pointer`), if it is
/// one.
///
/// # Safety
///
/// `p` is a userdata's block the caller checked the tag of, or one of `self`'s.
pub(crate) unsafe fn this_at(p: *const core::ffi::c_void) -> *mut ThisData {
    p.cast::<ThisData>().cast_mut()
}

/// A view's data behind userdata pointer `p`.
///
/// # Safety
///
/// As [`this_at`], for a view.
pub(crate) unsafe fn view_at(p: *const core::ffi::c_void) -> *const ViewData {
    p.cast::<ViewData>()
}

/// `self` takes component `id` (described by `info`) after its script was reloaded: the
/// props' new slots; its kept views go (they may name an old component).
///
/// # Safety
///
/// `this` is a live `self`'s data; `state` is its VM's.
pub(crate) unsafe fn retarget(
    state: *mut ffi::lua_State,
    tables: &mut Tables,
    this: *mut ThisData,
    id: ComponentId,
    info: &'static TypeInfo,
) {
    // SAFETY: as the caller says.
    unsafe {
        forget_views(state, this);
        (*this).id = id;
        (*this).info = info;
        (*this).slots = tables.slots(info);
    }
}

/// Lets go of the views `this` kept.
///
/// # Safety
///
/// `this` is a live `self`'s data; `state` is its VM's.
pub(crate) unsafe fn forget_views(state: *mut ffi::lua_State, this: *mut ThisData) {
    // SAFETY: as the caller says; the references were taken with `lua_ref`.
    unsafe {
        for (atom, r) in &mut (*this).views {
            if *r != 0 {
                ffi::lua_unref(state, *r);
            }
            *atom = 0;
            *r = 0;
        }
    }
}

/// Lets go of what `this` kept: its views and its locals (its instance is gone).
///
/// # Safety
///
/// As [`forget_views`].
pub(crate) unsafe fn release(state: *mut ffi::lua_State, this: *mut ThisData) {
    // SAFETY: as the caller says; the references were taken with `lua_ref`.
    unsafe {
        forget_views(state, this);
        if (*this).locals != 0 {
            ffi::lua_unref(state, (*this).locals);
            (*this).locals = 0;
        }
    }
}

/// Whether the key at stack index 2 (`key`, its atom `atom`) is name `name` of atom
/// `named`: by the atom, or by the bytes when it has none.
///
/// # Safety
///
/// `key` is a live string or null.
unsafe fn is_named(key: *const c_char, atom: c_int, named: c_int, name: &[u8]) -> bool {
    if key.is_null() {
        return false;
    }
    if atom >= 0 {
        return atom == named;
    }
    // SAFETY: as the caller says.
    unsafe { std::ffi::CStr::from_ptr(key).to_bytes() == name }
}

/// Raises `message` as an error at the calling script's line (`door:14: …`, as
/// `luaL_error` says it). The caller's frame holds nothing with a destructor.
///
/// # Safety
///
/// Called from a C function of `state`.
unsafe fn raise(state: *mut ffi::lua_State, message: String) -> ! {
    // SAFETY: the string's bytes for the copy; the message is dropped before the error.
    unsafe {
        ffi::luaL_where(state, 1);
        ffi::lua_pushlstring_(state, message.as_ptr().cast(), message.len());
        drop(message);
        ffi::lua_concat(state, 2);
        ffi::lua_error(state)
    }
}

/// The world and tables open for this stage, or why not.
fn opened() -> Result<(*mut World, *mut Tables), &'static str> {
    let w = crate::scripts::open_world();
    let t = OPEN_TABLES.get();
    if w.is_null() || t.is_null() {
        return Err("the world is open to scripts only in their hooks");
    }
    Ok((w, t))
}

/// The slot of the key at stack index 2 among `slots`: by its atom, or by its bytes when it
/// has none. Also the key's atom, and whether the key is a string at all.
///
/// # Safety
///
/// `state` is a C function's, `slots` valid.
unsafe fn find(
    state: *mut ffi::lua_State,
    slots: *const [Slot],
) -> (Option<Slot>, c_int, *const c_char) {
    let mut atom: c_int = -1;
    // SAFETY: as the caller says.
    unsafe {
        let key = ffi::lua_tostringatom(state, 2, &mut atom);
        if key.is_null() {
            return (None, atom, key);
        }
        let slots = &*slots;
        let found = if atom >= 0 {
            slots.iter().find(|s| c_int::from(s.atom) == atom).copied()
        } else {
            let bytes = std::ffi::CStr::from_ptr(key).to_bytes();
            slots.iter().find(|s| s.name.as_bytes() == bytes).copied()
        };
        (found, atom, key)
    }
}

/// Hands the call to the host's function at registry reference `r` with the arguments on
/// the stack: what it returns.
///
/// # Safety
///
/// `state` is a C function's; the caller's frame holds nothing with a destructor.
unsafe fn hand_over(state: *mut ffi::lua_State, r: c_int, returns: c_int) -> c_int {
    // SAFETY: as the caller says.
    unsafe {
        let args = ffi::lua_gettop(state);
        ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, r);
        ffi::lua_insert(state, 1);
        ffi::lua_call(state, args, returns);
        returns
    }
}

/// Hands the call to the host's function at registry reference `r`, as [`hand_over`],
/// with the place of slot `slot` among `slots` (its struct's fields) after the arguments:
/// the host's function finds the field by it, not by the key's name (a string read
/// through mlua allocates).
///
/// # Safety
///
/// As [`hand_over`]; `slots` valid.
unsafe fn hand_over_slot(
    state: *mut ffi::lua_State,
    r: c_int,
    slots: *const [Slot],
    slot: &Slot,
    returns: c_int,
) -> c_int {
    // SAFETY: as the caller says.
    unsafe {
        let place = (*slots)
            .iter()
            .position(|s| s.offset == slot.offset && s.atom == slot.atom && s.name == slot.name);
        match place {
            Some(i) => ffi::lua_pushinteger(state, i as ffi::lua_Integer),
            None => ffi::lua_pushnil(state),
        }
        hand_over(state, r, returns)
    }
}

/// Pushes the value of a slot of kind `fast` at `at`: false for a kind the slow path reads.
///
/// # Safety
///
/// `at` points to a value of the slot's type in a live component.
unsafe fn push_value(state: *mut ffi::lua_State, at: *const u8, fast: Fast) -> bool {
    // SAFETY: (each read) as the caller says.
    unsafe {
        match fast {
            Fast::F32 => ffi::lua_pushnumber(state, f64::from(at.cast::<f32>().read_unaligned())),
            Fast::F64 => ffi::lua_pushnumber(state, at.cast::<f64>().read_unaligned()),
            Fast::I8 => ffi::lua_pushnumber(state, f64::from(at.cast::<i8>().read())),
            Fast::U8 => ffi::lua_pushnumber(state, f64::from(at.read())),
            Fast::I16 => ffi::lua_pushnumber(state, f64::from(at.cast::<i16>().read_unaligned())),
            Fast::U16 => ffi::lua_pushnumber(state, f64::from(at.cast::<u16>().read_unaligned())),
            Fast::I32 => ffi::lua_pushnumber(state, f64::from(at.cast::<i32>().read_unaligned())),
            Fast::U32 => ffi::lua_pushnumber(state, f64::from(at.cast::<u32>().read_unaligned())),
            Fast::I64 => ffi::lua_pushnumber(state, at.cast::<i64>().read_unaligned() as f64),
            Fast::U64 => ffi::lua_pushnumber(state, at.cast::<u64>().read_unaligned() as f64),
            Fast::Bool => ffi::lua_pushboolean(state, c_int::from(at.read() != 0)),
            Fast::Vec3 | Fast::Vec3A => {
                let v = at.cast::<[f32; 3]>().read_unaligned();
                ffi::lua_pushvector(state, v[0], v[1], v[2]);
            }
            Fast::Vec2 => {
                let v = at.cast::<[f32; 2]>().read_unaligned();
                ffi::lua_pushvector(state, v[0], v[1], 0.0);
            }
            Fast::Entity => push_entity(state, at.cast::<Entity>().read_unaligned()),
            Fast::Struct | Fast::Slow => return false,
        }
    }
    true
}

/// What a write of the value at stack index 3 into a slot came to.
enum Wrote {
    Done,
    /// The slow path writes it (or says why it cannot).
    Slow,
    /// The value is not of the slot's kind: what it takes.
    Wrong(&'static str),
}

/// Writes the value at stack index 3 into slot `slot` at `at`, clamped to its range.
///
/// # Safety
///
/// `at` points to a value of the slot's type in a live component, plain data.
unsafe fn write_value(state: *mut ffi::lua_State, at: *mut u8, slot: &Slot) -> Wrote {
    // SAFETY: (each access) as the caller says.
    unsafe {
        match slot.fast {
            Fast::Bool => {
                if ffi::lua_type(state, 3) != ffi::LUA_TBOOLEAN {
                    return Wrote::Wrong("a boolean");
                }
                at.write(u8::from(ffi::lua_toboolean(state, 3) != 0));
            }
            Fast::Vec3 | Fast::Vec3A | Fast::Vec2 => {
                let v = ffi::lua_tovector(state, 3);
                if v.is_null() {
                    return Wrote::Wrong("a vector");
                }
                let v = v.cast::<[f32; 3]>().read_unaligned();
                match slot.fast {
                    Fast::Vec2 => at.cast::<[f32; 2]>().write_unaligned([v[0], v[1]]),
                    _ => at.cast::<[f32; 3]>().write_unaligned(v),
                }
            }
            Fast::Entity => {
                let e = if ffi::lua_type(state, 3) == ffi::LUA_TNIL {
                    Entity::NULL
                } else {
                    let Some(e) = entity_at(state, 3) else {
                        return Wrote::Wrong("an entity");
                    };
                    e
                };
                at.cast::<Entity>().write_unaligned(e);
            }
            Fast::Struct | Fast::Slow => return Wrote::Slow,
            number => {
                let mut is_number: c_int = 0;
                let n = ffi::lua_tonumberx(state, 3, &mut is_number);
                if is_number == 0 || ffi::lua_type(state, 3) != ffi::LUA_TNUMBER {
                    return Wrote::Wrong("a number");
                }
                // Clamped as the inspector clamps (a NaN stays one where there is no range).
                let n = if slot.min.is_finite() {
                    n.max(slot.min)
                } else {
                    n
                };
                let n = if slot.max.is_finite() {
                    n.min(slot.max)
                } else {
                    n
                };
                let whole = n.fract() == 0.0;
                match number {
                    Fast::F32 => at.cast::<f32>().write_unaligned(n as f32),
                    Fast::F64 => at.cast::<f64>().write_unaligned(n),
                    _ if !whole => return Wrote::Wrong("a whole number"),
                    Fast::I8 => at.cast::<i8>().write(n as i8),
                    Fast::U8 => at.write(n as u8),
                    Fast::I16 => at.cast::<i16>().write_unaligned(n as i16),
                    Fast::U16 => at.cast::<u16>().write_unaligned(n as u16),
                    Fast::I32 => at.cast::<i32>().write_unaligned(n as i32),
                    Fast::U32 => at.cast::<u32>().write_unaligned(n as u32),
                    Fast::I64 => at.cast::<i64>().write_unaligned(n as i64),
                    Fast::U64 => at.cast::<u64>().write_unaligned(n as u64),
                    _ => return Wrote::Slow,
                }
            }
        }
    }
    Wrote::Done
}

/// The type name of the value at stack index 3, for an error.
///
/// # Safety
///
/// `state` is a C function's with three arguments.
unsafe fn given(state: *mut ffi::lua_State) -> String {
    // SAFETY: as the caller says; type names are Luau's static strings.
    unsafe {
        if entity_at(state, 3).is_some() {
            return String::from("Entity");
        }
        let name = ffi::lua_typename(state, ffi::lua_type(state, 3));
        if name.is_null() {
            return String::from("a value");
        }
        std::ffi::CStr::from_ptr(name)
            .to_string_lossy()
            .into_owned()
    }
}

/// `self.key`.
unsafe extern "C-unwind" fn self_index(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM, called with `self` and the key; the world and the
    // tables are those of the stage the host opened, used within this call only.
    unsafe {
        let this = ffi::lua_touserdatatagged(state, 1, TAG_SELF).cast::<ThisData>();
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        if this.is_null() {
            raise(state, String::from("not a script's `self`"));
        }
        let (slot, atom, key) = find(state, (*this).slots);
        if key.is_null() {
            return hand_over(state, (*tables).slow.self_index, 1);
        }
        // Its entity: a value.
        if is_named(key, atom, (*tables).entity, b"entity") {
            push_entity(state, (*this).entity);
            return 1;
        }
        // Kept between hooks, made the first time.
        if is_named(key, atom, (*tables).locals, b"locals") {
            if (*this).locals != 0 {
                ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, (*this).locals);
            } else {
                ffi::lua_createtable(state, 0, 0);
                (*this).locals = ffi::lua_ref(state, -1);
            }
            return 1;
        }
        if let Some(slot) = slot {
            let Some(base) = (*world).get_raw((*this).entity, (*this).id) else {
                raise(
                    state,
                    String::from("the entity is gone, or lost the component"),
                );
            };
            if push_value(state, base.as_ptr().add(slot.offset as usize), slot.fast) {
                return 1;
            }
            return hand_over_slot(state, (*tables).slow.self_index, (*this).slots, &slot, 1);
        }
        // A component of its entity: the view kept, or one made and kept.
        if let Some(&(_, r)) = (*this).views.iter().find(|(a, r)| *r != 0 && *a == atom) {
            ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, r);
            return 1;
        }
        if let Some(named) = (*tables).named(atom)
            && (*world).get_raw((*this).entity, named.id).is_some()
        {
            push_kept_view(
                state,
                tables,
                ViewData {
                    entity: (*this).entity,
                    id: named.id,
                    info: named.info,
                    offset: 0,
                    slots: named.slots,
                },
            );
            if let Some(free) = (*this).views.iter_mut().find(|(_, r)| *r == 0) {
                *free = (atom, ffi::lua_ref(state, -1));
            }
            return 1;
        }
        // `entity`, `locals`, and what is missing (its error's wording).
        hand_over(state, (*tables).slow.self_index, 1)
    }
}

/// `self.key = value`.
unsafe extern "C-unwind" fn self_newindex(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: as `self_index`, with the value third.
    unsafe {
        let this = ffi::lua_touserdatatagged(state, 1, TAG_SELF).cast::<ThisData>();
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        if this.is_null() {
            raise(state, String::from("not a script's `self`"));
        }
        let (slot, _, key) = find(state, (*this).slots);
        let Some(slot) = slot.filter(|_| !key.is_null()) else {
            return hand_over(state, (*tables).slow.self_newindex, 0);
        };
        let Some(base) = (*world).get_raw_mut((*this).entity, (*this).id) else {
            raise(
                state,
                String::from("the entity is gone, or lost the component"),
            );
        };
        match write_value(state, base.as_ptr().add(slot.offset as usize), &slot) {
            Wrote::Done => 0,
            Wrote::Slow => {
                hand_over_slot(state, (*tables).slow.self_newindex, (*this).slots, &slot, 0)
            }
            Wrote::Wrong(what) => {
                let message = format!(
                    "{}.{} takes {what}, not {}",
                    (*this).info.name,
                    slot.name,
                    given(state)
                );
                raise(state, message)
            }
        }
    }
}

/// `view.key`.
unsafe extern "C-unwind" fn view_index(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: as `self_index`, for a view.
    unsafe {
        let view = ffi::lua_touserdatatagged(state, 1, TAG_VIEW).cast::<ViewData>();
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        if view.is_null() {
            raise(state, String::from("not a component's view"));
        }
        let (slot, _, key) = find(state, (*view).slots);
        let Some(slot) = slot.filter(|_| !key.is_null()) else {
            return hand_over(state, (*tables).slow.view_index, 1);
        };
        let Some(base) = (*world).get_raw((*view).entity, (*view).id) else {
            raise(
                state,
                String::from("the entity is gone, or lost the component"),
            );
        };
        let offset = (*view).offset + slot.offset as usize;
        if push_value(state, base.as_ptr().add(offset), slot.fast) {
            return 1;
        }
        if slot.fast == Fast::Struct {
            let slots = (*tables).slots(slot.ty);
            push_kept_view(
                state,
                tables,
                ViewData {
                    entity: (*view).entity,
                    id: (*view).id,
                    info: slot.ty,
                    offset,
                    slots,
                },
            );
            return 1;
        }
        hand_over_slot(state, (*tables).slow.view_index, (*view).slots, &slot, 1)
    }
}

/// `view.key = value`.
unsafe extern "C-unwind" fn view_newindex(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: as `self_newindex`, for a view.
    unsafe {
        let view = ffi::lua_touserdatatagged(state, 1, TAG_VIEW).cast::<ViewData>();
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        if view.is_null() {
            raise(state, String::from("not a component's view"));
        }
        let (slot, _, key) = find(state, (*view).slots);
        let Some(slot) = slot.filter(|_| !key.is_null()) else {
            return hand_over(state, (*tables).slow.view_newindex, 0);
        };
        let Some(base) = (*world).get_raw_mut((*view).entity, (*view).id) else {
            raise(
                state,
                String::from("the entity is gone, or lost the component"),
            );
        };
        let at = base.as_ptr().add((*view).offset + slot.offset as usize);
        match write_value(state, at, &slot) {
            Wrote::Done => 0,
            Wrote::Slow => {
                hand_over_slot(state, (*tables).slow.view_newindex, (*view).slots, &slot, 0)
            }
            Wrote::Wrong(what) => {
                let message = format!(
                    "{}.{} takes {what}, not {}",
                    (*view).info.name,
                    slot.name,
                    given(state)
                );
                raise(state, message)
            }
        }
    }
}

/// `entity.key`: its `name`, whether it is `alive`, its components by name.
unsafe extern "C-unwind" fn entity_index(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: as `self_index`, for an entity.
    unsafe {
        let Some(e) = entity_at(state, 1) else {
            raise(state, String::from("not an entity"));
        };
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        let mut atom: c_int = -1;
        let key = ffi::lua_tostringatom(state, 2, &mut atom);
        if key.is_null() {
            raise(
                state,
                String::from("an entity's fields are named by strings"),
            );
        }
        if is_named(key, atom, (*tables).alive, b"alive") {
            ffi::lua_pushboolean(state, c_int::from((*world).contains(e)));
            return 1;
        }
        if is_named(key, atom, (*tables).name, b"name") {
            match (*world).get::<engine_scene::Name>(e) {
                Some(n) => {
                    let n = n.as_str();
                    ffi::lua_pushlstring_(state, n.as_ptr().cast(), n.len());
                }
                None => ffi::lua_pushnil(state),
            }
            return 1;
        }
        if is_named(key, atom, (*tables).methods.parent, b"parent") {
            push_entity(state, (*world).parent(e).unwrap_or(Entity::NULL));
            return 1;
        }
        if (*tables).methods.entity_method(key, atom) {
            let text = std::ffi::CStr::from_ptr(key).to_string_lossy().into_owned();
            let message = format!("`{text}` is a method: call it with a colon (`e:{text}(…)`)");
            drop(text);
            raise(state, message);
        }
        let named = (*tables).named(atom).or_else(|| {
            // A name made before the atoms were: by its bytes.
            let bytes = std::ffi::CStr::from_ptr(key).to_bytes();
            core::str::from_utf8(bytes)
                .ok()
                .map(atom_of)
                .and_then(|a| (*tables).named(c_int::from(a)))
        });
        if let Some(named) = named
            && (*world).get_raw(e, named.id).is_some()
        {
            push_kept_view(
                state,
                tables,
                ViewData {
                    entity: e,
                    id: named.id,
                    info: named.info,
                    offset: 0,
                    slots: named.slots,
                },
            );
            return 1;
        }
        // What is missing, said before the error is raised (the text dropped first).
        let text = std::ffi::CStr::from_ptr(key).to_string_lossy().into_owned();
        let message = if named.is_some() {
            format!("the entity has no {text} (check with `e:has({text})` once it exists)")
        } else if text.starts_with(|c: char| c.is_ascii_uppercase()) {
            format!("no component is called `{text}`")
        } else {
            format!("an entity has no `{text}`")
        };
        drop(text);
        raise(state, message)
    }
}

/// The component the value at stack index `idx` names: a component's table (`Transform`,
/// a script's `component` table: its `name`) or its name; with the name's text (null when
/// there is none) for an error.
///
/// # Safety
///
/// `idx` is a valid index of `state`; `tables` are the stage's.
unsafe fn component_at(
    state: *mut ffi::lua_State,
    tables: *mut Tables,
    idx: c_int,
) -> (Option<Named>, *const c_char) {
    // SAFETY: as the caller says; a table's `name` lives while the table does.
    unsafe {
        let mut atom: c_int = -1;
        let key = if ffi::lua_type(state, idx) == ffi::LUA_TTABLE {
            ffi::lua_rawgetfield(state, idx, c"name".as_ptr());
            let key = ffi::lua_tostringatom(state, -1, &mut atom);
            ffi::lua_pop(state, 1);
            key
        } else {
            ffi::lua_tostringatom(state, idx, &mut atom)
        };
        if key.is_null() {
            return (None, key);
        }
        let named = (*tables).named(atom).or_else(|| {
            let bytes = std::ffi::CStr::from_ptr(key).to_bytes();
            core::str::from_utf8(bytes)
                .ok()
                .map(atom_of)
                .and_then(|a| (*tables).named(c_int::from(a)))
        });
        (named, key)
    }
}

/// Why the value [`component_at`] read for `method` names no component.
///
/// # Safety
///
/// `key` is a live string or null.
unsafe fn no_component(method: &str, key: *const c_char) -> String {
    if key.is_null() {
        return format!("{method} takes a component (`Transform`, a script's) or its name");
    }
    // SAFETY: as the caller says.
    let text = unsafe { std::ffi::CStr::from_ptr(key) }.to_string_lossy();
    format!("{method}: no component is called `{text}`")
}

/// The nearest of `e`'s descendants named `name`: its children first, then theirs.
/// `search` is the queue.
fn find_child(world: &World, search: &mut Vec<Entity>, e: Entity, name: &[u8]) -> Option<Entity> {
    search.clear();
    search.push(e);
    let mut at = 0;
    while at < search.len() {
        let parent = search[at];
        at += 1;
        for child in world.children(parent) {
            if world
                .get::<Name>(child)
                .is_some_and(|n| n.as_str().as_bytes() == name)
            {
                return Some(child);
            }
            search.push(child);
        }
    }
    None
}

/// `e:method(…)` and `self:method(…)` for entity `e`, the method the call names: an
/// entity's own; else, with `own` the table of `self`'s component (0: none), a function
/// on it, called with `self` and the arguments. What the method returns.
///
/// # Safety
///
/// A C function's state, the receiver at index 1 and the arguments after it; the world and
/// tables are the stage's.
unsafe fn entity_method(
    state: *mut ffi::lua_State,
    world: *mut World,
    tables: *mut Tables,
    e: Entity,
    own: c_int,
) -> c_int {
    // SAFETY: as the caller says; each call is Luau's own API on that state, strings read
    // while they are on the stack.
    unsafe {
        let mut atom: c_int = -1;
        let method = ffi::lua_namecallatom(state, &mut atom);
        if method.is_null() {
            raise(state, String::from("call an entity's methods with a colon"));
        }
        let m = (*tables).methods;
        if is_named(method, atom, m.has, b"has") {
            let (named, key) = component_at(state, tables, 2);
            let Some(named) = named else {
                let message = no_component("has", key);
                raise(state, message);
            };
            ffi::lua_pushboolean(state, c_int::from((*world).get_raw(e, named.id).is_some()));
            return 1;
        }
        if is_named(method, atom, m.remove, b"remove") {
            let (named, key) = component_at(state, tables, 2);
            let Some(named) = named else {
                let message = no_component("remove", key);
                raise(state, message);
            };
            if matches!(named.info.name, "Parent" | "Children") {
                raise(
                    state,
                    String::from(
                        "`Parent` and `Children` keep the hierarchy: they are not removed",
                    ),
                );
            }
            (*world).remove_raw(e, named.id);
            return 0;
        }
        if is_named(method, atom, m.add, b"add") {
            // The host's function makes the component's default and writes the values.
            push_entity(state, e);
            ffi::lua_replace(state, 1);
            return hand_over(state, (*tables).slow.add, 0);
        }
        if is_named(method, atom, m.despawn, b"despawn") {
            (*tables).despawns.push(e);
            return 0;
        }
        if is_named(method, atom, m.children, b"children") {
            ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, (*tables).slow.children);
            push_entity(state, e);
            ffi::lua_pushnil(state);
            return 3;
        }
        if is_named(method, atom, m.find_child, b"find_child") {
            if ffi::lua_type(state, 2) != ffi::LUA_TSTRING {
                raise(state, String::from("find_child takes a name"));
            }
            let mut len = 0usize;
            let name = ffi::lua_tolstring(state, 2, &mut len);
            let name = core::slice::from_raw_parts(name.cast::<u8>(), len);
            let found = find_child(&*world, &mut (*tables).search, e, name);
            push_entity(state, found.unwrap_or(Entity::NULL));
            return 1;
        }
        // `self`'s component's own functions: `self:open()` is `Door.open(self)`.
        if own != 0 {
            ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, own);
            ffi::lua_rawgetfield(state, -1, method);
            if ffi::lua_type(state, -1) == ffi::LUA_TFUNCTION {
                ffi::lua_remove(state, -2);
                ffi::lua_insert(state, 1);
                let args = ffi::lua_gettop(state) - 1;
                ffi::lua_call(state, args, ffi::LUA_MULTRET);
                return ffi::lua_gettop(state);
            }
            ffi::lua_pop(state, 2);
        }
        let text = std::ffi::CStr::from_ptr(method)
            .to_string_lossy()
            .into_owned();
        let message = if own != 0 {
            format!("no method `{text}`: neither an entity's nor a function of the component")
        } else {
            format!("an entity has no method `{text}`")
        };
        drop(text);
        raise(state, message)
    }
}

/// `entity:method(…)`.
unsafe extern "C-unwind" fn entity_namecall(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM, called with the entity and the arguments; the world
    // and tables are the stage's.
    unsafe {
        let Some(e) = entity_at(state, 1) else {
            raise(state, String::from("not an entity"));
        };
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        entity_method(state, world, tables, e, 0)
    }
}

/// `self:method(…)`: its entity's methods, then its component's own functions.
unsafe extern "C-unwind" fn self_namecall(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: as `entity_namecall`, for `self`.
    unsafe {
        let this = ffi::lua_touserdatatagged(state, 1, TAG_SELF).cast::<ThisData>();
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        if this.is_null() {
            raise(state, String::from("not a script's `self`"));
        }
        let own = (*tables).own.get(&(*this).id.index()).copied().unwrap_or(0);
        entity_method(state, world, tables, (*this).entity, own)
    }
}

/// `for child in e:children() do`: the child after `prev` (the first after nil), nil after
/// the last or once `prev` left the parent. A step keeps nothing: the parent and the child
/// before are the loop's own values.
pub(crate) unsafe extern "C-unwind" fn children_next(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM, called with the parent and the child before.
    unsafe {
        let (world, _) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        let next = entity_at(state, 1).and_then(|parent| match entity_at(state, 2) {
            None => (*world).children(parent).next(),
            Some(prev) => (*world)
                .get::<Parent>(prev)
                .filter(|p| p.get() == parent)
                .map(|p| p.next_sibling())
                .filter(|&n| n != Entity::NULL),
        });
        push_entity(state, next.unwrap_or(Entity::NULL));
        1
    }
}

/// The vector at stack index `idx`, if one.
///
/// # Safety
///
/// `idx` is a valid index of `state`.
unsafe fn vector_at(state: *mut ffi::lua_State, idx: c_int) -> Option<Vec3> {
    // SAFETY: as the caller says; a vector's three floats.
    unsafe {
        let v = ffi::lua_tovector(state, idx);
        (!v.is_null()).then(|| Vec3::from_array(v.cast::<[f32; 3]>().read_unaligned()))
    }
}

/// `Transform`'s method `method` (atom `atom`) on `e`'s, if it is one: what it returns.
///
/// # Safety
///
/// As [`entity_method`], the view at index 1; `id` is `Transform`'s.
unsafe fn transform_method(
    state: *mut ffi::lua_State,
    world: *mut World,
    m: Methods,
    e: Entity,
    id: ComponentId,
    method: *const c_char,
    atom: c_int,
) -> Option<c_int> {
    // SAFETY: as the caller says; the component's bytes are a `Transform`.
    unsafe {
        let gone = "the entity is gone, or lost the component";
        if is_named(method, atom, m.rotate, b"rotate") {
            let mut is_number: c_int = 0;
            let angle = ffi::lua_tonumberx(state, 3, &mut is_number);
            let (Some(axis), true) = (vector_at(state, 2), is_number != 0) else {
                raise(
                    state,
                    String::from("rotate takes an axis (a vector) and an angle (radians)"),
                );
            };
            let Some(axis) = axis.try_normalize() else {
                raise(state, String::from("rotate: an axis of length 0"));
            };
            let Some(base) = (*world).get_raw_mut(e, id) else {
                raise(state, String::from(gone));
            };
            let t = base.as_ptr().cast::<Transform>();
            let mut value = t.read_unaligned();
            value.rotation =
                (Quat::from_axis_angle(axis, angle as f32) * value.rotation).normalize();
            t.write_unaligned(value);
            return Some(0);
        }
        if is_named(method, atom, m.look_at, b"look_at") {
            let Some(point) = vector_at(state, 2) else {
                raise(
                    state,
                    String::from("look_at takes a point (a vector), then an up (optional)"),
                );
            };
            let up = vector_at(state, 3).unwrap_or(Vec3::Y);
            let Some(base) = (*world).get_raw_mut(e, id) else {
                raise(state, String::from(gone));
            };
            let t = base.as_ptr().cast::<Transform>();
            let mut value = t.read_unaligned();
            let to = point - value.translation;
            // Nowhere to look (the point is where it stands, or straight along `up`): as it was.
            if to.length_squared() > 1e-12
                && to
                    .normalize()
                    .cross(up.normalize_or_zero())
                    .length_squared()
                    > 1e-8
            {
                value.rotation = engine_runtime::look_rotation(value.translation, point, up);
                t.write_unaligned(value);
            }
            return Some(0);
        }
        let axis = if is_named(method, atom, m.forward, b"forward") {
            Vec3::NEG_Z
        } else if is_named(method, atom, m.right, b"right") {
            Vec3::X
        } else if is_named(method, atom, m.up, b"up") {
            Vec3::Y
        } else {
            return None;
        };
        let Some(base) = (*world).get_raw(e, id) else {
            raise(state, String::from(gone));
        };
        let v = base.as_ptr().cast::<Transform>().read_unaligned().rotation * axis;
        ffi::lua_pushvector(state, v.x, v.y, v.z);
        Some(1)
    }
}

/// `view:method(…)`: `Transform`'s methods on its views.
unsafe extern "C-unwind" fn view_namecall(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: as `entity_namecall`, for a view.
    unsafe {
        let view = ffi::lua_touserdatatagged(state, 1, TAG_VIEW).cast::<ViewData>();
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        if view.is_null() {
            raise(state, String::from("not a component's view"));
        }
        let mut atom: c_int = -1;
        let method = ffi::lua_namecallatom(state, &mut atom);
        if method.is_null() {
            raise(
                state,
                String::from("call a component's methods with a colon"),
            );
        }
        if let Some(id) = (*tables).transform
            && (*view).id == id
            && (*view).offset == 0
            && let Some(n) = transform_method(
                state,
                world,
                (*tables).methods,
                (*view).entity,
                id,
                method,
                atom,
            )
        {
            return n;
        }
        let text = std::ffi::CStr::from_ptr(method)
            .to_string_lossy()
            .into_owned();
        let message = format!("{} has no method `{text}`", (*view).info.name);
        drop(text);
        raise(state, message)
    }
}

/// `tostring(entity)`: `Entity(index)`.
unsafe extern "C-unwind" fn entity_tostring(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM; the text is copied before the frame ends.
    unsafe {
        let text = match entity_at(state, 1) {
            Some(e) => format!("Entity({})", e.index()),
            None => String::from("Entity(?)"),
        };
        ffi::lua_pushlstring_(state, text.as_ptr().cast(), text.len());
        1
    }
}

/// A query's tag.
pub(crate) const TAG_QUERY: c_int = 67;

thread_local! {
    /// Registry references of queries gone (their views), with their VM's main state, let
    /// go of at that VM's next stage: a destructor runs inside Luau's collector, where the
    /// registry is not written.
    static DEAD_REFS: RefCell<Vec<(usize, c_int)>> = const { RefCell::new(Vec::new()) };
}

/// How many references of queries gone wait to be let go of (tests).
#[cfg(test)]
pub(crate) fn dead_refs() -> usize {
    DEAD_REFS.with(|d| d.borrow().len())
}

/// The references of the queries of VM `main` (its main state's address) gone since the
/// last call, into `out`.
pub(crate) fn take_dead_refs(main: usize, out: &mut Vec<c_int>) {
    DEAD_REFS.with(|d| {
        if let Ok(mut d) = d.try_borrow_mut() {
            d.retain(|&(vm, r)| {
                if vm == main {
                    out.push(r);
                }
                vm != main
            });
        }
    });
}

/// The address of `state`'s VM's main state, which names the VM.
///
/// # Safety
///
/// `state` is a live state.
pub(crate) unsafe fn main_state(state: *mut ffi::lua_State) -> usize {
    // SAFETY: as the caller says.
    unsafe { ffi::lua_mainthread(state) as usize }
}

/// `world.query(A, B, …)` (ADR 0114 §3): the components a loop gives (`for e, a, b in q
/// do`), its filters (`q:with(C)`, `q:without(D)`), and, once a loop began, what matched
/// then and where the loop is.
pub(crate) struct Query {
    /// The columns and filters by name, until the first loop finds them: a script's own
    /// component is registered once its module ran.
    names: Vec<Box<str>>,
    with: Vec<Box<str>>,
    without: Vec<Box<str>>,
    found: bool,
    columns: Vec<Named>,
    with_ids: Vec<ComponentId>,
    without_ids: Vec<ComponentId>,
    /// The entities that matched when the loop began, and the one given last.
    matched: Vec<Entity>,
    at: usize,
    /// One view a column, moved to each entity in turn (registry references).
    views: Vec<c_int>,
}

/// A query's userdata: its query, boxed (the destructor drops it).
#[repr(C)]
struct QueryData(*mut Query);

/// The query at stack index `idx`, if one is there.
///
/// # Safety
///
/// `idx` is a valid index of `state`.
unsafe fn query_at(state: *mut ffi::lua_State, idx: c_int) -> *mut Query {
    // SAFETY: as the caller says; a query's block holds its box.
    unsafe {
        let p = ffi::lua_touserdatatagged(state, idx, TAG_QUERY).cast::<QueryData>();
        if p.is_null() {
            core::ptr::null_mut()
        } else {
            (*p).0
        }
    }
}

/// The name of the component the value at stack index `idx` names (its table's `name`,
/// or the name), copied; none for another value.
///
/// # Safety
///
/// `idx` is a valid index of `state`.
unsafe fn component_name(state: *mut ffi::lua_State, idx: c_int) -> Option<Box<str>> {
    // SAFETY: as the caller says; the string is copied before it is popped.
    unsafe {
        let table = ffi::lua_type(state, idx) == ffi::LUA_TTABLE;
        if table {
            ffi::lua_rawgetfield(state, idx, c"name".as_ptr());
        }
        let at = if table { -1 } else { idx };
        let name = (ffi::lua_type(state, at) == ffi::LUA_TSTRING).then(|| {
            let mut len = 0usize;
            let s = ffi::lua_tolstring(state, at, &mut len);
            String::from_utf8_lossy(core::slice::from_raw_parts(s.cast::<u8>(), len))
                .into_owned()
                .into_boxed_str()
        });
        if table {
            ffi::lua_pop(state, 1);
        }
        name
    }
}

/// The names of the components the arguments from index `from` name, or the place of the
/// first that names none.
///
/// # Safety
///
/// A C function's state.
unsafe fn component_names(state: *mut ffi::lua_State, from: c_int) -> Result<Vec<Box<str>>, c_int> {
    // SAFETY: as the caller says.
    unsafe {
        let n = ffi::lua_gettop(state);
        let mut names = Vec::with_capacity(n.max(0) as usize);
        for i in from..=n {
            match component_name(state, i) {
                Some(name) => names.push(name),
                None => return Err(i),
            }
        }
        Ok(names)
    }
}

/// `world.query(A, B, …)`: a query of those components, made once (at a module's top) and
/// looped over every tick.
pub(crate) unsafe extern "C-unwind" fn world_query(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM; the names are owned before the query is made.
    unsafe {
        let names = match component_names(state, 1) {
            Ok(names) if !names.is_empty() => names,
            Ok(_) => raise(
                state,
                String::from("world.query takes the components to give"),
            ),
            Err(i) => raise(
                state,
                format!(
                    "world.query's argument {i} is not a component (`Transform`, a script's) or its name"
                ),
            ),
        };
        let query = Box::new(Query {
            names,
            with: Vec::new(),
            without: Vec::new(),
            found: false,
            columns: Vec::new(),
            with_ids: Vec::new(),
            without_ids: Vec::new(),
            matched: Vec::new(),
            at: 0,
            views: Vec::new(),
        });
        let p = ffi::lua_newuserdatataggedwithmetatable(state, size_of::<QueryData>(), TAG_QUERY)
            .cast::<QueryData>();
        p.write(QueryData(Box::into_raw(query)));
        1
    }
}

/// A query's userdata collected: its box dropped, its views' references let go of at the
/// next stage.
unsafe extern "C" fn query_dtor(state: *mut ffi::lua_State, block: *mut c_void) {
    // SAFETY: Luau calls it once with the block of a query's userdata, which holds the box
    // `world_query` made.
    unsafe {
        let p = block.cast::<QueryData>();
        if (*p).0.is_null() {
            return;
        }
        let query = Box::from_raw((*p).0);
        (*p).0 = core::ptr::null_mut();
        let vm = main_state(state);
        DEAD_REFS.with(|d| {
            if let Ok(mut d) = d.try_borrow_mut() {
                d.extend(query.views.iter().map(|&r| (vm, r)));
            }
        });
        drop(query);
    }
}

/// `q:with(C)`, `q:without(D)`: the query, filtered.
unsafe extern "C-unwind" fn query_namecall(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM, called with the query and the components.
    unsafe {
        let q = query_at(state, 1);
        let (_, tables) = match opened() {
            Ok(open) => open,
            Err(_) => (core::ptr::null_mut(), OPEN_TABLES.get()),
        };
        if q.is_null() {
            raise(state, String::from("not a query"));
        }
        let mut atom: c_int = -1;
        let method = ffi::lua_namecallatom(state, &mut atom);
        if method.is_null() {
            raise(state, String::from("call a query's methods with a colon"));
        }
        let m = if tables.is_null() {
            Methods::new()
        } else {
            (*tables).methods
        };
        let with = is_named(method, atom, m.with, b"with");
        if !with && !is_named(method, atom, m.without, b"without") {
            let text = std::ffi::CStr::from_ptr(method)
                .to_string_lossy()
                .into_owned();
            let message = format!("a query has no method `{text}` (it has `with` and `without`)");
            drop(text);
            raise(state, message);
        }
        let names = match component_names(state, 2) {
            Ok(names) => names,
            Err(i) => raise(
                state,
                format!(
                    "argument {} is not a component (`Transform`, a script's) or its name",
                    i - 1
                ),
            ),
        };
        if with {
            (*q).with.extend(names);
        } else {
            (*q).without.extend(names);
        }
        (*q).found = false;
        ffi::lua_settop(state, 1);
        1
    }
}

/// `for e, a, b in q do`: what matches now is what the loop visits; the step after it is
/// [`query_next`].
unsafe extern "C-unwind" fn query_iter(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM, called with the query; the world and tables are the
    // stage's.
    unsafe {
        let q = query_at(state, 1);
        let (world, tables) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        if q.is_null() {
            raise(state, String::from("not a query"));
        }
        let query = &mut *q;
        if !query.found {
            let find = |name: &str| (*tables).named(c_int::from(atom_of(name)));
            let mut missing = None;
            query.columns.clear();
            for name in &query.names {
                match find(name) {
                    Some(named) => query.columns.push(named),
                    None => missing = missing.or(Some(name.clone())),
                }
            }
            query.with_ids.clear();
            for name in &query.with {
                match find(name) {
                    Some(named) => query.with_ids.push(named.id),
                    None => missing = missing.or(Some(name.clone())),
                }
            }
            query.without_ids.clear();
            for name in &query.without {
                // A component no script has made yet: no entity has it.
                if let Some(named) = find(name) {
                    query.without_ids.push(named.id);
                }
            }
            if let Some(name) = missing {
                let message = format!("world.query: no component is called `{name}`");
                drop(name);
                raise(state, message);
            }
            query.found = true;
            // Its views, one a column, made the first time.
            for &r in &query.views {
                ffi::lua_unref(state, r);
            }
            query.views.clear();
            for c in 0..query.columns.len() {
                let named = query.columns[c];
                push_view_data(
                    state,
                    ViewData {
                        entity: Entity::NULL,
                        id: named.id,
                        info: named.info,
                        offset: 0,
                        slots: named.slots,
                    },
                );
                let r = ffi::lua_ref(state, -1);
                ffi::lua_pop(state, 1);
                query.views.push(r);
            }
        }
        query.matched.clear();
        for arch in (*world).archetypes() {
            let has = arch.components();
            if query.columns.iter().all(|c| has.contains(&c.id))
                && query.with_ids.iter().all(|id| has.contains(id))
                && !query.without_ids.iter().any(|id| has.contains(id))
            {
                for chunk in 0..arch.chunk_count() {
                    query.matched.extend_from_slice(arch.chunk_entities(chunk));
                }
            }
        }
        query.at = 0;
        ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, (*tables).slow.query_next);
        ffi::lua_pushvalue(state, 1);
        ffi::lua_pushnil(state);
        3
    }
}

/// A query loop's step: the entity after `prev` that still has the columns, and its views
/// moved to it; nil after the last.
pub(crate) unsafe extern "C-unwind" fn query_next(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM, called with the query and the entity before; the
    // views' blocks are views'.
    unsafe {
        let q = query_at(state, 1);
        let (world, _) = match opened() {
            Ok(open) => open,
            Err(why) => raise(state, why.to_owned()),
        };
        if q.is_null() {
            raise(state, String::from("not a query"));
        }
        let query = &mut *q;
        let mut i = match entity_at(state, 2) {
            None => 0,
            Some(prev) if query.matched.get(query.at) == Some(&prev) => query.at + 1,
            Some(prev) => query
                .matched
                .iter()
                .position(|&e| e == prev)
                .map_or(query.matched.len(), |p| p + 1),
        };
        // What went or lost a column since the loop began is passed over.
        while let Some(&e) = query.matched.get(i)
            && !query
                .columns
                .iter()
                .all(|c| (*world).get_raw(e, c.id).is_some())
        {
            i += 1;
        }
        let Some(&e) = query.matched.get(i) else {
            ffi::lua_pushnil(state);
            return 1;
        };
        query.at = i;
        push_entity(state, e);
        for &r in &query.views {
            ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, r);
            let view = ffi::lua_touserdatatagged(state, -1, TAG_VIEW).cast::<ViewData>();
            if !view.is_null() {
                (*view).entity = e;
            }
        }
        1 + query.views.len() as c_int
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_take_one_atom_each_and_others_none() {
        let a = atom_of("translation");
        assert!(a >= 0);
        assert_eq!(atom_of("translation"), a);
        assert_ne!(atom_of("rotation"), a);
        for not_a_name in ["", "two words", "1st", "ä"] {
            assert_eq!(atom_of(not_a_name), -1, "{not_a_name:?}");
        }
        assert_eq!(atom_of(&"a".repeat(LONGEST_ATOM + 1)), -1);
    }
}
