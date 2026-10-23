//! Every reflected component and resource type linked into the program (ADR 0104 step 1).
//!
//! [`reflect_component!`](crate::reflect_component) and
//! [`reflect_resource!`](crate::reflect_resource) add their types to a list the linker
//! collects (`linkme`'s distributed slices: no code runs before `main`, nothing is
//! registered by hand). A save finds a type by its stable name before the world has seen
//! it; the editor's "add component" and scripting can list them all.

use crate::component::{ComponentDesc, ComponentId, DropGlue};
use crate::world::World;
use core::alloc::Layout;
use core::marker::PhantomData;
use engine_reflect::TypeInfo;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes a type's `Default` value into uninitialized memory of its size and alignment.
pub type WriteDefault = unsafe fn(*mut u8);

/// Inserts a resource's `Default` value into a world, replacing one there.
pub type InsertDefault = fn(&mut World);

/// A reflected type of the program.
#[derive(Debug)]
pub struct ReflectedType {
    /// Its description; [`TypeInfo::name`] is the stable name files use.
    pub info: &'static TypeInfo,
    /// What it is to the world.
    pub kind: ReflectedKind,
}

/// What a reflected type is to the world.
#[derive(Debug)]
pub enum ReflectedKind {
    /// A component.
    Component {
        /// Registers it in a world, or finds it there.
        id: fn(&mut World) -> ComponentId,
        /// Its default writer; none for a type without `Default` (it cannot be made from
        /// a file).
        default: fn() -> Option<WriteDefault>,
    },
    /// A resource.
    Resource {
        /// The world's value of it, if the world has one.
        get: fn(&World) -> Option<*const u8>,
        /// The same, writable.
        get_mut: fn(&mut World) -> Option<*mut u8>,
        /// Its default inserter; none for a type without `Default`.
        insert_default: fn() -> Option<InsertDefault>,
    },
}

#[doc(hidden)]
#[linkme::distributed_slice]
pub static REFLECTED: [ReflectedType];

/// Every reflected component and resource type linked into the program, in no
/// particular order.
pub fn reflected_types() -> &'static [ReflectedType] {
    &REFLECTED
}

/// The reflected type with this stable name.
pub fn find_reflected(name: &str) -> Option<&'static ReflectedType> {
    REFLECTED.iter().find(|t| t.info.name == name)
}

/// A `String` field of a type made at run time (a script's text prop): where it is in a
/// value, and its default text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextField {
    /// Its offset.
    pub offset: usize,
    /// Its default text.
    pub default: &'static str,
}

/// A component type made while the program runs (a script's, ADR 0114): what a world needs
/// to register it, and its default value. Scenes and the editor find these beside the
/// program's [`reflected_types`].
#[derive(Debug, Clone)]
pub struct RuntimeType {
    /// Its description; [`TypeInfo::name`] is the stable name files use.
    pub info: &'static TypeInfo,
    /// Its name in a world's registry, unique (`script::Door`).
    pub component: Box<str>,
    /// Size and alignment of one value.
    pub layout: Layout,
    /// Its default value, laid out, its texts empty: a value of its own, which any copy of
    /// its bytes is too. [`Self::write_default`] writes one with its texts.
    pub defaults: Box<[u8]>,
    /// Its `String` fields: a value owns their texts, dropped with it, so a type with any
    /// is no plain data.
    pub texts: &'static [TextField],
    /// Where it was declared, for the editor (`scripts/door.luau`); empty when nowhere.
    pub source: Box<str>,
}

impl RuntimeType {
    /// Its id in `world`: found by its registry name, or registered there now. `None` when
    /// another component holds the name with another layout.
    pub fn id(&self, world: &mut World) -> Option<ComponentId> {
        if let Some(id) = world.components().id_by_name(&self.component) {
            return (world.components().layout(id) == Some(self.layout)).then_some(id);
        }
        let desc = ComponentDesc {
            name: self.component.clone(),
            layout: self.layout,
            drop: (!self.texts.is_empty()).then_some(DropGlue::Texts(self.texts)),
            pod: self.texts.is_empty(),
            type_info: Some(self.info),
        };
        // SAFETY: `add_runtime_type`'s caller promised plain data of `layout` but for the
        // texts, `String`s the glue drops.
        unsafe { world.register_dynamic(desc) }.ok()
    }

    /// Writes its default value at `dst`: its bytes, then its texts, which the value owns.
    ///
    /// # Safety
    ///
    /// `dst` is room for one value of its layout (any alignment); what it held is not
    /// dropped.
    pub unsafe fn write_default(&self, dst: *mut u8) {
        // SAFETY: as the caller says; the bytes are a value of the layout with its texts
        // empty, each replaced (unaligned, as the room may be) without being dropped.
        unsafe {
            let len = self.defaults.len().min(self.layout.size());
            core::ptr::copy_nonoverlapping(self.defaults.as_ptr(), dst, len);
            for t in self.texts {
                dst.add(t.offset)
                    .cast::<String>()
                    .write_unaligned(String::from(t.default));
            }
        }
    }
}

/// How a reflected component's default value is written: a program type's by its
/// `Default`, a type made at run time's from its bytes and texts.
#[derive(Debug, Clone)]
pub enum DefaultValue {
    /// A type of the program.
    Written(WriteDefault),
    /// A type made at run time (a script's component, ADR 0114).
    Runtime(RuntimeType),
}

impl DefaultValue {
    /// The default of the component type named `name`: the program's type of that name,
    /// or else the latest made at run time (of `layout`, when given). `None` for a type
    /// without `Default`, or none of that name.
    pub fn of(name: &str, layout: Option<Layout>) -> Option<Self> {
        if let Some(t) = find_reflected(name) {
            let ReflectedKind::Component { default, .. } = &t.kind else {
                return None;
            };
            return default().map(Self::Written);
        }
        runtime_types()
            .into_iter()
            .rev()
            .find(|t| t.info.name == name && layout.is_none_or(|l| l == t.layout))
            .map(Self::Runtime)
    }

    /// Writes it at `dst`.
    ///
    /// # Safety
    ///
    /// `dst` is room for one value of the type, aligned; what it held is not dropped.
    pub unsafe fn write(&self, dst: *mut u8) {
        // SAFETY: as the caller says.
        unsafe {
            match self {
                Self::Written(write) => write(dst),
                Self::Runtime(t) => t.write_default(dst),
            }
        }
    }
}

/// How the values of a component made at run time move to a new layout of it (a script's
/// props changed, ADR 0114 E7-4b): field by field, by name. A field of the same name and
/// type keeps its value; the others take the new layout's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relayout {
    /// The component of the old layout.
    pub old: ComponentId,
    /// The component of the new layout.
    pub new: ComponentId,
    /// The new layout's default value (its texts empty).
    pub defaults: Box<[u8]>,
    /// The new layout's texts.
    pub texts: &'static [TextField],
    /// The fields that keep their values.
    pub moves: Vec<FieldMove>,
    /// Where the new layout holds entities (byte offsets), for values that keep entity
    /// references apart from their bytes (the editor's undo).
    pub entities: Vec<usize>,
}

/// A field that keeps its value in a [`Relayout`]: its bytes, from the old layout's offset
/// to the new's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldMove {
    /// Its offset in the old layout.
    pub from: usize,
    /// Its offset in the new layout.
    pub to: usize,
    /// Its size.
    pub len: usize,
    /// It holds an entity.
    pub entity: bool,
    /// It holds a text (a `String`): moved, not copied.
    pub text: bool,
}

impl Relayout {
    /// From component `old`, described by `before`, to `new`, described by `after` with
    /// default value `defaults` (its texts empty) and texts `texts`. Fields match by name,
    /// and keep their value when their types have one name and size (type descriptions
    /// are not unique, so not by address).
    pub fn between(
        old: ComponentId,
        before: &TypeInfo,
        new: ComponentId,
        after: &TypeInfo,
        defaults: Box<[u8]>,
        texts: &'static [TextField],
    ) -> Self {
        let fields = |t: &TypeInfo| match t.kind {
            engine_reflect::TypeKind::Struct(fields) => fields,
            _ => &[],
        };
        let entity = <crate::Entity as engine_reflect::Reflect>::TYPE_INFO;
        let is_entity = |t: &TypeInfo| t.name == entity.name && t.size == entity.size;
        let moves = fields(after)
            .iter()
            .filter_map(|f| {
                let was = fields(before).iter().find(|b| b.name == f.name)?;
                (was.ty.name == f.ty.name && was.ty.size == f.ty.size).then_some(FieldMove {
                    from: was.offset,
                    to: f.offset,
                    len: f.ty.size,
                    entity: is_entity(f.ty),
                    text: matches!(f.ty.kind, engine_reflect::TypeKind::String),
                })
            })
            .collect();
        let entities = fields(after)
            .iter()
            .filter(|f| is_entity(f.ty))
            .map(|f| f.offset)
            .collect();
        Self {
            old,
            new,
            defaults,
            texts,
            moves,
            entities,
        }
    }

    /// A value of the old layout kept as bytes (`value`, plain data: no texts) in the new,
    /// the new layout's texts empty.
    pub fn apply(&self, value: &[u8]) -> Vec<u8> {
        let mut out = self.defaults.to_vec();
        for m in self.moves.iter().filter(|m| !m.text) {
            if let (Some(from), Some(to)) = (
                value.get(m.from..m.from + m.len),
                out.get_mut(m.to..m.to + m.len),
            ) {
                to.copy_from_slice(from);
            }
        }
        out
    }

    /// Writes at `new` the value of the new layout made from the live value at `old`: the
    /// fields that keep their values take them, texts moved (left empty in the old value,
    /// so dropping it drops only those that did not move), and the new layout's other
    /// texts take their defaults.
    ///
    /// # Safety
    ///
    /// `old` points to a live, writable value of the old layout, aligned; `new` is room for
    /// one value of the new layout (any alignment), what it held not dropped.
    pub unsafe fn take(&self, old: *mut u8, new: *mut u8) {
        // SAFETY: as the caller says; the default's bytes are a value of the new layout,
        // a moved text's `String` is owned by `new` once copied and replaced in `old`
        // without a drop, and a text that did not move is written without a drop.
        unsafe {
            core::ptr::copy_nonoverlapping(self.defaults.as_ptr(), new, self.defaults.len());
            for m in &self.moves {
                core::ptr::copy_nonoverlapping(old.add(m.from), new.add(m.to), m.len);
                if m.text {
                    old.add(m.from).cast::<String>().write(String::new());
                }
            }
            for t in self.texts {
                if !self.moves.iter().any(|m| m.text && m.to == t.offset) {
                    new.add(t.offset)
                        .cast::<String>()
                        .write_unaligned(String::from(t.default));
                }
            }
        }
    }
}

/// The component types made at run time, by stable name.
static RUNTIME: RwLock<Vec<RuntimeType>> = RwLock::new(Vec::new());

/// How many times the list changed.
static RUNTIME_VERSION: AtomicU64 = AtomicU64::new(0);

/// Adds a component type made at run time, or replaces the one of its stable name.
///
/// # Safety
///
/// Its values are plain data of `layout` but for `texts`, each a `String` at its offset:
/// no other drop glue, `Send` and `Sync`, any bytes of `defaults` a valid value (its
/// texts empty).
pub unsafe fn add_runtime_type(t: RuntimeType) {
    let mut list = RUNTIME
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match list.iter_mut().find(|r| r.info.name == t.info.name) {
        Some(r) => *r = t,
        None => list.push(t),
    }
    RUNTIME_VERSION.fetch_add(1, Ordering::Release);
}

/// How many times [`add_runtime_type`] changed the list (a type added or replaced): what
/// tells a cache of [`runtime_types`] that it is stale, without copying the list.
pub fn runtime_types_version() -> u64 {
    RUNTIME_VERSION.load(Ordering::Acquire)
}

/// The component types made at run time so far.
pub fn runtime_types() -> Vec<RuntimeType> {
    RUNTIME
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// For the macros: whether a concrete type implements `Default`, found by method
/// resolution on references (the "autoref" pattern), as no trait bound can say "if".
#[doc(hidden)]
pub struct Probe<T>(PhantomData<T>);

impl<T> Probe<T> {
    #[doc(hidden)]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> Default for Probe<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[doc(hidden)]
pub trait ViaDefault {
    fn writer(&self) -> Option<unsafe fn(*mut u8)>;
    fn inserter(&self) -> Option<fn(&mut World)>;
}

impl<T: Default + Send + Sync + 'static> ViaDefault for &Probe<T> {
    fn writer(&self) -> Option<unsafe fn(*mut u8)> {
        Some(write_default::<T>)
    }

    fn inserter(&self) -> Option<fn(&mut World)> {
        Some(|world| world.insert_resource(T::default()))
    }
}

#[doc(hidden)]
pub trait NoDefault {
    fn writer(&self) -> Option<unsafe fn(*mut u8)>;
    fn inserter(&self) -> Option<fn(&mut World)>;
}

impl<T> NoDefault for Probe<T> {
    fn writer(&self) -> Option<unsafe fn(*mut u8)> {
        None
    }

    fn inserter(&self) -> Option<fn(&mut World)> {
        None
    }
}

/// # Safety
///
/// `ptr` is valid for a write of a `T`, aligned.
unsafe fn write_default<T: Default>(ptr: *mut u8) {
    // SAFETY: guaranteed by the caller.
    unsafe { ptr.cast::<T>().write(T::default()) }
}

/// Lists reflected types as resources of the world: the game's state that is not
/// attached to an entity (a quest log, the score). Each type implements `Reflect` (and
/// `Default` to be loaded from a file).
///
/// ```
/// use engine_ecs::reflect::Reflect;
/// #[derive(Reflect, Default)]
/// struct Score { points: u32 }
/// engine_ecs::reflect_resource!(Score);
/// assert!(engine_ecs::find_reflected("Score").is_some());
/// ```
#[macro_export]
macro_rules! reflect_resource {
    ($($t:ty),+ $(,)?) => {
        $(const _: () = {
            fn get(world: &$crate::World) -> ::core::option::Option<*const u8> {
                world.resource::<$t>().map(|r| (r as *const $t).cast::<u8>())
            }
            fn get_mut(world: &mut $crate::World) -> ::core::option::Option<*mut u8> {
                world.resource_mut::<$t>().map(|r| (r as *mut $t).cast::<u8>())
            }
            fn insert_default() -> ::core::option::Option<fn(&mut $crate::World)> {
                #[allow(unused_imports)]
                use $crate::reflected::{NoDefault as _, ViaDefault as _};
                (&&$crate::reflected::Probe::<$t>::new()).inserter()
            }
            #[$crate::linkme::distributed_slice($crate::reflected::REFLECTED)]
            #[linkme(crate = $crate::linkme)]
            static ENTRY: $crate::reflected::ReflectedType = $crate::reflected::ReflectedType {
                info: <$t as $crate::reflect::Reflect>::TYPE_INFO,
                kind: $crate::reflected::ReflectedKind::Resource {
                    get,
                    get_mut,
                    insert_default,
                },
            };
        };)+
    };
}

/// For `reflect_component!`: lists a component type.
#[doc(hidden)]
#[macro_export]
macro_rules! __reflected_component {
    ($t:ty) => {
        const _: () = {
            fn id(world: &mut $crate::World) -> $crate::ComponentId {
                world.register::<$t>()
            }
            fn default() -> ::core::option::Option<unsafe fn(*mut u8)> {
                #[allow(unused_imports)]
                use $crate::reflected::{NoDefault as _, ViaDefault as _};
                (&&$crate::reflected::Probe::<$t>::new()).writer()
            }
            #[$crate::linkme::distributed_slice($crate::reflected::REFLECTED)]
            #[linkme(crate = $crate::linkme)]
            static ENTRY: $crate::reflected::ReflectedType = $crate::reflected::ReflectedType {
                info: <$t as $crate::reflect::Reflect>::TYPE_INFO,
                kind: $crate::reflected::ReflectedKind::Component { id, default },
            };
        };
    };
}
