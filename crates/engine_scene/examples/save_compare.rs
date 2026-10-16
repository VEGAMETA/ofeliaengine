//! Save formats compared (research for save games): the scene text format (ADR 0029)
//! against a tagged binary of the same reflected fields, on 10,000 entities with a
//! `Transform` and a gameplay component, and the ECS snapshot (a copy of the rows) as
//! the floor. Median of 20 runs, one thread. Writes both encodings to the directory given.
//!
//! ```text
//! cargo run -p engine_scene --release --example save_compare -- <OUT_DIR>
//! ```

#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::cast_possible_truncation
)]

use engine_ecs::reflect::{Primitive, Reflect, TypeInfo, TypeKind};
use engine_ecs::{Entity, QueryState, Snapshot, Transform, World, reflect_component};
use engine_scene::{SceneId, SceneRegistry, load, save};
use std::collections::HashMap;
use std::time::Instant;

/// A gameplay component: what a game keeps per unit.
#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
#[repr(C)]
struct Unit {
    health: f32,
    max_health: f32,
    ammo: u32,
    cooldown: f32,
    team: u32,
    state: u32,
    velocity: [f32; 3],
}
// SAFETY: `repr(C)`, 4-byte fields only, no padding; every bit pattern is valid.
unsafe impl engine_ecs::bytemuck::Zeroable for Unit {}
// SAFETY: as above.
unsafe impl engine_ecs::bytemuck::Pod for Unit {}
reflect_component!(pod Unit);

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// A field as the binary writes it: its path's hash, offset and width.
struct Leaf {
    tag: u32,
    offset: usize,
    width: usize,
}

fn leaves(info: &'static TypeInfo) -> Vec<Leaf> {
    let mut out = Vec::new();
    info.visit_leaves(&mut |path, offset, ty, _| {
        let width = match ty.kind {
            TypeKind::Primitive(p) | TypeKind::Enum { repr: p, .. } => match p {
                Primitive::Bool | Primitive::I8 | Primitive::U8 => 1,
                Primitive::I16 | Primitive::U16 => 2,
                Primitive::I32 | Primitive::U32 | Primitive::F32 => 4,
                Primitive::I64 | Primitive::U64 | Primitive::F64 => 8,
            },
            _ => return,
        };
        out.push(Leaf {
            tag: fnv(path),
            offset,
            width,
        });
    });
    out
}

/// A component type the binary knows: its name's hash and fields.
struct BinType {
    tag: u32,
    leaves: Vec<Leaf>,
}

impl BinType {
    fn of<T: Reflect>() -> Self {
        Self {
            tag: fnv(T::TYPE_INFO.name),
            leaves: leaves(T::TYPE_INFO),
        }
    }

    /// Tagged fields: count, then each field's tag and bytes.
    fn write(&self, ptr: *const u8, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.tag.to_le_bytes());
        out.push(self.leaves.len() as u8);
        for l in &self.leaves {
            out.extend_from_slice(&l.tag.to_le_bytes());
            // SAFETY: `ptr` is a live value of this type; the leaf lies inside it.
            let bytes = unsafe { core::slice::from_raw_parts(ptr.add(l.offset), l.width) };
            out.extend_from_slice(bytes);
        }
    }

    /// Reads tagged fields into `ptr` (a default value): unknown tags are skipped by the
    /// width they carry here, missing ones keep the default.
    fn read(&self, data: &[u8], at: &mut usize, ptr: *mut u8) {
        let count = data[*at] as usize;
        *at += 1;
        for _ in 0..count {
            let tag = u32::from_le_bytes(data[*at..*at + 4].try_into().unwrap());
            *at += 4;
            let Some(l) = self.leaves.iter().find(|l| l.tag == tag) else {
                continue;
            };
            // SAFETY: `ptr` is a live value of this type; the leaf lies inside it.
            unsafe {
                core::ptr::copy_nonoverlapping(data[*at..].as_ptr(), ptr.add(l.offset), l.width);
            }
            *at += l.width;
        }
    }
}

fn save_binary(world: &mut World, types: &[BinType; 2], out: &mut Vec<u8>) {
    let mut q = QueryState::<(Entity, &SceneId)>::new(world);
    let ids: HashMap<Entity, SceneId> = q.iter_readonly(world).map(|(e, &id)| (e, id)).collect();
    let mut q = QueryState::<(Entity, &SceneId, Option<&Transform>, Option<&Unit>)>::new(world);
    out.extend_from_slice(b"SAVE");
    out.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for (e, id, transform, unit) in q.iter_readonly(world) {
        out.extend_from_slice(engine_ecs::bytemuck::bytes_of(id));
        let parent = world.parent(e).and_then(|p| ids.get(&p));
        out.push(u8::from(parent.is_some()));
        if let Some(p) = parent {
            out.extend_from_slice(engine_ecs::bytemuck::bytes_of(p));
        }
        out.push(u8::from(transform.is_some()) + u8::from(unit.is_some()));
        if let Some(t) = transform {
            types[0].write((t as *const Transform).cast(), out);
        }
        if let Some(u) = unit {
            types[1].write((u as *const Unit).cast(), out);
        }
    }
}

fn load_binary(world: &mut World, types: &[BinType; 2], data: &[u8]) {
    let count = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    let mut at = 8;
    let mut by_id: HashMap<SceneId, Entity> = HashMap::with_capacity(count);
    let mut links: Vec<(Entity, SceneId)> = Vec::new();
    for _ in 0..count {
        let id: SceneId = engine_ecs::bytemuck::pod_read_unaligned(&data[at..at + 16]);
        at += 16;
        let has_parent = data[at] != 0;
        at += 1;
        let parent = has_parent.then(|| {
            let p: SceneId = engine_ecs::bytemuck::pod_read_unaligned(&data[at..at + 16]);
            at += 16;
            p
        });
        let components = data[at];
        at += 1;
        let (mut transform, mut unit) = (None, None);
        for _ in 0..components {
            let tag = u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
            at += 4;
            if tag == types[0].tag {
                let mut t = Transform::default();
                types[0].read(data, &mut at, (&raw mut t).cast());
                transform = Some(t);
            } else if tag == types[1].tag {
                let mut u = Unit::default();
                types[1].read(data, &mut at, (&raw mut u).cast());
                unit = Some(u);
            }
        }
        let e = match (transform, unit) {
            (Some(t), Some(u)) => world.spawn((id, t, engine_ecs::GlobalTransform::default(), u)),
            (Some(t), None) => world.spawn((id, t, engine_ecs::GlobalTransform::default())),
            (None, Some(u)) => world.spawn((id, u)),
            (None, None) => world.spawn((id,)),
        };
        by_id.insert(id, e);
        if let Some(p) = parent {
            links.push((e, p));
        }
    }
    for (e, p) in links {
        world.set_parent(e, Some(by_id[&p])).unwrap();
    }
}

fn main() {
    let out_dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let mut registry = SceneRegistry::with_engine_types();
    registry.register::<Unit>();
    let mut world = World::new();
    for r in 0..100u32 {
        let root = world.spawn((SceneId::new(), Transform::from_xyz(r as f32, 0.0, 0.0)));
        for c in 0..99u32 {
            let k = r * 100 + c;
            let child = world.spawn((
                SceneId::new(),
                Transform::from_xyz(0.0, c as f32 * 0.37, 1.5)
                    .with_scale(Transform::IDENTITY.scale * 1.25),
                Unit {
                    health: (k % 97) as f32 * 1.03,
                    max_health: 100.0,
                    ammo: k % 31,
                    cooldown: (k % 7) as f32 * 0.25,
                    team: k % 4,
                    state: k % 5,
                    velocity: [0.1 * (k % 13) as f32, 0.0, -0.37],
                },
            ));
            world.set_parent(child, Some(root)).unwrap();
        }
    }
    let types = [BinType::of::<Transform>(), BinType::of::<Unit>()];

    let mut text = String::new();
    let text_save = median(
        (0..20)
            .map(|_| {
                text.clear();
                let t = Instant::now();
                save(&mut world, &registry, &mut text).unwrap();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect(),
    );
    let text_load = median(
        (0..20)
            .map(|_| {
                let mut w = World::new();
                let t = Instant::now();
                load(&mut w, &registry, &text).unwrap();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect(),
    );
    let mut bin = Vec::new();
    let bin_save = median(
        (0..20)
            .map(|_| {
                bin.clear();
                let t = Instant::now();
                save_binary(&mut world, &types, &mut bin);
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect(),
    );
    let bin_load = median(
        (0..20)
            .map(|_| {
                let mut w = World::new();
                let t = Instant::now();
                load_binary(&mut w, &types, &bin);
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect(),
    );
    // Both decode to the same world.
    let (mut a, mut b) = (World::new(), World::new());
    load(&mut a, &registry, &text).unwrap();
    load_binary(&mut b, &types, &bin);
    let mut again = String::new();
    save(&mut b, &registry, &mut again).unwrap();
    assert_eq!(again, text, "the binary round trip differs from the text");
    // The floor: a copy of every row.
    let mut snapshot = Snapshot::new();
    let snap = median(
        (0..20)
            .map(|_| {
                let t = Instant::now();
                world.save(&mut snapshot).unwrap();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect(),
    );
    std::fs::write(out_dir.join("save.txt"), &text).unwrap();
    std::fs::write(out_dir.join("save.bin"), &bin).unwrap();
    println!("10,000 entities (9,900 with Unit), 100 x 100 hierarchy, one thread, ms:");
    println!(
        "text   save {text_save:.2}  load {text_load:.2}  {} KiB",
        text.len() / 1024
    );
    println!(
        "binary save {bin_save:.2}  load {bin_load:.2}  {} KiB",
        bin.len() / 1024
    );
    println!(
        "snapshot (copy of the rows) {snap:.3} ms, {} KiB",
        snapshot.byte_len() / 1024
    );
}
