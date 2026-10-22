//! Every reflected component and resource type linked into the program (ADR 0104 step 1).
//!
//! [`reflect_component!`](crate::reflect_component) and
//! [`reflect_resource!`](crate::reflect_resource) add their types to a list the linker
//! collects (`linkme`'s distributed slices: no code runs before `main`, nothing is
//! registered by hand). A save finds a type by its stable name before the world has seen
//! it; the editor's "add component" and scripting can list them all.

use crate::component::{ComponentDesc, ComponentId};
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
    /// Its default value, laid out.
    pub defaults: Box<[u8]>,
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
            drop: None,
            pod: true,
            type_info: Some(self.info),
        };
        // SAFETY: `add_runtime_type`'s caller promised plain data of `layout`.
        unsafe { world.register_dynamic(desc) }.ok()
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
/// Its values are plain data of `layout`: no drop glue, `Send` and `Sync`, any bytes of
/// `defaults` a valid value.
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
