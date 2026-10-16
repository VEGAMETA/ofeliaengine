//! Component types, their run-time descriptors and the registry.

use crate::fxhash::FastMap;
use core::alloc::Layout;
use core::any::TypeId;
use engine_core::hash::state_hash64;

/// Largest supported component alignment: chunks are 64-byte aligned.
pub const MAX_COMPONENT_ALIGN: usize = 64;

/// Data attached to entities.
///
/// Implement it with [`component!`](crate::component) or, for plain data that may be
/// copied, hashed and rolled back byte-wise, [`pod_component!`](crate::pod_component).
///
/// ```
/// use engine_ecs::{bytemuck, component, pod_component};
///
/// #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
/// #[repr(C)]
/// struct Velocity {
///     x: f32,
///     y: f32,
/// }
/// pod_component!(Velocity);
///
/// struct Name(String);
/// component!(Name);
/// ```
pub trait Component: Send + Sync + 'static {
    /// Whether the type is plain data ([`Pod::of`] proves it at compile time).
    const POD: Pod = Pod::NO;
    /// The reflection description, for serialization, the inspector, scripting and
    /// replication (set by [`reflect_component!`](crate::reflect_component)).
    const TYPE_INFO: Option<&'static engine_reflect::TypeInfo> = None;
}

/// Proof that a component is plain data: no padding, no pointers, every bit pattern
/// valid. Such components are snapshotted, restored and hashed as bytes.
///
/// The only way to claim it is [`Pod::of`], which requires [`bytemuck::Pod`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pod(bool);

impl Pod {
    /// Not plain data (the default).
    pub const NO: Self = Self(false);

    /// Plain data, proven by `T: bytemuck::Pod`.
    pub const fn of<T: bytemuck::Pod>() -> Self {
        Self(true)
    }

    /// Whether this is the plain-data proof.
    pub const fn is_pod(self) -> bool {
        self.0
    }
}

/// Implements [`Component`] for types that are not plain data.
#[macro_export]
macro_rules! component {
    ($($t:ty),+ $(,)?) => {
        $(impl $crate::Component for $t {})+
    };
}

/// Implements [`Component`] for plain-data types (`bytemuck::Pod`).
#[macro_export]
macro_rules! pod_component {
    ($($t:ty),+ $(,)?) => {
        $(impl $crate::Component for $t {
            const POD: $crate::Pod = $crate::Pod::of::<$t>();
        })+
    };
}

/// Implements [`Component`] with its reflection description (`engine_reflect::Reflect`),
/// and lists the type among the program's reflected types ([`crate::reflected`]), so saves
/// find it by name. Add `pod` first for plain-data types:
/// `reflect_component!(pod Health, Velocity)`.
#[macro_export]
macro_rules! reflect_component {
    (pod $($t:ty),+ $(,)?) => {
        $(impl $crate::Component for $t {
            const POD: $crate::Pod = $crate::Pod::of::<$t>();
            const TYPE_INFO: ::core::option::Option<&'static $crate::reflect::TypeInfo> =
                ::core::option::Option::Some(<$t as $crate::reflect::Reflect>::TYPE_INFO);
        }
        $crate::__reflected_component!($t);)+
    };
    ($($t:ty),+ $(,)?) => {
        $(impl $crate::Component for $t {
            const TYPE_INFO: ::core::option::Option<&'static $crate::reflect::TypeInfo> =
                ::core::option::Option::Some(<$t as $crate::reflect::Reflect>::TYPE_INFO);
        }
        $crate::__reflected_component!($t);)+
    };
}

/// Index of a registered component type in its world.
///
/// Ids follow registration order and are not stable across worlds or runs; the
/// component's name is (see [`ComponentDesc::name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId(pub(crate) u32);

impl ComponentId {
    /// The raw index.
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Run-time description of a component type, for components defined outside Rust
/// (scripts) through [`World::register_dynamic`](crate::World::register_dynamic).
#[derive(Debug, Clone)]
pub struct ComponentDesc {
    /// Unique, stable name. Rust components use `core::any::type_name`. Archetype and
    /// hash order derive from names, so they must not depend on registration order.
    pub name: Box<str>,
    /// Size and alignment of one value (alignment at most [`MAX_COMPONENT_ALIGN`]).
    pub layout: Layout,
    /// Drops a value in place; `None` for types without drop glue.
    pub drop: Option<unsafe fn(*mut u8)>,
    /// Whether values are plain data (see [`Pod`]).
    pub pod: bool,
    /// Field description (script components build one at run time).
    pub type_info: Option<&'static engine_reflect::TypeInfo>,
}

/// Errors from registering components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// Another component already uses this name.
    DuplicateName(Box<str>),
    /// Alignment above [`MAX_COMPONENT_ALIGN`].
    AlignmentTooLarge(Box<str>),
}

impl core::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DuplicateName(name) => write!(f, "component name `{name}` is already registered"),
            Self::AlignmentTooLarge(name) => write!(
                f,
                "component `{name}` needs alignment above {MAX_COMPONENT_ALIGN} bytes"
            ),
        }
    }
}

impl std::error::Error for RegisterError {}

pub(crate) struct ComponentInfo {
    pub(crate) name: Box<str>,
    /// Stable hash of the name: the canonical sort key.
    pub(crate) key: u64,
    pub(crate) layout: Layout,
    pub(crate) drop: Option<unsafe fn(*mut u8)>,
    pub(crate) pod: bool,
    pub(crate) type_info: Option<&'static engine_reflect::TypeInfo>,
}

unsafe fn drop_as<T>(ptr: *mut u8) {
    // SAFETY: the caller passes a valid, initialized `T`.
    unsafe { core::ptr::drop_in_place(ptr.cast::<T>()) }
}

/// Registry of a world's component types ([`World::components`](crate::World::components)).
#[derive(Default)]
pub struct Components {
    pub(crate) infos: Vec<ComponentInfo>,
    by_type: FastMap<TypeId, ComponentId>,
    by_key: FastMap<u64, ComponentId>,
}

impl Components {
    #[inline]
    pub(crate) fn id_of<T: 'static>(&self) -> Option<ComponentId> {
        self.by_type.get(&TypeId::of::<T>()).copied()
    }

    #[inline]
    pub(crate) fn info(&self, id: ComponentId) -> &ComponentInfo {
        &self.infos[id.0 as usize]
    }

    /// Number of registered components.
    pub fn len(&self) -> usize {
        self.infos.len()
    }

    /// Whether no component is registered.
    pub fn is_empty(&self) -> bool {
        self.infos.is_empty()
    }

    /// The component's name, if `id` is registered.
    pub fn name(&self, id: ComponentId) -> Option<&str> {
        self.infos.get(id.0 as usize).map(|i| &*i.name)
    }

    /// Whether the component is plain data.
    pub fn is_pod(&self, id: ComponentId) -> bool {
        self.infos.get(id.0 as usize).is_some_and(|i| i.pod)
    }

    /// Size and alignment of one value.
    pub fn layout(&self, id: ComponentId) -> Option<Layout> {
        self.infos.get(id.0 as usize).map(|i| i.layout)
    }

    /// The component's reflection description, if it has one.
    pub fn type_info(&self, id: ComponentId) -> Option<&'static engine_reflect::TypeInfo> {
        self.infos.get(id.0 as usize).and_then(|i| i.type_info)
    }

    pub(crate) fn register<T: Component>(&mut self) -> ComponentId {
        if let Some(id) = self.id_of::<T>() {
            return id;
        }
        let desc = ComponentDesc {
            name: core::any::type_name::<T>().into(),
            layout: Layout::new::<T>(),
            drop: core::mem::needs_drop::<T>().then_some(drop_as::<T> as unsafe fn(*mut u8)),
            pod: T::POD.is_pod(),
            type_info: T::TYPE_INFO,
        };
        let id = match self.add(desc) {
            Ok(id) => id,
            // Rust type names are unique per type, and Rust types cannot be over-aligned
            // beyond what `#[repr(align)]` allows; both only fail for broken programs.
            Err(e) => panic!("registering a Rust component: {e}"),
        };
        self.by_type.insert(TypeId::of::<T>(), id);
        id
    }

    pub(crate) fn add(&mut self, desc: ComponentDesc) -> Result<ComponentId, RegisterError> {
        if desc.layout.align() > MAX_COMPONENT_ALIGN {
            return Err(RegisterError::AlignmentTooLarge(desc.name));
        }
        let key = state_hash64(desc.name.as_bytes());
        if self.by_key.contains_key(&key) {
            return Err(RegisterError::DuplicateName(desc.name));
        }
        let id = ComponentId(self.infos.len() as u32);
        self.by_key.insert(key, id);
        self.infos.push(ComponentInfo {
            name: desc.name,
            key,
            layout: desc.layout,
            drop: desc.drop,
            pod: desc.pod,
            type_info: desc.type_info,
        });
        Ok(id)
    }
}
