//! A save's body (ADR 0104): its types, entities with their components, resources,
//! states and the level's destroyed entities. Written in the binary from a
//! [`SaveImage`]; read from the binary or from text into a [`LoadedSave`], whose values
//! are decoded into the current types only when applied.

use crate::SaveError;
use crate::capture::{Key, SaveImage};
use crate::schema::Node;
use crate::value::{ReadContext, read_value, write_value};
use crate::wire::{Reader, write_str, write_varint};
use engine_ecs::Entity;
use engine_reflect::{Reflect, TypeInfo};
use engine_scene::SceneId;

/// The body's own version: what this engine writes and the newest it reads.
pub(crate) const BODY_VERSION: u64 = 1;

/// How a loaded entity is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadedKey {
    Stable(SceneId),
    Local,
    /// Left out when saved: not created, references to it none.
    Excluded,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedType {
    pub(crate) name: String,
    pub(crate) version: u32,
    /// The binary's shape; text names fields itself.
    pub(crate) node: Option<Node>,
}

/// A line of a text value: the path below the value, the value's text, the line number.
#[derive(Debug, Clone)]
pub(crate) struct TextLine {
    pub(crate) path: String,
    pub(crate) value: String,
    pub(crate) line: usize,
}

#[derive(Debug, Clone)]
pub(crate) enum ValueData {
    Binary { at: usize, len: usize },
    Text(Vec<TextLine>),
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedValue {
    pub(crate) ty: u32,
    pub(crate) data: ValueData,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedEntity {
    pub(crate) key: LoadedKey,
    pub(crate) parent: Option<u32>,
    /// The sibling before it under the same parent: the children's order.
    pub(crate) prev: Option<u32>,
    pub(crate) components: Vec<LoadedValue>,
}

/// A save read back: what [`apply`](crate::apply) puts into a world, and the states the
/// game and the engine's subsystems take back themselves.
#[derive(Debug, Clone, Default)]
pub struct LoadedSave {
    pub(crate) types: Vec<LoadedType>,
    pub(crate) entities: Vec<LoadedEntity>,
    pub(crate) resources: Vec<LoadedValue>,
    pub(crate) states: Vec<(String, LoadedValue)>,
    pub(crate) destroyed: Vec<SceneId>,
    pub(crate) sim_tick: u64,
    /// The binary body (the values' bytes); empty for text.
    pub(crate) data: Vec<u8>,
    /// Text: each entity's label (`~N`, or its id's hex) and index.
    pub(crate) labels: std::collections::HashMap<String, u32>,
}

impl LoadedSave {
    /// Entities in the save.
    pub fn entities(&self) -> usize {
        self.entities.len()
    }

    /// The simulation tick it was taken at.
    pub fn sim_tick(&self) -> u64 {
        self.sim_tick
    }

    /// The version a type was saved at (`#[reflect(version = N)]`, 0 when never set), for
    /// migrations after loading; `None` if the save has no value of it.
    pub fn version_of(&self, type_name: &str) -> Option<u32> {
        self.types
            .iter()
            .find(|t| t.name == type_name)
            .map(|t| t.version)
    }

    /// The names of its states.
    pub fn state_names(&self) -> impl Iterator<Item = &str> {
        self.states.iter().map(|(n, _)| n.as_str())
    }

    /// The state called `name`, read into a `T` (fields by name, what no longer fits left
    /// at `T::default()`'s); `None` if the save has none. Entity references in it resolve
    /// to `Entity::NULL` (states are the engine's and the game's own data).
    ///
    /// # Errors
    ///
    /// If the state's data is damaged.
    pub fn state<T: Reflect + Default>(&self, name: &str) -> Result<Option<T>, SaveError> {
        let Some((_, value)) = self.states.iter().find(|(n, _)| n == name) else {
            return Ok(None);
        };
        let mut out = T::default();
        let mut notes = Vec::new();
        let none = |_: u32| Entity::NULL;
        // SAFETY: `out` is a live `T`, described by `T::TYPE_INFO`.
        unsafe {
            self.decode(
                value,
                T::TYPE_INFO,
                core::ptr::from_mut(&mut out).cast(),
                &none,
                &|_| None,
                &mut notes,
            )?;
        }
        Ok(Some(out))
    }

    /// Reads a loaded value into the value at `ptr` of the current type `info`.
    ///
    /// # Safety
    ///
    /// `ptr` points to a valid, writable value of `info`.
    pub(crate) unsafe fn decode(
        &self,
        value: &LoadedValue,
        info: &'static TypeInfo,
        ptr: *mut u8,
        entity: &dyn Fn(u32) -> Entity,
        label: &dyn Fn(&str) -> Option<Entity>,
        notes: &mut Vec<String>,
    ) -> Result<(), SaveError> {
        match &value.data {
            ValueData::Binary { at, len } => {
                let node = self
                    .types
                    .get(value.ty as usize)
                    .and_then(|t| t.node.as_ref())
                    .ok_or_else(|| SaveError::damaged("a value of an unknown type"))?;
                let bytes = self
                    .data
                    .get(*at..at + len)
                    .ok_or_else(|| SaveError::damaged("a value past the end"))?;
                let mut r = Reader::new(bytes);
                let mut cx = ReadContext { entity, notes };
                // SAFETY: as the caller guarantees.
                unsafe { read_value(&mut r, node, info, &info.attrs, ptr, &mut cx) }
            }
            ValueData::Text(lines) => {
                // SAFETY: as the caller guarantees.
                unsafe { crate::text::apply_lines(lines, info, ptr, label, notes) }
            }
        }
    }
}

/// Writes `image`'s body in the binary.
pub(crate) fn write_binary(image: &SaveImage, out: &mut Vec<u8>) {
    engine_profiler::zone!("save::write_binary");
    write_varint(out, BODY_VERSION);
    write_varint(out, image.sim_tick);
    write_varint(out, image.types.len() as u64);
    for t in &image.types {
        write_str(out, t.name);
        write_varint(out, u64::from(t.version));
        t.node.write(out);
    }
    let excluded = |i: u32| image.entities.get(i as usize).is_none_or(|e| e.excluded);
    let refs = |e: Entity| {
        let i = image.index.get(e);
        if excluded(i) { u32::MAX } else { i }
    };
    let mut scratch = Vec::new();
    let value = |out: &mut Vec<u8>, v: &crate::capture::SavedValue, scratch: &mut Vec<u8>| {
        write_varint(out, u64::from(v.ty));
        let bytes = &image.bytes[v.at as usize..(v.at + v.len) as usize];
        if v.raw {
            scratch.clear();
            // SAFETY: a plain-data value copied byte for byte from a live value of the
            // type: no pointers, read unaligned field by field.
            unsafe {
                write_value(
                    scratch,
                    image.types[v.ty as usize].info,
                    bytes.as_ptr(),
                    &refs,
                );
            }
            write_varint(out, scratch.len() as u64);
            out.extend_from_slice(scratch);
        } else {
            write_varint(out, bytes.len() as u64);
            out.extend_from_slice(bytes);
        }
    };
    write_varint(out, image.entities.len() as u64);
    for e in &image.entities {
        match (e.excluded, e.key) {
            (true, _) => out.push(2),
            (false, Key::Stable(id)) => {
                out.push(0);
                out.extend_from_slice(&id.as_u128().to_le_bytes());
            }
            (false, Key::Local) => out.push(1),
        }
        let link = |i: u32| {
            if i == u32::MAX || excluded(i) {
                0
            } else {
                u64::from(i) + 1
            }
        };
        let parent = link(e.parent);
        write_varint(out, parent);
        write_varint(out, if parent == 0 { 0 } else { link(e.prev) });
        let components: &[crate::capture::SavedValue] = if e.excluded {
            &[]
        } else {
            &image.components[e.first as usize..(e.first + e.count) as usize]
        };
        write_varint(out, components.len() as u64);
        for v in components {
            value(out, v, &mut scratch);
        }
    }
    write_varint(out, image.resources.len() as u64);
    for v in &image.resources {
        value(out, v, &mut scratch);
    }
    write_varint(out, image.states.len() as u64);
    for (name, v) in &image.states {
        write_str(out, name);
        value(out, v, &mut scratch);
    }
    write_varint(out, image.destroyed.len() as u64);
    for id in &image.destroyed {
        out.extend_from_slice(&id.as_u128().to_le_bytes());
    }
}

/// Reads a binary body.
pub(crate) fn read_binary(data: Vec<u8>) -> Result<LoadedSave, SaveError> {
    engine_profiler::zone!("save::read_binary");
    let mut save = LoadedSave::default();
    let mut r = Reader::new(&data);
    let version = r.varint()?;
    if version == 0 || version > BODY_VERSION {
        return Err(SaveError::damaged(format!(
            "body version {version}: written by a newer engine"
        )));
    }
    save.sim_tick = r.varint()?;
    for _ in 0..r.len()? {
        let name = r.string()?.to_owned();
        let version = u32::try_from(r.varint()?)
            .map_err(|_| SaveError::damaged("a type version out of range"))?;
        let node = Node::read(&mut r)?;
        save.types.push(LoadedType {
            name,
            version,
            node: Some(node),
        });
    }
    let types = save.types.len();
    let value = |r: &mut Reader<'_>| -> Result<LoadedValue, SaveError> {
        let ty = r.varint()?;
        if ty >= types as u64 {
            return Err(SaveError::damaged("a value of an unknown type"));
        }
        let len = r.len()?;
        let at = r.position();
        r.bytes(len)?;
        Ok(LoadedValue {
            ty: ty as u32,
            data: ValueData::Binary { at, len },
        })
    };
    let count = r.len()?;
    for _ in 0..count {
        let key = match r.u8()? {
            0 => LoadedKey::Stable(SceneId::from_u128(r.u128()?)),
            1 => LoadedKey::Local,
            2 => LoadedKey::Excluded,
            _ => return Err(SaveError::damaged("an unknown kind of entity")),
        };
        let mut link = || match r.varint()? {
            0 => Ok(None),
            p if p <= count as u64 => Ok(Some((p - 1) as u32)),
            _ => Err(SaveError::damaged("a parent or sibling past the entities")),
        };
        let parent = link()?;
        let prev = link()?;
        let n = r.len()?;
        let mut components = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            components.push(value(&mut r)?);
        }
        save.entities.push(LoadedEntity {
            key,
            parent,
            prev,
            components,
        });
    }
    for _ in 0..r.len()? {
        save.resources.push(value(&mut r)?);
    }
    for _ in 0..r.len()? {
        let name = r.string()?.to_owned();
        save.states.push((name, value(&mut r)?));
    }
    for _ in 0..r.len()? {
        save.destroyed.push(SceneId::from_u128(r.u128()?));
    }
    if r.remaining() != 0 {
        return Err(SaveError::damaged("bytes after the body"));
    }
    save.data = data;
    Ok(save)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::capture::{CaptureOptions, capture};
    use engine_ecs::reflect::Reflect;
    use engine_ecs::{World, reflect_component};
    use std::collections::BTreeMap;

    #[derive(Reflect, Default, Clone, Debug, PartialEq)]
    struct Pack {
        name: String,
        items: Vec<u16>,
        by_name: BTreeMap<String, Option<u32>>,
        target: Entity,
    }
    reflect_component!(Pack);

    #[test]
    fn damaged_bodies_are_errors_or_harmless_never_panics() {
        let mut world = World::new();
        let first = world.spawn((SceneId::from_u128(7),));
        for i in 0..20u16 {
            let mut by_name = BTreeMap::new();
            by_name.insert(format!("k{i}"), (i % 2 == 0).then_some(u32::from(i)));
            let e = world.spawn((Pack {
                name: "x".repeat(usize::from(i)),
                items: (0..i).collect(),
                by_name,
                target: first,
            },));
            if i % 3 == 0 {
                world.set_parent(e, Some(first)).unwrap();
            }
        }
        let mut body = Vec::new();
        write_binary(&capture(&world, &CaptureOptions::default()), &mut body);
        assert!(read_binary(body.clone()).is_ok());
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut readable = 0;
        for round in 0..3000 {
            let mut damaged = body.clone();
            if round % 3 == 0 {
                damaged.truncate((next() % body.len() as u64) as usize);
            } else {
                for _ in 0..1 + next() % 3 {
                    let at = (next() % damaged.len() as u64) as usize;
                    damaged[at] = next() as u8;
                }
            }
            let Ok(save) = read_binary(damaged) else {
                continue;
            };
            readable += 1;
            // What parses must apply and print without a panic, the world kept valid.
            let mut w = World::new();
            let _ = crate::apply(&mut w, &save);
            let _ = crate::text::write_text(&save);
        }
        assert!(readable > 0, "some damage leaves a readable body");
    }
}
