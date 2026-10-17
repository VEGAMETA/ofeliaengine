//! The line grammar scenes (ADR 0029, ADR 0108) and the text form of saves (ADR 0104)
//! share: quoting, splitting a line and its items, paths through structs, arrays, lists,
//! maps and optional values, and values read into reflected types.
//!
//! - **Paths:** `field.sub`; list and array indices (`items.0`); map keys in brackets
//!   (`flags["met the smith"]`, `counts[9]`). Optional values go straight to their value's
//!   fields, and a struct with one unnamed field (`Name(String)`) to that field's.
//! - **Values:**
//!   - numbers, `true`, `false`, enum variants by name;
//!   - `"strings"` with `\"`, `\\`, `\n`, `\r`, `\t` and `\u{…}`;
//!   - `@<label>` or `@none` for entities;
//!   - `(x, y, z)` for vectors;
//!   - `[a, b]` for arrays and lists of single values, `[]` when empty;
//!   - `{}` for an empty map or a value with no fields;
//!   - `none` and `some` for optional values;
//!   - `asset("path#part", <32 hex digits>)` or `none` for asset references.

use crate::{AssetId, AssetRef};
use engine_ecs::Entity;
use engine_reflect::{Attrs, Field, Primitive, Reflect, TypeInfo, TypeKind, Value};
use std::fmt::Write as _;

/// What reading a value needs from whoever reads it.
pub trait ReadRefs {
    /// The entity an `@…` reference names; `label` is the text after `@`.
    ///
    /// # Errors
    ///
    /// When the reader refuses the reference (a save naming an entity it does not hold).
    fn entity(&mut self, label: &str) -> Result<Entity, String>;

    /// An asset reference was read, with the path it was written with (for writing it
    /// back the same way).
    fn asset(&mut self, asset: AssetRef, path: &str) {
        let _ = (asset, path);
    }
}

/// `s` as a quoted string.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    quote_into(&mut out, s);
    out
}

/// Appends `s` as a quoted string.
pub fn quote_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The text inside a quoted string.
///
/// # Errors
///
/// When `text` is not one quoted string or holds a bad escape.
pub fn unquote(text: &str) -> Result<String, String> {
    let inner = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .ok_or_else(|| format!("expected a \"string\", found `{text}`"))?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let rest: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let code = rest
                    .strip_prefix('{')
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .and_then(char::from_u32)
                    .ok_or_else(|| format!("a bad \\u{{…}} escape in `{text}`"))?;
                out.push(code);
            }
            _ => return Err(format!("a bad escape in `{text}`")),
        }
    }
    Ok(out)
}

/// The first `=` outside a string: a line's path and value.
pub fn split_assignment(line: &str) -> Option<(&str, &str)> {
    let first = line.bytes().position(|b| b == b'=' || b == b'"')?;
    if line.as_bytes()[first] == b'=' {
        return Some((line[..first].trim(), line[first + 1..].trim()));
    }
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_str => escaped = true,
            '"' => in_str = !in_str,
            '=' if !in_str => return Some((line[..i].trim(), line[i + 1..].trim())),
            _ => {}
        }
    }
    None
}

/// Splits `a, (b, c), "d, e"` at its top-level commas.
pub fn split_items(inner: &str) -> Vec<&str> {
    if !inner.contains(['"', '(', '[', '{']) {
        let mut items: Vec<&str> = inner.split(',').map(str::trim).collect();
        if items.last().is_some_and(|s| s.is_empty()) {
            items.pop();
        }
        return items;
    }
    let mut items = Vec::new();
    let (mut depth, mut in_str, mut escaped, mut start) = (0i32, false, false, 0);
    for (i, c) in inner.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_str => escaped = true,
            '"' => in_str = !in_str,
            '(' | '[' | '{' if !in_str => depth += 1,
            ')' | ']' | '}' if !in_str => depth -= 1,
            ',' if !in_str && depth == 0 => {
                items.push(inner[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        items.push(last);
    }
    items
}

/// The head of a component's path: its name, then the rest without the dot.
pub fn component_of(path: &str) -> (&str, &str) {
    let end = path
        .bytes()
        .position(|b| b == b'.' || b == b'[')
        .unwrap_or(path.len());
    let rest = &path[end..];
    (&path[..end], rest.strip_prefix('.').unwrap_or(rest))
}

/// A step of a text path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seg<'a> {
    /// A field's name, or a list or array index.
    Name(&'a str),
    /// A map key, as written between the brackets.
    Key(&'a str),
}

/// The steps of a path.
///
/// # Errors
///
/// When a `[` is not closed.
pub fn segments(path: &str) -> Result<Vec<Seg<'_>>, String> {
    let mut out = Vec::new();
    segments_into(path, &mut out)?;
    Ok(out)
}

/// The steps of a path, into a vector kept between lines.
///
/// # Errors
///
/// When a `[` is not closed.
pub fn segments_into<'a>(path: &'a str, out: &mut Vec<Seg<'a>>) -> Result<(), String> {
    out.clear();
    if !path.contains('[') {
        out.extend(path.split('.').filter(|p| !p.is_empty()).map(Seg::Name));
        return Ok(());
    }
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            rest = r;
        } else if let Some(r) = rest.strip_prefix('[') {
            // The key ends at the first `]` outside a string.
            let (mut in_str, mut escaped) = (false, false);
            let end = r
                .char_indices()
                .find(|&(_, c)| match c {
                    _ if escaped => {
                        escaped = false;
                        false
                    }
                    '\\' if in_str => {
                        escaped = true;
                        false
                    }
                    '"' => {
                        in_str = !in_str;
                        false
                    }
                    ']' => !in_str,
                    _ => false,
                })
                .map(|(i, _)| i)
                .ok_or_else(|| format!("an unclosed `[` in `{path}`"))?;
            out.push(Seg::Key(&r[..end]));
            rest = &r[end + 1..];
        } else {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            out.push(Seg::Name(&rest[..end]));
            rest = &rest[end..];
        }
    }
    Ok(())
}

/// Entity references are recognized by name and layout: `&'static` constants are not
/// guaranteed to have one address across crates, so pointer comparison is unreliable.
pub fn is_entity(ty: &TypeInfo) -> bool {
    let e = <Entity as Reflect>::TYPE_INFO;
    ty.size == e.size && ty.align == e.align && ty.name == e.name
}

/// Whether `ty` is an [`AssetRef`] (by name and layout, as [`is_entity`]).
pub fn is_asset_ref(ty: &TypeInfo) -> bool {
    let a = <AssetRef as Reflect>::TYPE_INFO;
    ty.size == a.size && ty.align == a.align && ty.name == a.name
}

/// The fields of a struct written as a tuple: all `f32`, named from `x y z w`.
pub fn vector_fields(ty: &TypeInfo) -> Option<&'static [Field]> {
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

/// The one field of a struct written as that field (`Name(String)`: `Name = "Door"`).
pub fn newtype_field(ty: &TypeInfo) -> Option<&'static Field> {
    match ty.kind {
        TypeKind::Struct([field])
            if field.name == "0" && !field.attrs.skip && !is_entity(ty) && !is_asset_ref(ty) =>
        {
            Some(field)
        }
        _ => None,
    }
}

/// Whether a value is written on one line: numbers, variants, strings, entities, asset
/// references, vectors, and structs of one unnamed field holding one of these.
pub fn is_single(ty: &TypeInfo) -> bool {
    match ty.kind {
        TypeKind::Primitive(_) | TypeKind::Enum { .. } | TypeKind::String => true,
        TypeKind::Struct(_) => {
            is_entity(ty)
                || is_asset_ref(ty)
                || vector_fields(ty).is_some()
                || newtype_field(ty).is_some_and(|f| is_single(f.ty))
        }
        _ => false,
    }
}

/// Whether a path step can index `ty` by number.
fn indexable(ty: &TypeInfo) -> bool {
    matches!(ty.kind, TypeKind::Array { .. } | TypeKind::List(_))
}

/// Lists grown by a line's index stop here: a typo must not allocate gigabytes.
pub const MAX_TEXT_LIST_BYTES: usize = 256 << 20;

/// Follows a path into a value: the type, attributes and address it ends at. `None` when
/// a field is gone (the type changed since the text was written).
///
/// # Errors
///
/// When a step does not fit the type (an index into a struct, a key into a list).
///
/// # Safety
///
/// `ptr` points to a valid, writable value of `ty`.
pub unsafe fn resolve(
    segs: &[Seg<'_>],
    mut ty: &'static TypeInfo,
    mut ptr: *mut u8,
    refs: &mut dyn ReadRefs,
) -> Result<Option<(&'static TypeInfo, Attrs, *mut u8)>, String> {
    let mut attrs = ty.attrs;
    let mut i = 0;
    while i < segs.len() {
        let seg = segs[i];
        // SAFETY: for the block, every step stays inside the value its description lays
        // out; containers are changed through their own functions.
        unsafe {
            // An optional value's fields are its value's.
            while let TypeKind::Option(option) = ty.kind {
                ptr = (option.set)(ptr, true);
                ty = option.inner;
            }
            if let Some(f) = newtype_field(ty) {
                ptr = ptr.add(f.offset);
                attrs = f.attrs;
                // Format 1 wrote the field as `.0`; format 2 leaves it out.
                let named = seg == Seg::Name("0") && !indexable(f.ty);
                ty = f.ty;
                if named {
                    i += 1;
                }
                continue;
            }
            match (seg, &ty.kind) {
                (Seg::Name(name), TypeKind::Struct(fields)) => {
                    let Some(f) = fields.iter().find(|f| f.name == name && !f.attrs.skip) else {
                        return Ok(None);
                    };
                    ptr = ptr.add(f.offset);
                    ty = f.ty;
                    attrs = f.attrs;
                }
                (Seg::Name(index), &TypeKind::Array { element, len }) => {
                    let i: usize = index
                        .parse()
                        .ok()
                        .filter(|&i| i < len)
                        .ok_or_else(|| format!("`{index}` is not an index below {len}"))?;
                    ptr = ptr.add(i * element.size);
                    ty = element;
                    attrs = element.attrs;
                }
                (Seg::Name(index), TypeKind::List(list)) => {
                    let i: usize = index
                        .parse()
                        .ok()
                        .filter(|&i: &usize| {
                            i.saturating_mul(list.element.size.max(1)) < MAX_TEXT_LIST_BYTES
                        })
                        .ok_or_else(|| format!("`{index}` is not a list index"))?;
                    if i >= (list.len)(ptr) {
                        (list.resize)(ptr, i + 1);
                    }
                    ptr = (list.items_mut)(ptr).add(i * list.element.size);
                    ty = list.element;
                    attrs = ty.attrs;
                }
                (Seg::Key(key), TypeKind::Map(map)) => {
                    let mut bad = None;
                    let slot = (map.insert)(ptr, &mut |k| match set_text(
                        map.key,
                        &map.key.attrs,
                        k,
                        key,
                        refs,
                    ) {
                        Ok(()) => true,
                        Err(e) => {
                            bad = Some(e);
                            false
                        }
                    });
                    if let Some(e) = bad {
                        return Err(e);
                    }
                    ptr = slot;
                    ty = map.value;
                    attrs = ty.attrs;
                }
                (Seg::Name(name), _) => {
                    return Err(format!("`{name}` is not a part of `{}`", ty.name));
                }
                (Seg::Key(key), _) => {
                    return Err(format!("`[{key}]`: `{}` is not a map", ty.name));
                }
            }
        }
        i += 1;
    }
    Ok(Some((ty, attrs, ptr)))
}

/// An asset reference's text: `asset("path#part", <id>)`; `None` for `none`.
///
/// # Errors
///
/// When the text is neither.
pub fn parse_asset(text: &str) -> Result<Option<(String, AssetId)>, String> {
    if text == "none" {
        return Ok(None);
    }
    let bad = || format!("expected `asset(\"path\", <id>)` or `none`, found `{text}`");
    let inner = text
        .strip_prefix("asset(")
        .and_then(|t| t.strip_suffix(')'))
        .ok_or_else(bad)?;
    let items = split_items(inner);
    let [path, id] = items.as_slice() else {
        return Err(bad());
    };
    let id = AssetId::parse(id).ok_or_else(bad)?;
    Ok(Some((unquote(path)?, id)))
}

/// Writes a line's value text into the value at `ptr` of type `ty`.
///
/// # Errors
///
/// When the text is not a value of the type.
///
/// # Safety
///
/// `ptr` points to a valid, writable value of `ty`.
pub unsafe fn set_text(
    ty: &'static TypeInfo,
    attrs: &Attrs,
    ptr: *mut u8,
    text: &str,
    refs: &mut dyn ReadRefs,
) -> Result<(), String> {
    // SAFETY: for the block, as the caller guarantees; containers are changed through
    // their own functions, elements written inside their storage.
    unsafe {
        if matches!(ty.kind, TypeKind::Struct(_)) {
            return set_struct_text(ty, ptr, text, refs);
        }
        match ty.kind {
            TypeKind::Option(option) => match text {
                "none" => {
                    (option.set)(ptr, false);
                }
                "some" => {
                    (option.set)(ptr, true);
                }
                other => {
                    let inner = (option.set)(ptr, true);
                    set_text(option.inner, &option.inner.attrs, inner, other, refs)?;
                }
            },
            TypeKind::Primitive(p) => set_primitive(ty, p, attrs, ptr, text)?,
            TypeKind::Enum { variants, .. } => {
                let value = match variants.iter().find(|v| v.name == text) {
                    Some(v) => v.value,
                    None => text
                        .parse()
                        .map_err(|_| format!("`{text}` is not a variant of `{}`", ty.name))?,
                };
                engine_reflect::write(ptr, ty, attrs, Value::Int(value))
                    .map_err(|e| e.to_string())?;
            }
            TypeKind::String => engine_reflect::write_str(ptr, &unquote(text)?),
            TypeKind::Array { element, len } => {
                let items = text
                    .strip_prefix('[')
                    .and_then(|t| t.strip_suffix(']'))
                    .map(split_items)
                    .ok_or_else(|| format!("expected `[…]`, found `{text}`"))?;
                for (i, item) in items.into_iter().enumerate().take(len) {
                    set_text(
                        element,
                        &element.attrs,
                        ptr.add(i * element.size),
                        item,
                        refs,
                    )?;
                }
            }
            TypeKind::List(list) => {
                let items = text
                    .strip_prefix('[')
                    .and_then(|t| t.strip_suffix(']'))
                    .map(split_items)
                    .ok_or_else(|| format!("expected `[…]`, found `{text}`"))?;
                (list.resize)(ptr, items.len());
                let base = (list.items_mut)(ptr);
                for (i, item) in items.into_iter().enumerate() {
                    let el = base.add(i * list.element.size);
                    set_text(list.element, &list.element.attrs, el, item, refs)?;
                }
            }
            TypeKind::Map(map) if text == "{}" => (map.clear)(ptr),
            _ => return Err(format!("`{text}` is not a value of `{}`", ty.name)),
        }
    }
    Ok(())
}

/// [`set_text`] for structs: entities, asset references, vectors, structs of one unnamed
/// field, and `{}` (nothing to set).
///
/// # Safety
///
/// `ptr` points to a valid, writable struct described by `ty`.
unsafe fn set_struct_text(
    ty: &'static TypeInfo,
    ptr: *mut u8,
    text: &str,
    refs: &mut dyn ReadRefs,
) -> Result<(), String> {
    // SAFETY: for the block, as the caller guarantees; fields are written at their
    // offsets.
    unsafe {
        if is_entity(ty) {
            let e = match text {
                "@none" => Entity::NULL,
                t => {
                    let label = t
                        .strip_prefix('@')
                        .ok_or_else(|| format!("expected `@<id>` or `@none`, found `{t}`"))?;
                    refs.entity(label)?
                }
            };
            ptr.cast::<Entity>().write_unaligned(e);
            return Ok(());
        }
        if is_asset_ref(ty) {
            let asset = match parse_asset(text)? {
                None => AssetRef::NONE,
                Some((path, file)) => {
                    let asset = AssetRef::from_path(file, &path)?;
                    refs.asset(asset, &path);
                    asset
                }
            };
            ptr.cast::<AssetRef>().write_unaligned(asset);
            return Ok(());
        }
        if let Some(fields) = vector_fields(ty) {
            let inner = text
                .strip_prefix('(')
                .and_then(|t| t.strip_suffix(')'))
                .ok_or_else(|| format!("expected `(…)`, found `{text}`"))?;
            // Numbers hold no commas: the items are the comma-separated pieces.
            let count = if inner.trim().is_empty() {
                0
            } else {
                inner.split(',').count()
            };
            if count != fields.len() {
                return Err(format!("expected {} numbers, found {count}", fields.len()));
            }
            for (f, item) in fields.iter().zip(inner.split(',')) {
                set_primitive(
                    f.ty,
                    Primitive::F32,
                    &f.attrs,
                    ptr.add(f.offset),
                    item.trim(),
                )?;
            }
            return Ok(());
        }
        if let Some(f) = newtype_field(ty) {
            return set_text(f.ty, &f.attrs, ptr.add(f.offset), text, refs);
        }
    }
    if text == "{}" {
        return Ok(());
    }
    Err(format!("`{text}` is not a value of `{}`", ty.name))
}

/// Writes a number or a boolean's text into the primitive at `ptr`.
///
/// # Safety
///
/// `ptr` points to a valid, writable primitive `p` described by `ty`.
unsafe fn set_primitive(
    ty: &TypeInfo,
    p: Primitive,
    attrs: &Attrs,
    ptr: *mut u8,
    text: &str,
) -> Result<(), String> {
    let number = || format!("expected a number, found `{text}`");
    let value = match p {
        Primitive::Bool => match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(format!("expected `true` or `false`, found `{text}`")),
        },
        // Parsed at their own width, so written values round-trip bit for bit.
        Primitive::F32 => Value::Float(f64::from(text.parse::<f32>().map_err(|_| number())?)),
        Primitive::F64 => Value::Float(text.parse().map_err(|_| number())?),
        Primitive::U8 | Primitive::U16 | Primitive::U32 | Primitive::U64 => {
            Value::UInt(text.parse().map_err(|_| number())?)
        }
        _ => Value::Int(text.parse().map_err(|_| number())?),
    };
    // SAFETY: as the caller guarantees.
    unsafe { engine_reflect::write(ptr, ty, attrs, value) }.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn strings_round_trip_through_quotes() {
        for s in [
            "",
            "Ofelia",
            "a \"quote\" and \\",
            "line\nnext\ttab\r",
            "Офелия ☕",
            "\u{1}",
        ] {
            assert_eq!(unquote(&quote(s)).as_deref(), Ok(s), "{s:?}");
        }
        assert!(unquote("no quotes").is_err());
        assert!(unquote(r#""bad \q""#).is_err());
    }

    #[test]
    fn lines_split_outside_strings_and_brackets() {
        assert_eq!(
            split_assignment(r#"Bag.flags["a = b"] = "c = d""#),
            Some((r#"Bag.flags["a = b"]"#, r#""c = d""#))
        );
        assert_eq!(
            split_items(r#"1, (2, 3), "4, 5", [6, 7]"#),
            ["1", "(2, 3)", "\"4, 5\"", "[6, 7]"]
        );
        assert_eq!(component_of("Bag.items.0.id"), ("Bag", "items.0.id"));
        assert_eq!(component_of(r#"Bag["k"]"#), ("Bag", r#"["k"]"#));
        assert_eq!(component_of("Marker"), ("Marker", ""));
    }

    #[test]
    fn paths_split_into_names_indices_and_keys() {
        assert_eq!(
            segments(r#"items.0.tags["a.b]"].x"#).unwrap(),
            [
                Seg::Name("items"),
                Seg::Name("0"),
                Seg::Name("tags"),
                Seg::Key(r#""a.b]""#),
                Seg::Name("x"),
            ]
        );
        assert!(segments("map[\"open").is_err());
    }

    #[test]
    fn asset_text_parses_with_and_without_a_part() {
        let id = AssetId::from_u128(0x5f2c);
        assert_eq!(
            parse_asset(r#"asset("props/crate.gltf#Crate", 00000000000000000000000000005f2c)"#)
                .unwrap(),
            Some((String::from("props/crate.gltf#Crate"), id))
        );
        assert_eq!(parse_asset("none").unwrap(), None);
        assert!(parse_asset("asset(\"x\")").is_err());
        assert!(parse_asset("crate.gltf").is_err());
    }
}
