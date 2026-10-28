//! Reflection: static descriptions of types and their fields (research note
//! `docs/research/reflection.md`, ADR 0022).
//!
//! `#[derive(Reflect)]` generates a [`TypeInfo`] for a struct or C-like enum:
//! - field names, byte offsets and types;
//! - attributes shared by serialization, the editor's inspector, scripting and network
//!   replication.
//!
//! Strings, lists (`Vec`), optional values and maps (`BTreeMap`, `HashMap`) are described
//! with functions for their contents (ADR 0104 step 1, [`containers`]).
//!
//! Access is pointer arithmetic on those offsets, not dynamic dispatch. Script-defined
//! types build the same description at run time.
//!
//! ```
//! use engine_reflect::{Reflect, Value, get, set};
//!
//! #[derive(Reflect, Default)]
//! struct Health {
//!     #[reflect(min = 0.0, max = 100.0, tooltip = "Hit points")]
//!     current: f32,
//!     #[reflect(net_skip)]
//!     regen_delay: f32,
//! }
//!
//! let mut h = Health::default();
//! set(&mut h, "current", Value::Float(250.0)).unwrap(); // clamped to the range
//! assert_eq!(h.current, 100.0);
//! assert_eq!(get(&h, "current"), Some(Value::Float(100.0)));
//! let field = Health::TYPE_INFO.field("current").unwrap();
//! assert_eq!(field.attrs.tooltip, Some("Hit points"));
//! ```

pub use engine_reflect_derive::Reflect;

// Lets the derive's `::engine_reflect` paths resolve inside this crate's own tests.
extern crate self as engine_reflect;

/// A description of a type.
#[derive(Debug)]
pub struct TypeInfo {
    /// Stable name, as written in files (the type's identifier unless renamed).
    pub name: &'static str,
    /// Size in bytes.
    pub size: usize,
    /// Alignment in bytes.
    pub align: usize,
    /// What the type is made of.
    pub kind: TypeKind,
    /// Type-level attributes (for example a color picker for a color type).
    pub attrs: Attrs,
}

/// What a type is made of.
#[derive(Debug)]
pub enum TypeKind {
    /// A number or a boolean.
    Primitive(Primitive),
    /// Named fields at offsets (tuple structs name them "0", "1", ...).
    Struct(&'static [Field]),
    /// A C-like enum stored as `repr`.
    Enum {
        /// The integer representation.
        repr: Primitive,
        /// The variants and their values.
        variants: &'static [Variant],
    },
    /// A fixed-size array.
    Array {
        /// The element type.
        element: &'static TypeInfo,
        /// Number of elements.
        len: usize,
    },
    /// A `String` ([`read_str`], [`write_str`]).
    String,
    /// A list, `Vec<T>`.
    List(&'static ListInfo),
    /// An optional value, `Option<T>`.
    Option(&'static OptionInfo),
    /// A map, `BTreeMap<K, V>` or `HashMap<K, V, S>`.
    Map(&'static MapInfo),
    /// Not introspectable (serialized through a hook, if at all).
    Opaque,
}

/// A primitive type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Primitive {
    /// `bool`.
    Bool,
    /// `i8`.
    I8,
    /// `i16`.
    I16,
    /// `i32`.
    I32,
    /// `i64`.
    I64,
    /// `u8`.
    U8,
    /// `u16`.
    U16,
    /// `u32`.
    U32,
    /// `u64`.
    U64,
    /// `f32`.
    F32,
    /// `f64`.
    F64,
}

/// A field of a struct.
#[derive(Debug)]
pub struct Field {
    /// Name (stable across code renames with `#[reflect(rename = "...")]`).
    pub name: &'static str,
    /// Byte offset inside the struct.
    pub offset: usize,
    /// The field's type.
    pub ty: &'static TypeInfo,
    /// Attributes.
    pub attrs: Attrs,
}

/// A variant of a C-like enum.
#[derive(Debug, Clone, Copy)]
pub struct Variant {
    /// Name.
    pub name: &'static str,
    /// Discriminant.
    pub value: i64,
}

/// How the editor presents a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hint {
    /// The default widget for the type.
    #[default]
    None,
    /// A color picker (three or four components).
    Color,
    /// An angle stored in radians, shown in degrees.
    Degrees,
    /// Multi-line text.
    Multiline,
    /// A sound file under the sounds folder, by its path (`ambience/hum.wav`): the
    /// editor's sound field (a file dropped on it, the `#sound:` list).
    Sound,
    /// A mixer bus, by name: the editor's list of the mixer's buses.
    Bus,
}

/// Attributes of a field (or type), shared by the four users of reflection.
#[derive(Debug, Clone, Copy, Default)]
pub struct Attrs {
    /// Smallest allowed value; writes through reflection clamp to it.
    pub min: Option<f64>,
    /// Largest allowed value; writes through reflection clamp to it.
    pub max: Option<f64>,
    /// Step for editor sliders and drags.
    pub step: Option<f64>,
    /// Help text for the inspector and generated script types.
    pub tooltip: Option<&'static str>,
    /// Not shown in the inspector.
    pub hidden: bool,
    /// Not saved or loaded. On a type: none of its values are (caches, effects, data made
    /// again on load).
    pub skip: bool,
    /// Network quantization step (ADR 0019); `None` sends full precision.
    pub net_quantum: Option<f64>,
    /// Not replicated.
    pub net_skip: bool,
    /// Editor presentation.
    pub hint: Hint,
    /// On a type: the version its saves record (ADR 0104), for migrations after a change
    /// of meaning; 0 when never set.
    pub version: u32,
}

impl Attrs {
    /// No attributes.
    pub const NONE: Attrs = Attrs {
        min: None,
        max: None,
        step: None,
        tooltip: None,
        hidden: false,
        skip: false,
        net_quantum: None,
        net_skip: false,
        hint: Hint::None,
        version: 0,
    };
}

/// Types with a static description.
pub trait Reflect: 'static {
    /// The description.
    const TYPE_INFO: &'static TypeInfo;
}

impl TypeInfo {
    /// The field called `name`, for structs.
    pub fn field(&self, name: &str) -> Option<&'static Field> {
        match self.kind {
            TypeKind::Struct(fields) => fields.iter().find(|f| f.name == name),
            _ => None,
        }
    }

    /// Resolves a dotted path (`"position.x"`, `"weights.2"`) to its byte offset, its
    /// type and the attributes of its last field.
    pub fn path(&'static self, path: &str) -> Option<(usize, &'static TypeInfo, Attrs)> {
        let mut offset = 0;
        let mut ty: &'static TypeInfo = self;
        let mut attrs = self.attrs;
        if path.is_empty() {
            return Some((0, self, attrs));
        }
        for part in path.split('.') {
            match ty.kind {
                TypeKind::Struct(fields) => {
                    let f = fields.iter().find(|f| f.name == part)?;
                    offset += f.offset;
                    ty = f.ty;
                    attrs = f.attrs;
                }
                TypeKind::Array { element, len } => {
                    let i: usize = part.parse().ok()?;
                    if i >= len {
                        return None;
                    }
                    offset += i * element.size;
                    ty = element;
                    attrs = element.attrs;
                }
                _ => return None,
            }
        }
        Some((offset, ty, attrs))
    }

    /// Calls `f` with the path, offset, type and attributes of every primitive or enum
    /// inside this type, depth first in declaration order (serialization, network
    /// field lists). Opaque parts and containers (strings, lists, optional values, maps:
    /// no fixed offsets) are skipped.
    pub fn visit_leaves(&'static self, f: &mut impl FnMut(&str, usize, &'static TypeInfo, Attrs)) {
        let mut path = String::new();
        visit(self, 0, self.attrs, &mut path, f);
    }
}

fn visit(
    ty: &'static TypeInfo,
    offset: usize,
    attrs: Attrs,
    path: &mut String,
    f: &mut impl FnMut(&str, usize, &'static TypeInfo, Attrs),
) {
    match ty.kind {
        TypeKind::Primitive(_) | TypeKind::Enum { .. } => f(path, offset, ty, attrs),
        TypeKind::Struct(fields) => {
            for field in fields {
                let len = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(field.name);
                visit(field.ty, offset + field.offset, field.attrs, path, f);
                path.truncate(len);
            }
        }
        TypeKind::Array { element, len } => {
            for i in 0..len {
                let old = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(&i.to_string());
                visit(element, offset + i * element.size, attrs, path, f);
                path.truncate(old);
            }
        }
        TypeKind::String
        | TypeKind::List(_)
        | TypeKind::Option(_)
        | TypeKind::Map(_)
        | TypeKind::Opaque => {}
    }
}

/// A primitive value read or written through reflection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// A boolean.
    Bool(bool),
    /// A signed integer (also enum values).
    Int(i64),
    /// An unsigned integer.
    UInt(u64),
    /// A floating-point number.
    Float(f64),
}

impl Value {
    /// The value as a float, for any numeric kind.
    pub fn as_f64(self) -> f64 {
        match self {
            Value::Bool(b) => f64::from(u8::from(b)),
            Value::Int(i) => i as f64,
            Value::UInt(u) => u as f64,
            Value::Float(f) => f,
        }
    }
}

/// Errors from writing through reflection.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteError {
    /// No field at this path.
    NoSuchPath,
    /// The path names a struct, array or opaque value, not a primitive.
    NotAPrimitive,
    /// The value is not a variant of the enum.
    NoSuchVariant(i64),
}

impl core::fmt::Display for WriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchPath => write!(f, "no field at this path"),
            Self::NotAPrimitive => write!(f, "the path does not name a primitive value"),
            Self::NoSuchVariant(v) => write!(f, "{v} is not a variant of the enum"),
        }
    }
}

impl std::error::Error for WriteError {}

/// Reads the primitive or enum at `ptr`.
///
/// # Safety
///
/// `ptr` points to a valid, initialized value of type `ty`.
pub unsafe fn read(ptr: *const u8, ty: &TypeInfo) -> Option<Value> {
    // SAFETY: the caller guarantees a valid value of `ty`; primitives are read at their
    // own type, unaligned reads avoid assuming the pointer's alignment.
    unsafe {
        let p = match ty.kind {
            TypeKind::Primitive(p) => p,
            TypeKind::Enum { repr, .. } => repr,
            _ => return None,
        };
        Some(match p {
            Primitive::Bool => Value::Bool(ptr.read() != 0),
            Primitive::I8 => Value::Int(i64::from(ptr.cast::<i8>().read_unaligned())),
            Primitive::I16 => Value::Int(i64::from(ptr.cast::<i16>().read_unaligned())),
            Primitive::I32 => Value::Int(i64::from(ptr.cast::<i32>().read_unaligned())),
            Primitive::I64 => Value::Int(ptr.cast::<i64>().read_unaligned()),
            Primitive::U8 => Value::UInt(u64::from(ptr.read())),
            Primitive::U16 => Value::UInt(u64::from(ptr.cast::<u16>().read_unaligned())),
            Primitive::U32 => Value::UInt(u64::from(ptr.cast::<u32>().read_unaligned())),
            Primitive::U64 => Value::UInt(ptr.cast::<u64>().read_unaligned()),
            Primitive::F32 => Value::Float(f64::from(ptr.cast::<f32>().read_unaligned())),
            Primitive::F64 => Value::Float(ptr.cast::<f64>().read_unaligned()),
        })
    }
}

/// Writes `value` to the primitive or enum at `ptr`, converting between numeric kinds
/// (saturating), clamping to `attrs`' range, and refusing values that are not variants
/// of an enum.
///
/// # Safety
///
/// `ptr` points to a valid, writable value of type `ty`.
pub unsafe fn write(
    ptr: *mut u8,
    ty: &TypeInfo,
    attrs: &Attrs,
    value: Value,
) -> Result<(), WriteError> {
    let (p, variants) = match ty.kind {
        TypeKind::Primitive(p) => (p, None),
        TypeKind::Enum { repr, variants } => (repr, Some(variants)),
        _ => return Err(WriteError::NotAPrimitive),
    };
    let mut v = value.as_f64();
    if let Some(min) = attrs.min {
        v = v.max(min);
    }
    if let Some(max) = attrs.max {
        v = v.min(max);
    }
    let exact_int = match value {
        Value::Int(i) if attrs.min.is_none() && attrs.max.is_none() => Some(i),
        Value::UInt(u) if attrs.min.is_none() && attrs.max.is_none() => i64::try_from(u).ok(),
        _ => None,
    };
    if let Some(variants) = variants {
        let wanted = exact_int.unwrap_or(v as i64);
        if !variants.iter().any(|var| var.value == wanted) {
            return Err(WriteError::NoSuchVariant(wanted));
        }
    }
    let int = |lo: f64, hi: f64| -> i64 { exact_int.unwrap_or(v.round().clamp(lo, hi) as i64) };
    // SAFETY: the caller guarantees a writable value of `ty`; every write stores a valid
    // value of the primitive (bools as 0 or 1, enums only as existing variants).
    unsafe {
        match p {
            Primitive::Bool => ptr.write(u8::from(v != 0.0)),
            Primitive::I8 => ptr
                .cast::<i8>()
                .write_unaligned(int(-128.0, 127.0).clamp(-128, 127) as i8),
            Primitive::I16 => ptr
                .cast::<i16>()
                .write_unaligned(int(-32768.0, 32767.0).clamp(-32768, 32767) as i16),
            Primitive::I32 => ptr.cast::<i32>().write_unaligned(
                int(f64::from(i32::MIN), f64::from(i32::MAX))
                    .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            ),
            Primitive::I64 => ptr
                .cast::<i64>()
                .write_unaligned(int(i64::MIN as f64, i64::MAX as f64)),
            Primitive::U8 => ptr.write(int(0.0, 255.0).clamp(0, 255) as u8),
            Primitive::U16 => ptr
                .cast::<u16>()
                .write_unaligned(int(0.0, 65535.0).clamp(0, 65535) as u16),
            Primitive::U32 => {
                ptr.cast::<u32>().write_unaligned(
                    int(0.0, f64::from(u32::MAX)).clamp(0, i64::from(u32::MAX)) as u32,
                )
            }
            Primitive::U64 => {
                let u = match value {
                    Value::UInt(u) if attrs.min.is_none() && attrs.max.is_none() => u,
                    _ => v.round().max(0.0) as u64,
                };
                ptr.cast::<u64>().write_unaligned(u);
            }
            Primitive::F32 => ptr.cast::<f32>().write_unaligned(v as f32),
            Primitive::F64 => ptr.cast::<f64>().write_unaligned(v),
        }
    }
    Ok(())
}

/// Reads the value at `path` of `object`.
pub fn get<T: Reflect>(object: &T, path: &str) -> Option<Value> {
    let (offset, ty, _) = T::TYPE_INFO.path(path)?;
    // SAFETY: `path` resolved inside `T`'s description, so the offset points to a valid
    // value of `ty` within `object`.
    unsafe { read(core::ptr::from_ref(object).cast::<u8>().add(offset), ty) }
}

/// Writes `value` at `path` of `object` (see [`write()`] for conversions and clamping).
pub fn set<T: Reflect>(object: &mut T, path: &str, value: Value) -> Result<(), WriteError> {
    let (offset, ty, attrs) = T::TYPE_INFO.path(path).ok_or(WriteError::NoSuchPath)?;
    // SAFETY: as in `get`; `&mut T` makes the write exclusive, and `write` only stores
    // valid values of the field's type.
    unsafe {
        write(
            core::ptr::from_mut(object).cast::<u8>().add(offset),
            ty,
            &attrs,
            value,
        )
    }
}

macro_rules! primitive {
    ($($t:ty => $p:ident),+ $(,)?) => {
        $(impl Reflect for $t {
            const TYPE_INFO: &'static TypeInfo = &TypeInfo {
                name: stringify!($t),
                size: core::mem::size_of::<$t>(),
                align: core::mem::align_of::<$t>(),
                kind: TypeKind::Primitive(Primitive::$p),
                attrs: Attrs::NONE,
            };
        })+
    };
}

primitive!(
    bool => Bool, i8 => I8, i16 => I16, i32 => I32, i64 => I64,
    u8 => U8, u16 => U16, u32 => U32, u64 => U64, f32 => F32, f64 => F64,
);

impl<T: Reflect, const N: usize> Reflect for [T; N] {
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "array",
        size: core::mem::size_of::<[T; N]>(),
        align: core::mem::align_of::<[T; N]>(),
        kind: TypeKind::Array {
            element: T::TYPE_INFO,
            len: N,
        },
        attrs: Attrs::NONE,
    };
}

pub mod containers;
mod math;

pub use containers::{ListInfo, MapInfo, OptionInfo, read_str, write_str};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_core::math::{Mat4, Quat, Vec3, Vec3A, Vec4};

    #[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    enum Team {
        Red = 1,
        #[reflect(rename = "Azure")]
        Blue = 7,
    }

    #[derive(Reflect)]
    struct Stats(u16, #[reflect(min = -5, max = 5)] i32);

    #[derive(Reflect)]
    #[reflect(name = "Soldier")]
    struct Unit {
        position: Vec3,
        #[reflect(
            min = 0.0,
            max = 100.0,
            step = 0.5,
            tooltip = "Hit points",
            net_quantum = 0.1
        )]
        health: f32,
        team: Team,
        #[reflect(skip, hidden)]
        cached: u64,
        stats: Stats,
        weights: [f32; 3],
        #[reflect(hint = "degrees", rename = "heading")]
        yaw: f64,
        alive: bool,
    }

    fn unit() -> Unit {
        Unit {
            position: Vec3::new(1.0, 2.0, 3.0),
            health: 50.0,
            team: Team::Red,
            cached: 9,
            stats: Stats(3, 0),
            weights: [0.5, 0.25, 0.125],
            yaw: 0.0,
            alive: true,
        }
    }

    #[test]
    fn derived_descriptions_match_the_layout() {
        let info = Unit::TYPE_INFO;
        assert_eq!(info.name, "Soldier");
        assert_eq!(info.size, core::mem::size_of::<Unit>());
        let health = info.field("health").unwrap();
        assert_eq!(health.offset, core::mem::offset_of!(Unit, health));
        assert_eq!(health.attrs.max, Some(100.0));
        assert_eq!(health.attrs.step, Some(0.5));
        assert_eq!(health.attrs.net_quantum, Some(0.1));
        assert_eq!(health.attrs.tooltip, Some("Hit points"));
        assert!(info.field("cached").unwrap().attrs.skip);
        assert_eq!(info.field("heading").unwrap().attrs.hint, Hint::Degrees);
        assert!(info.field("yaw").is_none());
        match Team::TYPE_INFO.kind {
            TypeKind::Enum { repr, variants } => {
                assert_eq!(repr, Primitive::U8);
                assert_eq!(variants[1].name, "Azure");
                assert_eq!(variants[1].value, 7);
            }
            _ => panic!("not an enum"),
        }
    }

    #[test]
    fn paths_read_and_write_with_clamping_and_validation() {
        let mut u = unit();
        assert_eq!(get(&u, "position.y"), Some(Value::Float(2.0)));
        assert_eq!(get(&u, "weights.2"), Some(Value::Float(0.125)));
        assert_eq!(get(&u, "stats.0"), Some(Value::UInt(3)));
        assert_eq!(get(&u, "team"), Some(Value::UInt(1)));
        assert_eq!(get(&u, "alive"), Some(Value::Bool(true)));
        assert_eq!(get(&u, "weights.3"), None);
        assert_eq!(get(&u, "position"), None, "not a primitive");

        set(&mut u, "health", Value::Float(-3.0)).unwrap();
        assert_eq!(u.health, 0.0);
        set(&mut u, "stats.1", Value::Int(99)).unwrap();
        assert_eq!(u.stats.1, 5);
        set(&mut u, "team", Value::Int(7)).unwrap();
        assert_eq!(u.team, Team::Blue);
        assert_eq!(
            set(&mut u, "team", Value::Int(3)),
            Err(WriteError::NoSuchVariant(3))
        );
        assert_eq!(u.team, Team::Blue, "a refused write leaves the value alone");
        set(&mut u, "position.z", Value::Int(-4)).unwrap();
        assert_eq!(u.position.z, -4.0);
        set(&mut u, "alive", Value::Float(0.0)).unwrap();
        assert!(!u.alive);
        set(&mut u, "stats.0", Value::Float(1e9)).unwrap();
        assert_eq!(u.stats.0, u16::MAX, "saturates");
        assert_eq!(
            set(&mut u, "nope", Value::Int(0)),
            Err(WriteError::NoSuchPath)
        );
        assert_eq!(
            set(&mut u, "position", Value::Int(0)),
            Err(WriteError::NotAPrimitive)
        );
    }

    #[test]
    fn leaves_are_visited_in_declaration_order() {
        let mut paths = Vec::new();
        Unit::TYPE_INFO.visit_leaves(&mut |path, offset, ty, attrs| {
            paths.push((path.to_string(), offset, ty.name, attrs.skip));
        });
        let names: Vec<&str> = paths.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "position.x",
                "position.y",
                "position.z",
                "health",
                "team",
                "cached",
                "stats.0",
                "stats.1",
                "weights.0",
                "weights.1",
                "weights.2",
                "heading",
                "alive",
            ]
        );
        let u = unit();
        // Every leaf offset reads the right value.
        for (path, offset, _, _) in &paths {
            let (o, _, _) = Unit::TYPE_INFO.path(path).unwrap();
            assert_eq!(o, *offset, "{path}");
        }
        assert_eq!(get(&u, "cached"), Some(Value::UInt(9)));
        assert!(paths.iter().any(|p| p.0 == "cached" && p.3));
    }

    #[test]
    fn simd_math_types_are_described_by_their_layout() {
        let v = Vec4::new(1.0, 2.0, 3.0, 4.0);
        let q = Quat::from_xyzw(0.1, 0.2, 0.3, 0.4);
        let a = Vec3A::new(5.0, 6.0, 7.0);
        let m = Mat4::from_cols_array(&core::array::from_fn(|i| i as f32));
        assert_eq!(get(&v, "w"), Some(Value::Float(4.0)));
        assert_eq!(get(&q, "z"), Some(Value::Float(f64::from(0.3f32))));
        assert_eq!(get(&a, "y"), Some(Value::Float(6.0)));
        // Column 2, row 1 of a column-major matrix is element 9.
        assert_eq!(get(&m, "2.1"), Some(Value::Float(9.0)));
    }
}
