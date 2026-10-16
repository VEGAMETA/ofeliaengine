//! A type's shape as a save records it (ADR 0104): the binary's values carry no names, so
//! each saved type's fields are named once per file. Loading maps them to the current
//! type by name; the text form is written from the shape alone.

use crate::SaveError;
use crate::wire::{Reader, write_str, write_varint};
use engine_ecs::Entity;
use engine_reflect::{Primitive, Reflect, TypeInfo, TypeKind};

/// Deepest nesting a file may describe: real types are shallow; a damaged file must not
/// recurse without end.
const MAX_DEPTH: u32 = 32;

/// The shape of a saved value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    /// A number or a boolean, little-endian at its own width.
    Prim(Primitive),
    /// A C-like enum: its integer, and its variants' names to map renames and reorders.
    Enum {
        repr: Primitive,
        variants: Vec<(String, i64)>,
    },
    /// Fields in order, the skipped ones left out.
    Struct(Vec<(String, Node)>),
    /// A fixed array.
    Array(Box<Node>, u32),
    /// A string: its length and UTF-8 bytes.
    Str,
    /// A list: its length and elements.
    List(Box<Node>),
    /// An optional value: 0, or 1 and the value.
    Option(Box<Node>),
    /// A map: its length, then keys and values.
    Map(Box<Node>, Box<Node>),
    /// A reference to an entity: its place in the save's entity table, or `u32::MAX`.
    Entity,
    /// Not introspectable: nothing is written.
    Opaque,
}

/// Entity references are recognized by name and layout, as scenes do: `&'static`
/// constants are not guaranteed one address across crates.
pub(crate) fn is_entity(ty: &TypeInfo) -> bool {
    let e = <Entity as Reflect>::TYPE_INFO;
    ty.name == e.name && ty.size == e.size && ty.align == e.align
}

/// Vector-like structs (every field an `f32` named from `x y z w`): written as tuples.
pub(crate) fn vector_like(fields: &[(String, Node)]) -> bool {
    !fields.is_empty()
        && fields.len() <= 4
        && fields
            .iter()
            .zip(["x", "y", "z", "w"])
            .all(|((name, node), n)| name == n && *node == Node::Prim(Primitive::F32))
}

impl Node {
    /// The shape of the current type `ty`.
    pub(crate) fn of(ty: &'static TypeInfo) -> Node {
        if is_entity(ty) {
            return Node::Entity;
        }
        match ty.kind {
            TypeKind::Primitive(p) => Node::Prim(p),
            TypeKind::Enum { repr, variants } => Node::Enum {
                repr,
                variants: variants
                    .iter()
                    .map(|v| (v.name.to_owned(), v.value))
                    .collect(),
            },
            TypeKind::Struct(fields) => Node::Struct(
                fields
                    .iter()
                    .filter(|f| !f.attrs.skip)
                    .map(|f| (f.name.to_owned(), Node::of(f.ty)))
                    .collect(),
            ),
            TypeKind::Array { element, len } => {
                Node::Array(Box::new(Node::of(element)), len as u32)
            }
            TypeKind::String => Node::Str,
            TypeKind::List(list) => Node::List(Box::new(Node::of(list.element))),
            TypeKind::Option(option) => Node::Option(Box::new(Node::of(option.inner))),
            TypeKind::Map(map) => {
                Node::Map(Box::new(Node::of(map.key)), Box::new(Node::of(map.value)))
            }
            TypeKind::Opaque => Node::Opaque,
        }
    }

    /// The fewest bytes a value of this shape takes: a damaged length is caught before it
    /// allocates (a list of a million structs from a few bytes).
    pub(crate) fn min_size(&self) -> usize {
        match self {
            Node::Prim(p) | Node::Enum { repr: p, .. } => prim_size(*p),
            Node::Struct(fields) => fields.iter().map(|(_, f)| f.min_size()).sum(),
            Node::Array(element, len) => element.min_size().saturating_mul(*len as usize),
            Node::Str | Node::List(_) | Node::Option(_) | Node::Map(..) => 1,
            Node::Entity => 4,
            Node::Opaque => 0,
        }
    }

    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        match self {
            Node::Prim(p) => {
                out.push(0);
                out.push(prim_code(*p));
            }
            Node::Enum { repr, variants } => {
                out.push(1);
                out.push(prim_code(*repr));
                write_varint(out, variants.len() as u64);
                for (name, value) in variants {
                    write_str(out, name);
                    write_varint(out, zigzag(*value));
                }
            }
            Node::Struct(fields) => {
                out.push(2);
                write_varint(out, fields.len() as u64);
                for (name, node) in fields {
                    write_str(out, name);
                    node.write(out);
                }
            }
            Node::Array(element, len) => {
                out.push(3);
                write_varint(out, u64::from(*len));
                element.write(out);
            }
            Node::Str => out.push(4),
            Node::List(element) => {
                out.push(5);
                element.write(out);
            }
            Node::Option(inner) => {
                out.push(6);
                inner.write(out);
            }
            Node::Map(key, value) => {
                out.push(7);
                key.write(out);
                value.write(out);
            }
            Node::Entity => out.push(8),
            Node::Opaque => out.push(9),
        }
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Node, SaveError> {
        Self::read_at(r, 0)
    }

    fn read_at(r: &mut Reader<'_>, depth: u32) -> Result<Node, SaveError> {
        if depth > MAX_DEPTH {
            return Err(SaveError::damaged("a type nested too deeply"));
        }
        let next = |r: &mut Reader<'_>| Self::read_at(r, depth + 1).map(Box::new);
        Ok(match r.u8()? {
            0 => Node::Prim(prim_of(r.u8()?)?),
            1 => {
                let repr = prim_of(r.u8()?)?;
                let count = r.len()?;
                let mut variants = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    let name = r.string()?.to_owned();
                    variants.push((name, unzigzag(r.varint()?)));
                }
                Node::Enum { repr, variants }
            }
            2 => {
                let count = r.len()?;
                let mut fields = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    let name = r.string()?.to_owned();
                    fields.push((name, Self::read_at(r, depth + 1)?));
                }
                Node::Struct(fields)
            }
            3 => {
                let len = u32::try_from(r.varint()?)
                    .map_err(|_| SaveError::damaged("an array too long"))?;
                Node::Array(next(r)?, len)
            }
            4 => Node::Str,
            5 => Node::List(next(r)?),
            6 => Node::Option(next(r)?),
            7 => {
                let key = next(r)?;
                Node::Map(key, next(r)?)
            }
            8 => Node::Entity,
            9 => Node::Opaque,
            _ => return Err(SaveError::damaged("an unknown kind of value")),
        })
    }
}

pub(crate) fn prim_size(p: Primitive) -> usize {
    match p {
        Primitive::Bool | Primitive::I8 | Primitive::U8 => 1,
        Primitive::I16 | Primitive::U16 => 2,
        Primitive::I32 | Primitive::U32 | Primitive::F32 => 4,
        Primitive::I64 | Primitive::U64 | Primitive::F64 => 8,
    }
}

fn prim_code(p: Primitive) -> u8 {
    match p {
        Primitive::Bool => 0,
        Primitive::I8 => 1,
        Primitive::I16 => 2,
        Primitive::I32 => 3,
        Primitive::I64 => 4,
        Primitive::U8 => 5,
        Primitive::U16 => 6,
        Primitive::U32 => 7,
        Primitive::U64 => 8,
        Primitive::F32 => 9,
        Primitive::F64 => 10,
    }
}

fn prim_of(code: u8) -> Result<Primitive, SaveError> {
    Ok(match code {
        0 => Primitive::Bool,
        1 => Primitive::I8,
        2 => Primitive::I16,
        3 => Primitive::I32,
        4 => Primitive::I64,
        5 => Primitive::U8,
        6 => Primitive::U16,
        7 => Primitive::U32,
        8 => Primitive::U64,
        9 => Primitive::F32,
        10 => Primitive::F64,
        _ => return Err(SaveError::damaged("an unknown primitive")),
    })
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}
