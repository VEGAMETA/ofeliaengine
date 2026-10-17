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
use crate::schema::{Node, vector_like};
use crate::value::read_prim;
use crate::wire::Reader;
use engine_ecs::Entity;
use engine_reflect::{Primitive, TypeInfo, Value};
use engine_scene::SceneId;
use engine_scene::text::{self, ReadRefs, component_of, quote, split_assignment};
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

/// Entities named by a save's labels.
struct Labels<'a>(&'a dyn Fn(&str) -> Option<Entity>);

impl ReadRefs for Labels<'_> {
    fn entity(&mut self, label: &str) -> Result<Entity, String> {
        (self.0)(label).ok_or_else(|| format!("no entity `{label}` in the save"))
    }
}

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
    let mut refs = Labels(label);
    let mut segs = Vec::new();
    for line in lines {
        let fail = |message: String| SaveError::Text {
            line: line.line,
            message,
        };
        text::segments_into(&line.path, &mut segs).map_err(fail)?;
        // SAFETY: as the caller guarantees; each step stays inside the value it names.
        let target = unsafe { text::resolve(&segs, info, ptr, &mut refs) }.map_err(fail)?;
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
        unsafe { text::set_text(ty, &attrs, at, &line.value, &mut refs) }.map_err(fail)?;
    }
    Ok(())
}
