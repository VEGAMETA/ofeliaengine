//! Values in the binary (ADR 0104): written from a current type through its description,
//! in the order of its [`Node`]; read through the file's shape into a current type, by
//! field name, converting numbers between widths and enums by variant name.

use crate::SaveError;
use crate::schema::{Node, is_entity, prim_size};
use crate::wire::{Reader, write_varint};
use engine_ecs::Entity;
use engine_reflect::{Attrs, Primitive, TypeInfo, TypeKind, Value};

/// Writes the value at `ptr` of type `ty`; entity references become the save's entity
/// indices through `refs` (`u32::MAX`: none).
///
/// # Safety
///
/// `ptr` points to a valid value of `ty`.
pub(crate) unsafe fn write_value(
    out: &mut Vec<u8>,
    ty: &'static TypeInfo,
    ptr: *const u8,
    refs: &dyn Fn(Entity) -> u32,
) {
    if is_entity(ty) {
        // SAFETY: an `Entity` value.
        let e = unsafe { ptr.cast::<Entity>().read_unaligned() };
        out.extend_from_slice(&refs(e).to_le_bytes());
        return;
    }
    // SAFETY: for every arm, pointers stay inside the value as its description lays it
    // out; containers are read through their own functions.
    unsafe {
        match ty.kind {
            TypeKind::Primitive(p) | TypeKind::Enum { repr: p, .. } => {
                out.extend_from_slice(core::slice::from_raw_parts(ptr, prim_size(p)));
            }
            TypeKind::Struct(fields) => {
                for f in fields.iter().filter(|f| !f.attrs.skip) {
                    write_value(out, f.ty, ptr.add(f.offset), refs);
                }
            }
            TypeKind::Array { element, len } => {
                for i in 0..len {
                    write_value(out, element, ptr.add(i * element.size), refs);
                }
            }
            TypeKind::String => {
                let s = engine_reflect::read_str(ptr);
                write_varint(out, s.len() as u64);
                out.extend_from_slice(s.as_bytes());
            }
            TypeKind::List(list) => {
                let len = (list.len)(ptr);
                write_varint(out, len as u64);
                let items = (list.items)(ptr);
                for i in 0..len {
                    write_value(out, list.element, items.add(i * list.element.size), refs);
                }
            }
            TypeKind::Option(option) => {
                let inner = (option.get)(ptr);
                if inner.is_null() {
                    out.push(0);
                } else {
                    out.push(1);
                    write_value(out, option.inner, inner, refs);
                }
            }
            TypeKind::Map(map) => {
                write_varint(out, (map.len)(ptr) as u64);
                if map.ordered {
                    (map.for_each)(ptr, &mut |k, v| {
                        write_value(out, map.key, k, refs);
                        write_value(out, map.value, v, refs);
                    });
                } else {
                    // A stable order, so equal states write equal bytes: by the keys' bytes.
                    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                    (map.for_each)(ptr, &mut |k, v| {
                        let (mut kb, mut vb) = (Vec::new(), Vec::new());
                        write_value(&mut kb, map.key, k, refs);
                        write_value(&mut vb, map.value, v, refs);
                        entries.push((kb, vb));
                    });
                    entries.sort_unstable();
                    for (k, v) in entries {
                        out.extend_from_slice(&k);
                        out.extend_from_slice(&v);
                    }
                }
            }
            TypeKind::Opaque => {}
        }
    }
}

/// What reading values needs besides the bytes.
pub(crate) struct ReadContext<'a> {
    /// The entity of a save's entity index (`u32::MAX` and unknown ones: `Entity::NULL`).
    pub(crate) entity: &'a dyn Fn(u32) -> Entity,
    /// What did not fit the current types (fields gone, kinds changed, variants gone).
    pub(crate) notes: &'a mut Vec<String>,
}

impl ReadContext<'_> {
    fn note(&mut self, text: impl FnOnce() -> String) {
        // Enough to say what happened, without a damaged file filling memory.
        if self.notes.len() < 64 {
            self.notes.push(text());
        }
    }
}

/// Reads a value of shape `node` into the value at `ptr` of current type `ty`: fields by
/// name, numbers converted, variants by name, what does not fit skipped with a note.
///
/// # Safety
///
/// `ptr` points to a valid, writable value of `ty`.
pub(crate) unsafe fn read_value(
    r: &mut Reader<'_>,
    node: &Node,
    ty: &'static TypeInfo,
    attrs: &Attrs,
    ptr: *mut u8,
    cx: &mut ReadContext<'_>,
) -> Result<(), SaveError> {
    // SAFETY: for every arm, as the caller guarantees, pointers stay inside the value as
    // its description lays it out; containers are changed through their own functions.
    unsafe {
        match (node, &ty.kind) {
            (Node::Entity, _) if is_entity(ty) => {
                let e = (cx.entity)(r.u32()?);
                ptr.cast::<Entity>().write_unaligned(e);
            }
            (Node::Prim(p), TypeKind::Primitive(_)) => {
                let value = read_prim(r, *p)?;
                let _ = engine_reflect::write(ptr, ty, attrs, value);
            }
            (Node::Enum { repr, variants }, TypeKind::Enum { variants: now, .. }) => {
                let raw = read_prim(r, *repr)?;
                let raw = match raw {
                    Value::Int(i) => i,
                    Value::UInt(u) => u as i64,
                    other => other.as_f64() as i64,
                };
                let name = variants.iter().find(|(_, v)| *v == raw).map(|(n, _)| n);
                match name.and_then(|n| now.iter().find(|v| v.name == n)) {
                    Some(v) => {
                        let _ = engine_reflect::write(ptr, ty, attrs, Value::Int(v.value));
                    }
                    None => cx
                        .note(|| format!("`{}`: variant {raw} is gone; the default kept", ty.name)),
                }
            }
            (Node::Struct(fields), TypeKind::Struct(now)) => {
                for (name, field_node) in fields {
                    match now.iter().find(|f| f.name == name && !f.attrs.skip) {
                        Some(f) => {
                            read_value(r, field_node, f.ty, &f.attrs, ptr.add(f.offset), cx)?;
                        }
                        None => {
                            cx.note(|| format!("`{}.{name}` is gone; skipped", ty.name));
                            skip_value(r, field_node)?;
                        }
                    }
                }
            }
            (
                Node::Array(element, len),
                &TypeKind::Array {
                    element: now,
                    len: now_len,
                },
            ) => {
                for i in 0..*len as usize {
                    if i < now_len {
                        read_value(r, element, now, &now.attrs, ptr.add(i * now.size), cx)?;
                    } else {
                        skip_value(r, element)?;
                    }
                }
            }
            (Node::Str, TypeKind::String) => {
                let s = r.string()?;
                engine_reflect::write_str(ptr, s);
            }
            (Node::List(element), TypeKind::List(list)) => {
                let len = counted(r, element.min_size())?;
                (list.resize)(ptr, len);
                let items = (list.items_mut)(ptr);
                for i in 0..len {
                    let item = items.add(i * list.element.size);
                    read_value(r, element, list.element, &list.element.attrs, item, cx)?;
                }
            }
            (Node::Option(inner), TypeKind::Option(option)) => match r.u8()? {
                0 => {
                    (option.set)(ptr, false);
                }
                1 => {
                    let value = (option.set)(ptr, true);
                    read_value(r, inner, option.inner, &option.inner.attrs, value, cx)?;
                }
                _ => return Err(SaveError::damaged("an optional value's tag")),
            },
            (Node::Map(key, value), TypeKind::Map(map)) => {
                let len = counted(r, key.min_size() + value.min_size())?;
                (map.clear)(ptr);
                for _ in 0..len {
                    let mut failed = None;
                    let slot = (map.insert)(ptr, &mut |k| match read_value(
                        r,
                        key,
                        map.key,
                        &map.key.attrs,
                        k,
                        cx,
                    ) {
                        Ok(()) => true,
                        Err(e) => {
                            failed = Some(e);
                            false
                        }
                    });
                    if let Some(e) = failed {
                        return Err(e);
                    }
                    read_value(r, value, map.value, &map.value.attrs, slot, cx)?;
                }
            }
            (Node::Opaque, _) => {}
            // A primitive that became an enum, a list that became a number: keep the
            // default rather than guess.
            (node, _) => {
                cx.note(|| format!("`{}` changed its kind; the default kept", ty.name));
                skip_value(r, node)?;
            }
        }
    }
    Ok(())
}

/// A count of values of at least `min` bytes each, checked against what is left.
fn counted(r: &mut Reader<'_>, min: usize) -> Result<usize, SaveError> {
    let n = r.len()?;
    if n.saturating_mul(min.max(1)) > r.remaining() {
        return Err(SaveError::damaged("more values than bytes"));
    }
    Ok(n)
}

/// Moves past a value of shape `node`.
pub(crate) fn skip_value(r: &mut Reader<'_>, node: &Node) -> Result<(), SaveError> {
    match node {
        Node::Prim(p) | Node::Enum { repr: p, .. } => {
            r.bytes(prim_size(*p))?;
        }
        Node::Struct(fields) => {
            for (_, f) in fields {
                skip_value(r, f)?;
            }
        }
        Node::Array(element, len) => {
            for _ in 0..*len {
                skip_value(r, element)?;
            }
        }
        Node::Str => {
            let n = r.len()?;
            r.bytes(n)?;
        }
        Node::List(element) => {
            for _ in 0..r.len()? {
                skip_value(r, element)?;
            }
        }
        Node::Option(inner) => {
            if r.u8()? != 0 {
                skip_value(r, inner)?;
            }
        }
        Node::Map(key, value) => {
            for _ in 0..r.len()? {
                skip_value(r, key)?;
                skip_value(r, value)?;
            }
        }
        Node::Entity => {
            r.u32()?;
        }
        Node::Opaque => {}
    }
    Ok(())
}

pub(crate) fn read_prim(r: &mut Reader<'_>, p: Primitive) -> Result<Value, SaveError> {
    let b = r.bytes(prim_size(p))?;
    let mut w = [0u8; 8];
    w[..b.len()].copy_from_slice(b);
    let u = u64::from_le_bytes(w);
    Ok(match p {
        Primitive::Bool => Value::Bool(b[0] != 0),
        Primitive::I8 => Value::Int(i64::from(b[0] as i8)),
        Primitive::I16 => Value::Int(i64::from(u as u16 as i16)),
        Primitive::I32 => Value::Int(i64::from(u as u32 as i32)),
        Primitive::I64 => Value::Int(u as i64),
        Primitive::U8 | Primitive::U16 | Primitive::U32 | Primitive::U64 => Value::UInt(u),
        Primitive::F32 => Value::Float(f64::from(f32::from_bits(u as u32))),
        Primitive::F64 => Value::Float(f64::from_bits(u)),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use engine_reflect::Reflect;

    #[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    enum TeamV1 {
        Red = 1,
        Blue = 2,
    }

    #[derive(Reflect)]
    #[reflect(name = "Unit")]
    struct V1 {
        a: u32,
        b: u32,
        c: f32,
        team: TeamV1,
        tags: Vec<u32>,
    }

    #[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq, Default)]
    #[repr(u16)]
    enum TeamV2 {
        Blue = 5,
        Green = 6,
        #[default]
        Red = 7,
    }

    #[derive(Reflect, Default, Debug, PartialEq)]
    #[reflect(name = "Unit")]
    struct V2 {
        b: u16,
        c: f64,
        d: i32,
        team: TeamV2,
        /// Was a list.
        tags: u8,
    }

    #[test]
    fn a_changed_type_reads_its_old_values_by_name() {
        let old = V1 {
            a: 9,
            b: 70_000,
            c: 0.25,
            team: TeamV1::Blue,
            tags: vec![1, 2],
        };
        let mut bytes = Vec::new();
        let none = |_: Entity| u32::MAX;
        // SAFETY: `old` is a live `V1`.
        unsafe {
            write_value(
                &mut bytes,
                V1::TYPE_INFO,
                core::ptr::from_ref(&old).cast(),
                &none,
            )
        };
        let node = Node::of(V1::TYPE_INFO);
        let mut new = V2 {
            d: -4,
            ..V2::default()
        };
        let mut notes = Vec::new();
        let mut cx = ReadContext {
            entity: &|_| Entity::NULL,
            notes: &mut notes,
        };
        let mut r = Reader::new(&bytes);
        // SAFETY: `new` is a live `V2`.
        unsafe {
            read_value(
                &mut r,
                &node,
                V2::TYPE_INFO,
                &V2::TYPE_INFO.attrs,
                core::ptr::from_mut(&mut new).cast(),
                &mut cx,
            )
            .unwrap();
        }
        assert_eq!(r.remaining(), 0, "every byte read or skipped");
        assert_eq!(new.b, u16::MAX, "a narrower number saturates");
        assert_eq!(new.c, 0.25);
        assert_eq!(new.d, -4, "a new field keeps its value");
        assert_eq!(
            new.team,
            TeamV2::Blue,
            "a variant by name, whatever its number"
        );
        assert_eq!(new.tags, 0, "a list that became a number keeps the default");
        assert!(notes.iter().any(|n| n.contains("Unit.a")), "{notes:?}");
        assert!(
            notes.iter().any(|n| n.contains("changed its kind")),
            "{notes:?}"
        );
    }
}
