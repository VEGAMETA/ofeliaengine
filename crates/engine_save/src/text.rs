//! The text form of a save (ADR 0104): the scene format's lines (ADR 0029), grown for
//! strings, lists, optional values and maps. Written from the binary's shapes, so a save
//! dumps without the game's types; read into the current types by name.
//!
//! ```text
//! [save version=1 tick=3600]
//!
//! [versions]
//! Inventory = 2
//!
//! [entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
//! Transform.translation = (10, 0, 0)
//! Door.open = true
//!
//! [entity ~1]
//! parent = @6c1f0a3e9d2b47c88e51f0d4a7b3c912
//! Inventory.owner = "Ofelia"
//! Inventory.items.0.id = 3
//! Inventory.items.0.count = 2
//! Inventory.flags["met the smith"] = true
//! Inventory.best = none
//!
//! [resource Score]
//! points = 1200
//!
//! [state physics PhysicsState]
//! gravity = (0, -9.81, 0)
//!
//! [destroyed]
//! 0b12e4f8c3a94d5e9f7a6b1c2d3e4f50
//! ```
//!
//! - Entities are headed by their id, or `~N` for those the game spawned.
//! - One line per value: `Component.path = value`; in resource and state blocks just
//!   `path = value`.
//! - Paths:
//!   - lists take indices (`items.0`);
//!   - maps take keys in brackets (`flags["met the smith"]`, `counts[9]`);
//!   - optional values go straight to their value's fields.
//! - Values:
//!   - numbers, `true`, `false`, enum variants by name;
//!   - `"strings"` with `\"`, `\\`, `\n`, `\t` and `\u{…}`;
//!   - `@<id>`, `@~N` or `@none` for entities;
//!   - `(x, y, z)` for vectors, `[a, b]` for arrays and lists of single values, `[]` for
//!     an empty list;
//!   - `{}` for an empty map or a value with no fields;
//!   - `none` and `some` for optional values.

use crate::SaveError;
use crate::body::{
    BODY_VERSION, LoadedEntity, LoadedKey, LoadedSave, LoadedType, LoadedValue, TextLine, ValueData,
};
use crate::schema::{Node, is_entity, vector_like};
use crate::value::read_prim;
use crate::wire::Reader;
use engine_ecs::Entity;
use engine_reflect::{Attrs, Primitive, TypeInfo, TypeKind, Value};
use engine_scene::SceneId;
use std::collections::HashMap;
use std::fmt::Write as _;

/// The text of a save file, whichever encoding it was written in: what a developer reads,
/// diffs and fixes. Any build loads the text back as it is.
///
/// # Errors
///
/// If the file is not a save, is damaged, or was written by a newer engine.
pub fn dump(file: &[u8]) -> Result<String, SaveError> {
    let (_, encoding, body) = crate::file::open(file)?;
    match encoding {
        crate::Encoding::Text => {
            String::from_utf8(body).map_err(|_| SaveError::damaged("text that is not UTF-8"))
        }
        crate::Encoding::Binary => write_text(&crate::body::read_binary(body)?),
    }
}

/// The text form of a binary save.
pub(crate) fn write_text(save: &LoadedSave) -> Result<String, SaveError> {
    engine_profiler::zone!("save::write_text");
    let mut out = String::new();
    let _ = writeln!(out, "[save version={BODY_VERSION} tick={}]", save.sim_tick);
    let versioned: Vec<&LoadedType> = save.types.iter().filter(|t| t.version != 0).collect();
    if !versioned.is_empty() {
        out.push_str("\n[versions]\n");
        for t in versioned {
            let _ = writeln!(out, "{} = {}", t.name, t.version);
        }
    }
    let label = |i: u32| -> String {
        match save.entities.get(i as usize).map(|e| e.key) {
            Some(LoadedKey::Stable(id)) => format!("@{id}"),
            Some(LoadedKey::Local) => format!("@~{i}"),
            _ => "@none".to_owned(),
        }
    };
    let mut w = Writer {
        out: &mut out,
        label: &label,
    };
    for i in hierarchy_order(save) {
        let e = &save.entities[i];
        match e.key {
            LoadedKey::Excluded => continue,
            LoadedKey::Stable(id) => {
                let _ = writeln!(w.out, "\n[entity {id}]");
            }
            LoadedKey::Local => {
                let _ = writeln!(w.out, "\n[entity ~{i}]");
            }
        }
        if let Some(p) = e.parent {
            let _ = writeln!(w.out, "parent = {}", label(p));
        }
        for v in &e.components {
            let name = save.types[v.ty as usize].name.as_str();
            let before = w.out.len();
            w.value(save, v, name)?;
            if w.out.len() == before {
                let _ = writeln!(w.out, "{name} = {{}}");
            }
        }
    }
    for v in &save.resources {
        let name = &save.types[v.ty as usize].name;
        let _ = writeln!(w.out, "\n[resource {name}]");
        w.value(save, v, "")?;
    }
    for (name, v) in &save.states {
        let ty = &save.types[v.ty as usize].name;
        let _ = writeln!(w.out, "\n[state {name} {ty}]");
        w.value(save, v, "")?;
    }
    if !save.destroyed.is_empty() {
        out.push_str("\n[destroyed]\n");
        for id in &save.destroyed {
            let _ = writeln!(out, "{id}");
        }
    }
    Ok(out)
}

/// The entities parents first, each followed by its children in their order, so the
/// order of the blocks is the children's order (the reader takes it back from there).
fn hierarchy_order(save: &LoadedSave) -> Vec<usize> {
    let n = save.entities.len();
    let mut next = vec![usize::MAX; n];
    let mut heads: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots = Vec::new();
    for (i, e) in save.entities.iter().enumerate() {
        match e.parent.map(|p| p as usize).filter(|&p| p < n) {
            Some(p) => match e.prev.map(|q| q as usize) {
                Some(q)
                    if q < n && save.entities[q].parent == e.parent && next[q] == usize::MAX =>
                {
                    next[q] = i;
                }
                _ => heads[p].push(i),
            },
            None => roots.push(i),
        }
    }
    let mut order = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        order.push(i);
        let mut children = Vec::new();
        for &h in &heads[i] {
            let mut c = h;
            while c != usize::MAX && !seen[c] && children.len() < n {
                children.push(c);
                c = next[c];
            }
        }
        stack.extend(children.into_iter().rev());
    }
    // A damaged chain (a cycle) leaves some unreached: written at the end.
    order.extend((0..n).filter(|&i| !seen[i]));
    order
}

struct Writer<'a> {
    out: &'a mut String,
    label: &'a dyn Fn(u32) -> String,
}

impl Writer<'_> {
    fn value(&mut self, save: &LoadedSave, v: &LoadedValue, root: &str) -> Result<(), SaveError> {
        let ValueData::Binary { at, len } = v.data else {
            return Err(SaveError::damaged("text written from text"));
        };
        let node = save.types[v.ty as usize]
            .node
            .as_ref()
            .ok_or_else(|| SaveError::damaged("a value without its shape"))?;
        let bytes = save
            .data
            .get(at..at + len)
            .ok_or_else(|| SaveError::damaged("a value past the end"))?;
        let mut r = Reader::new(bytes);
        let mut path = root.to_owned();
        // A resource or state that is a single value has no field to name: `value`.
        if path.is_empty() && !matches!(node, Node::Struct(f) if !vector_like(f)) {
            path.push_str("value");
        }
        self.node(&mut path, node, &mut r)
    }

    fn line(&mut self, path: &str, value: &str) {
        let _ = writeln!(self.out, "{path} = {value}");
    }

    fn node(
        &mut self,
        path: &mut String,
        node: &Node,
        r: &mut Reader<'_>,
    ) -> Result<(), SaveError> {
        match node {
            Node::Struct(fields) if !vector_like(fields) => {
                if fields.is_empty() {
                    if !path.is_empty() {
                        self.line(path, "{}");
                    }
                    return Ok(());
                }
                for (name, field) in fields {
                    let len = path.len();
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(name);
                    self.node(path, field, r)?;
                    path.truncate(len);
                }
            }
            Node::Array(element, len) if !single(element) => {
                for i in 0..*len {
                    let old = path.len();
                    let _ = write!(path, ".{i}");
                    self.node(path, element, r)?;
                    path.truncate(old);
                }
            }
            Node::Array(element, len) => {
                let mut items = Vec::with_capacity(*len as usize);
                for _ in 0..*len {
                    items.push(self.single(element, r)?);
                }
                self.line(path, &format!("[{}]", items.join(", ")));
            }
            Node::List(element) => {
                let n = r.len()?;
                if n == 0 {
                    self.line(path, "[]");
                } else if single(element) {
                    let mut items = Vec::with_capacity(n);
                    for _ in 0..n {
                        items.push(self.single(element, r)?);
                    }
                    self.line(path, &format!("[{}]", items.join(", ")));
                } else {
                    for i in 0..n {
                        let old = path.len();
                        let _ = write!(path, ".{i}");
                        self.node(path, element, r)?;
                        path.truncate(old);
                    }
                }
            }
            Node::Option(inner) => match r.u8()? {
                0 => self.line(path, "none"),
                1 if single(inner) => {
                    let text = self.single(inner, r)?;
                    self.line(path, &text);
                }
                1 => {
                    self.line(path, "some");
                    self.node(path, inner, r)?;
                }
                _ => return Err(SaveError::damaged("an optional value's tag")),
            },
            Node::Map(key, value) => {
                let n = r.len()?;
                if n == 0 {
                    self.line(path, "{}");
                }
                if n > 0 && !single(key) {
                    return Err(SaveError::damaged("a map whose keys are not single values"));
                }
                for _ in 0..n {
                    let k = self.single(key, r)?;
                    let old = path.len();
                    let _ = write!(path, "[{k}]");
                    if single(value) {
                        let text = self.single(value, r)?;
                        self.line(path, &text);
                    } else {
                        self.node(path, value, r)?;
                    }
                    path.truncate(old);
                }
            }
            Node::Opaque => {}
            single_kind => {
                let text = self.single(single_kind, r)?;
                self.line(path, &text);
            }
        }
        Ok(())
    }

    /// A value written on one line.
    fn single(&mut self, node: &Node, r: &mut Reader<'_>) -> Result<String, SaveError> {
        Ok(match node {
            Node::Prim(p) => prim_text(*p, read_prim(r, *p)?),
            Node::Enum { repr, variants } => {
                let raw = match read_prim(r, *repr)? {
                    Value::Int(i) => i,
                    Value::UInt(u) => u as i64,
                    v => v.as_f64() as i64,
                };
                variants
                    .iter()
                    .find(|(_, v)| *v == raw)
                    .map_or_else(|| raw.to_string(), |(n, _)| n.clone())
            }
            Node::Str => quote(r.string()?),
            Node::Entity => {
                let i = r.u32()?;
                (self.label)(i)
            }
            Node::Struct(fields) => {
                let mut items = Vec::with_capacity(fields.len());
                for (_, f) in fields {
                    items.push(self.single(f, r)?);
                }
                format!("({})", items.join(", "))
            }
            _ => return Err(SaveError::damaged("a value that is not single")),
        })
    }
}

/// Values written on one line: numbers, variants, strings, entities, vectors.
fn single(node: &Node) -> bool {
    match node {
        Node::Prim(_) | Node::Enum { .. } | Node::Str | Node::Entity => true,
        Node::Struct(fields) => vector_like(fields),
        _ => false,
    }
}

fn prim_text(p: Primitive, v: Value) -> String {
    match (p, v) {
        (_, Value::Bool(b)) => b.to_string(),
        (_, Value::Int(i)) => i.to_string(),
        (_, Value::UInt(u)) => u.to_string(),
        (Primitive::F32, Value::Float(f)) => (f as f32).to_string(),
        (_, Value::Float(f)) => f.to_string(),
    }
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
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
    out
}

fn unquote(text: &str) -> Result<String, String> {
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
fn split_assignment(line: &str) -> Option<(&str, &str)> {
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
fn split_items(inner: &str) -> Vec<&str> {
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
fn component_of(path: &str) -> (&str, &str) {
    let end = path.find(['.', '[']).unwrap_or(path.len());
    let rest = &path[end..];
    (&path[..end], rest.strip_prefix('.').unwrap_or(rest))
}

/// Reads a text body.
pub(crate) fn read_text(text: &str) -> Result<LoadedSave, SaveError> {
    engine_profiler::zone!("save::read_text");
    #[derive(Clone, Copy)]
    enum Block {
        None,
        Versions,
        Entity(usize),
        Resource(usize),
        State(usize),
        Destroyed,
    }
    let mut save = LoadedSave::default();
    let mut type_index: HashMap<String, u32> = HashMap::new();
    let mut versions: HashMap<String, u32> = HashMap::new();
    let mut parents: Vec<(usize, String, usize)> = Vec::new();
    let mut block = Block::None;
    let mut ty = |name: &str, types: &mut Vec<LoadedType>| -> u32 {
        *type_index.entry(name.to_owned()).or_insert_with(|| {
            types.push(LoadedType {
                name: name.to_owned(),
                version: 0,
                node: None,
            });
            types.len() as u32 - 1
        })
    };
    let err = |line: usize, message: String| SaveError::Text { line, message };
    for (n, raw) in text.lines().enumerate() {
        let n = n + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(head) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let words: Vec<&str> = head.split_whitespace().collect();
            block = match words.as_slice() {
                ["save", rest @ ..] => {
                    for w in rest {
                        match w.split_once('=') {
                            Some(("version", v)) => {
                                let v: u64 =
                                    v.parse().map_err(|_| err(n, format!("version `{v}`")))?;
                                if v == 0 || v > BODY_VERSION {
                                    return Err(err(
                                        n,
                                        format!("version {v}: written by a newer engine"),
                                    ));
                                }
                            }
                            Some(("tick", t)) => {
                                save.sim_tick =
                                    t.parse().map_err(|_| err(n, format!("tick `{t}`")))?;
                            }
                            _ => {}
                        }
                    }
                    Block::None
                }
                ["versions"] => Block::Versions,
                ["entity", label] => {
                    let key = if label.starts_with('~') {
                        LoadedKey::Local
                    } else {
                        let id = (label.len() == 32)
                            .then(|| u128::from_str_radix(label, 16).ok())
                            .flatten()
                            .ok_or_else(|| {
                                err(n, format!("`{label}` is not an entity's id or ~N"))
                            })?;
                        LoadedKey::Stable(SceneId::from_u128(id))
                    };
                    let index = save.entities.len();
                    if save
                        .labels
                        .insert((*label).to_owned(), index as u32)
                        .is_some()
                    {
                        return Err(err(n, format!("entity `{label}` twice")));
                    }
                    save.entities.push(LoadedEntity {
                        key,
                        parent: None,
                        prev: None,
                        components: Vec::new(),
                    });
                    Block::Entity(index)
                }
                ["resource", name] => {
                    let t = ty(name, &mut save.types);
                    save.resources.push(LoadedValue {
                        ty: t,
                        data: ValueData::Text(Vec::new()),
                    });
                    Block::Resource(save.resources.len() - 1)
                }
                ["state", name, type_name] => {
                    let t = ty(type_name, &mut save.types);
                    save.states.push((
                        (*name).to_owned(),
                        LoadedValue {
                            ty: t,
                            data: ValueData::Text(Vec::new()),
                        },
                    ));
                    Block::State(save.states.len() - 1)
                }
                ["destroyed"] => Block::Destroyed,
                _ => return Err(err(n, format!("unknown block `[{head}]`"))),
            };
            continue;
        }
        match block {
            Block::None => return Err(err(n, "a line outside any block".to_owned())),
            Block::Destroyed => {
                let id = (line.len() == 32)
                    .then(|| u128::from_str_radix(line, 16).ok())
                    .flatten()
                    .ok_or_else(|| err(n, format!("`{line}` is not an entity's id")))?;
                save.destroyed.push(SceneId::from_u128(id));
            }
            Block::Versions => {
                let (name, v) = split_assignment(line)
                    .ok_or_else(|| err(n, "expected `Type = version`".to_owned()))?;
                let v = v.parse().map_err(|_| err(n, format!("version `{v}`")))?;
                versions.insert(name.to_owned(), v);
            }
            Block::Entity(e) => {
                let (path, value) = split_assignment(line)
                    .ok_or_else(|| err(n, "expected `Component.field = value`".to_owned()))?;
                if path == "parent" {
                    parents.push((e, value.to_owned(), n));
                    continue;
                }
                let (name, rest) = component_of(path);
                let t = ty(name, &mut save.types);
                let components = &mut save.entities[e].components;
                let slot = match components.iter().position(|c| c.ty == t) {
                    Some(i) => i,
                    None => {
                        components.push(LoadedValue {
                            ty: t,
                            data: ValueData::Text(Vec::new()),
                        });
                        components.len() - 1
                    }
                };
                if let ValueData::Text(lines) = &mut components[slot].data {
                    lines.push(TextLine {
                        path: rest.to_owned(),
                        value: value.to_owned(),
                        line: n,
                    });
                }
            }
            Block::Resource(i) | Block::State(i) => {
                let (path, value) = split_assignment(line)
                    .ok_or_else(|| err(n, "expected `field = value`".to_owned()))?;
                let target = match block {
                    Block::Resource(_) => &mut save.resources[i],
                    _ => &mut save.states[i].1,
                };
                if let ValueData::Text(lines) = &mut target.data {
                    lines.push(TextLine {
                        path: if path == "value" {
                            String::new()
                        } else {
                            path.to_owned()
                        },
                        value: value.to_owned(),
                        line: n,
                    });
                }
            }
        }
    }
    for (e, parent, n) in parents {
        if parent == "@none" {
            continue;
        }
        let label = parent
            .strip_prefix('@')
            .ok_or_else(|| err(n, format!("`{parent}` is not an entity (`@…`)")))?;
        let p = *save
            .labels
            .get(label)
            .ok_or_else(|| err(n, format!("no entity `{label}` in the save")))?;
        save.entities[e].parent = Some(p);
    }
    // The blocks' order is the children's order.
    let mut last: HashMap<u32, u32> = HashMap::new();
    for (i, e) in save.entities.iter_mut().enumerate() {
        if let Some(p) = e.parent {
            e.prev = last.insert(p, i as u32);
        }
    }
    for t in &mut save.types {
        t.version = versions.get(&t.name).copied().unwrap_or(0);
    }
    Ok(save)
}

/// A step of a text path.
enum Seg<'a> {
    Name(&'a str),
    Key(&'a str),
}

fn segments(path: &str) -> Result<Vec<Seg<'_>>, String> {
    let mut out = Vec::new();
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
    Ok(out)
}

/// Lists grown by a line's index stop here: a typo must not allocate gigabytes.
const MAX_TEXT_LIST_BYTES: usize = 256 << 20;

/// Reads text lines into the value at `ptr` of the current type `info`.
///
/// # Safety
///
/// `ptr` points to a valid, writable value of `info`.
pub(crate) unsafe fn apply_lines(
    lines: &[TextLine],
    info: &'static TypeInfo,
    ptr: *mut u8,
    label: &dyn Fn(&str) -> Option<Entity>,
    notes: &mut Vec<String>,
) -> Result<(), SaveError> {
    for line in lines {
        let fail = |message: String| SaveError::Text {
            line: line.line,
            message,
        };
        let segs = segments(&line.path).map_err(fail)?;
        // SAFETY: as the caller guarantees; each step stays inside the value it names.
        let target = unsafe { resolve(&segs, info, ptr, label) }.map_err(fail)?;
        let Some((ty, attrs, at)) = target else {
            if notes.len() < 64 {
                notes.push(format!(
                    "line {}: `{}.{}` is gone; skipped",
                    line.line, info.name, line.path
                ));
            }
            continue;
        };
        // SAFETY: `at` is a valid, writable value of `ty`.
        unsafe { set_text(ty, &attrs, at, &line.value, label) }.map_err(fail)?;
    }
    Ok(())
}

/// Follows a path into a value; `None` when a field is gone.
///
/// # Safety
///
/// `ptr` points to a valid, writable value of `ty`.
unsafe fn resolve(
    segs: &[Seg<'_>],
    mut ty: &'static TypeInfo,
    mut ptr: *mut u8,
    label: &dyn Fn(&str) -> Option<Entity>,
) -> Result<Option<(&'static TypeInfo, Attrs, *mut u8)>, String> {
    let mut attrs = ty.attrs;
    for seg in segs {
        // SAFETY: for the block, every step stays inside the value its description lays
        // out; containers are changed through their own functions.
        unsafe {
            // An optional value's fields are its value's.
            while let TypeKind::Option(option) = ty.kind {
                ptr = (option.set)(ptr, true);
                ty = option.inner;
            }
            match (seg, &ty.kind) {
                (Seg::Name(name), TypeKind::Struct(fields)) => {
                    let Some(f) = fields.iter().find(|f| f.name == *name && !f.attrs.skip) else {
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
                        label,
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
    }
    Ok(Some((ty, attrs, ptr)))
}

/// Whether a current struct is written as a tuple.
fn vector_fields(ty: &TypeInfo) -> Option<&'static [engine_reflect::Field]> {
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

/// Writes a line's value text into the value at `ptr` of type `ty`.
///
/// # Safety
///
/// `ptr` points to a valid, writable value of `ty`.
unsafe fn set_text(
    ty: &'static TypeInfo,
    attrs: &Attrs,
    ptr: *mut u8,
    text: &str,
    label: &dyn Fn(&str) -> Option<Entity>,
) -> Result<(), String> {
    // SAFETY: for the block, as the caller guarantees; containers are changed through
    // their own functions, elements written inside their storage.
    unsafe {
        if is_entity(ty) {
            let e = match text {
                "@none" => Entity::NULL,
                t => {
                    let l = t
                        .strip_prefix('@')
                        .ok_or_else(|| format!("`{t}` is not an entity (`@…`)"))?;
                    label(l).ok_or_else(|| format!("no entity `{l}` in the save"))?
                }
            };
            ptr.cast::<Entity>().write_unaligned(e);
            return Ok(());
        }
        if let Some(fields) = vector_fields(ty) {
            let items = text
                .strip_prefix('(')
                .and_then(|t| t.strip_suffix(')'))
                .map(split_items)
                .ok_or_else(|| format!("expected `(…)`, found `{text}`"))?;
            if items.len() != fields.len() {
                return Err(format!(
                    "expected {} numbers, found {}",
                    fields.len(),
                    items.len()
                ));
            }
            for (f, item) in fields.iter().zip(items) {
                set_text(f.ty, &f.attrs, ptr.add(f.offset), item, label)?;
            }
            return Ok(());
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
                    set_text(option.inner, &option.inner.attrs, inner, other, label)?;
                }
            },
            TypeKind::Primitive(p) => {
                let value = match p {
                    Primitive::Bool => match text {
                        "true" => Value::Bool(true),
                        "false" => Value::Bool(false),
                        _ => return Err(format!("expected true or false, found `{text}`")),
                    },
                    Primitive::F32 | Primitive::F64 => Value::Float(
                        text.parse()
                            .map_err(|_| format!("`{text}` is not a number"))?,
                    ),
                    Primitive::U8 | Primitive::U16 | Primitive::U32 | Primitive::U64 => {
                        Value::UInt(
                            text.parse()
                                .map_err(|_| format!("`{text}` is not a number"))?,
                        )
                    }
                    _ => Value::Int(
                        text.parse()
                            .map_err(|_| format!("`{text}` is not a number"))?,
                    ),
                };
                engine_reflect::write(ptr, ty, attrs, value).map_err(|e| e.to_string())?;
            }
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
                        label,
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
                    set_text(list.element, &list.element.attrs, el, item, label)?;
                }
            }
            TypeKind::Map(map) if text == "{}" => (map.clear)(ptr),
            TypeKind::Struct(_) if text == "{}" => {}
            _ => return Err(format!("`{text}` is not a value of `{}`", ty.name)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_round_trip_through_quotes() {
        for s in [
            "",
            "Ofelia",
            "a \"quote\" and \\",
            "line\nnext\ttab",
            "Офелия ☕",
            "\u{1}",
        ] {
            assert_eq!(unquote(&quote(s)).as_deref(), Ok(s), "{s:?}");
        }
        assert!(unquote("no quotes").is_err());
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
}
