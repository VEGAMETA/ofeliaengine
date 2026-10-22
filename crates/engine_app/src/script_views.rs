//! `self` and components' views on Luau's own API (ADR 0114 E7-1c): the script host's hot
//! path, where a field's read or write is a call from a script.
//!
//! - **Atoms.** Luau gives each string an atom when it makes it (`lua_Callbacks::useratom`):
//!   a name-like string takes the next number from the thread's table ([`atom_of`]), and a
//!   struct's fields are matched by their names' atoms ([`Slot`]), so finding a field
//!   compares small integers. A name made before the callback was set has none and is
//!   matched by its bytes.
//! - **Tags.** `self` ([`TAG_SELF`]), views ([`TAG_VIEW`]) and entities ([`TAG_ENTITY`])
//!   are userdata of tags of their own, made with their tag's metatable, whose metamethods
//!   are the C functions here: they read the userdata, the key's atom and the world
//!   directly. Numbers, booleans and vectors are read and written there, entities' names
//!   and components, `world.find`; the rest (rotations, entity fields, strings, enums,
//!   errors' wording) goes to the host's own functions ([`Slow`]), called from here.
//! - **No allocation per access:** a view of `self`'s entity's component is made once and
//!   kept on `self` ([`ThisData::views`]), as are `self.locals` and `self.entity`; vectors
//!   and numbers are values.
//! - **Errors** are raised with `lua_error` once nothing with a destructor is alive in the
//!   frame, as mlua's own callbacks raise theirs.

use engine_ecs::{ComponentId, Entity, World};
use engine_reflect::{Primitive, TypeInfo, TypeKind};
use engine_script::mlua::ffi;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int};

/// `self`'s tag (mlua's own userdata take 0 and 1).
pub(crate) const TAG_SELF: c_int = 64;
/// A view's tag.
pub(crate) const TAG_VIEW: c_int = 65;
/// An entity's tag.
pub(crate) const TAG_ENTITY: c_int = 66;

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
        "Quat" | "Entity" => return Fast::Slow,
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
/// `self`'s `__index` and `__newindex`, a view's.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Slow {
    pub(crate) self_index: c_int,
    pub(crate) self_newindex: c_int,
    pub(crate) view_index: c_int,
    pub(crate) view_newindex: c_int,
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
    /// The stage open (counted from 1), and the entities by name as `world.find` made them
    /// in the stage of that number (ADR 0114 §3: once a stage): of several of one name,
    /// the first in the entities' order.
    stage: u64,
    names: (u64, HashMap<Box<str>, Entity>),
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
            names: (0, HashMap::new()),
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

    /// The entity named `name` in `world`, as this stage's index has it.
    fn find(&mut self, world: &mut World, name: &str) -> Option<Entity> {
        if self.names.0 != self.stage {
            self.names.0 = self.stage;
            let by_name = &mut self.names.1;
            by_name.clear();
            let mut q = engine_ecs::QueryState::<(Entity, &engine_scene::Name)>::new(world);
            for (e, n) in q.iter_readonly(world) {
                by_name
                    .entry(n.as_str().into())
                    .and_modify(|first: &mut Entity| {
                        if e.index() < first.index() {
                            *first = e;
                        }
                    })
                    .or_insert(e);
            }
        }
        self.names.1.get(name).copied()
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
    /// `self.locals`'s and `self.entity`'s registry references, once made (0: not yet).
    locals: c_int,
    entity_ref: c_int,
}

/// An entity in a script.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct EntityData(Entity);

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
                &[(c"__index", self_index), (c"__newindex", self_newindex)],
            ),
            (
                TAG_VIEW,
                c"View",
                &[(c"__index", view_index), (c"__newindex", view_newindex)],
            ),
            (
                TAG_ENTITY,
                c"Entity",
                &[
                    (c"__index", entity_index),
                    (c"__eq", entity_eq),
                    (c"__tostring", entity_tostring),
                ],
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
        entity_ref: 0,
    };
    // SAFETY: a block of the data's size, of `self`'s tag, written whole.
    unsafe {
        let p = ffi::lua_newuserdatataggedwithmetatable(state, size_of::<ThisData>(), TAG_SELF)
            .cast::<ThisData>();
        p.write(data);
    }
}

/// Pushes entity `e` (nil for the null entity).
///
/// # Safety
///
/// `state` has room for a value.
pub(crate) unsafe fn push_entity(state: *mut ffi::lua_State, e: Entity) {
    // SAFETY: as the caller says; a block of the data's size, of an entity's tag.
    unsafe {
        if e == Entity::NULL {
            ffi::lua_pushnil(state);
            return;
        }
        let p = ffi::lua_newuserdatataggedwithmetatable(state, size_of::<EntityData>(), TAG_ENTITY)
            .cast::<EntityData>();
        p.write(EntityData(e));
    }
}

/// The entity at stack index `idx`, if an entity is there.
///
/// # Safety
///
/// `idx` is a valid index of `state`.
pub(crate) unsafe fn entity_at(state: *mut ffi::lua_State, idx: c_int) -> Option<Entity> {
    // SAFETY: as the caller says; an entity's block holds its data.
    unsafe {
        let p = ffi::lua_touserdatatagged(state, idx, TAG_ENTITY).cast::<EntityData>();
        (!p.is_null()).then(|| (*p).0)
    }
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

/// Lets go of what `this` kept: its views, its locals and its entity (its instance is
/// gone).
///
/// # Safety
///
/// As [`forget_views`].
pub(crate) unsafe fn release(state: *mut ffi::lua_State, this: *mut ThisData) {
    // SAFETY: as the caller says; the references were taken with `lua_ref`.
    unsafe {
        forget_views(state, this);
        for r in [&mut (*this).locals, &mut (*this).entity_ref] {
            if *r != 0 {
                ffi::lua_unref(state, *r);
                *r = 0;
            }
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
        // Its entity, made the first time and kept.
        if is_named(key, atom, (*tables).entity, b"entity") {
            if (*this).entity_ref != 0 {
                ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, (*this).entity_ref);
            } else {
                push_entity(state, (*this).entity);
                (*this).entity_ref = ffi::lua_ref(state, -1);
            }
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
            return hand_over(state, (*tables).slow.self_index, 1);
        }
        // A component of its entity: the view kept, or one made and kept.
        if let Some(&(_, r)) = (*this).views.iter().find(|(a, r)| *r != 0 && *a == atom) {
            ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, r);
            return 1;
        }
        if let Some(named) = (*tables).named(atom)
            && (*world).get_raw((*this).entity, named.id).is_some()
        {
            push_view_data(
                state,
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
            Wrote::Slow => hand_over(state, (*tables).slow.self_newindex, 0),
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
            push_view_data(
                state,
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
        hand_over(state, (*tables).slow.view_index, 1)
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
            Wrote::Slow => hand_over(state, (*tables).slow.view_newindex, 0),
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
            push_view_data(
                state,
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

/// `a == b` for two entities.
unsafe extern "C-unwind" fn entity_eq(state: *mut ffi::lua_State) -> c_int {
    // SAFETY: a C function of the VM, called with both operands.
    unsafe {
        let same = entity_at(state, 1).is_some_and(|a| entity_at(state, 2) == Some(a));
        ffi::lua_pushboolean(state, c_int::from(same));
        1
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
