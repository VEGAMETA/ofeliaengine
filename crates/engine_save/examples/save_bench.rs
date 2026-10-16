//! What a save costs (ADR 0104): 10,000 level entities with an id and a `Transform` in 100
//! hierarchies of 100, 9,900 with a plain-data gameplay component, 100 with an inventory
//! (a name, ten items, five flags). The frame's capture, the background's encoding and
//! compression, the file, reading and applying it to the level loaded again. Median of 20,
//! one thread. Prints Markdown.
//!
//! ```text
//! cargo run -p engine_save --release --example save_bench
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::cast_precision_loss)]

use engine_ecs::reflect::Reflect;
use engine_ecs::{Transform, World, reflect_component};
use engine_save::{
    CaptureOptions, Encoding, SaveHeader, SceneId, apply, capture, decode, encode, write_save,
};
use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq)]
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

#[derive(Reflect, Default, Clone, Debug, PartialEq)]
struct Item {
    id: u32,
    count: u16,
}

#[derive(Reflect, Default, Clone, Debug, PartialEq)]
struct Inventory {
    owner: String,
    items: Vec<Item>,
    flags: BTreeMap<String, bool>,
}
reflect_component!(Inventory);

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn time(mut f: impl FnMut()) -> f64 {
    median(
        (0..20)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect(),
    )
}

/// The level as it loads: every entity with its id.
fn level(world: &mut World, ids: &[SceneId]) {
    for r in 0..100usize {
        let root = world.spawn((ids[r * 100], Transform::from_xyz(r as f32, 0.0, 0.0)));
        for c in 0..99usize {
            let k = r * 100 + c;
            let child = world.spawn((
                ids[r * 100 + 1 + c],
                Transform::from_xyz(0.0, c as f32 * 0.37, 1.5),
                Unit {
                    health: (k % 97) as f32 * 1.03,
                    max_health: 100.0,
                    ammo: (k % 31) as u32,
                    cooldown: (k % 7) as f32 * 0.25,
                    team: (k % 4) as u32,
                    state: (k % 5) as u32,
                    velocity: [0.1 * (k % 13) as f32, 0.0, -0.37],
                },
            ));
            world.set_parent(child, Some(root)).unwrap();
            if c == 0 {
                world.insert(
                    child,
                    Inventory {
                        owner: format!("Ofelia {r}"),
                        items: (0..10).map(|i| Item { id: i, count: 1 }).collect(),
                        flags: (0..5).map(|f| (format!("quest {f}"), f % 2 == 0)).collect(),
                    },
                );
            }
        }
    }
}

fn main() {
    let ids: Vec<SceneId> = (0..10_000u128)
        .map(|i| SceneId::from_u128(0x5eed_0000 + i))
        .collect();
    let known: HashSet<SceneId> = ids.iter().copied().collect();
    let mut world = World::new();
    level(&mut world, &ids);
    let options = CaptureOptions {
        level: Some(&known),
    };
    let capture_ms = time(|| {
        let _ = capture(&world, &options);
    });
    let image = capture(&world, &options);
    let header = SaveHeader::default();
    println!("### engine_save (10,000 entities, 9,900 plain-data units, 100 inventories)\n");
    println!(
        "| encoding | capture (frame) | encode + compress | file | read | apply | body | file size |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    let dir = std::env::temp_dir().join(format!("engine_save_bench_{}", std::process::id()));
    for encoding in [Encoding::Binary, Encoding::Text] {
        let encode_ms = time(|| {
            let _ = encode(&image, encoding, &header).unwrap();
        });
        let bytes = encode(&image, encoding, &header).unwrap();
        let path = dir.join(format!("{encoding:?}.save"));
        let write_ms = median(
            (0..20)
                .map(|_| {
                    write_save(&path, &image, encoding, &header)
                        .unwrap()
                        .write_ms
                })
                .collect(),
        );
        let stats = write_save(&path, &image, encoding, &header).unwrap();
        let read_ms = time(|| {
            let _ = decode(&bytes).unwrap();
        });
        let save = decode(&bytes).unwrap();
        let mut worlds: Vec<World> = (0..20)
            .map(|_| {
                let mut w = World::new();
                level(&mut w, &ids);
                w
            })
            .collect();
        let apply_ms = median(
            worlds
                .iter_mut()
                .map(|w| {
                    let t = Instant::now();
                    let report = apply(w, &save).unwrap();
                    assert_eq!(report.matched, 10_000);
                    t.elapsed().as_secs_f64() * 1e3
                })
                .collect(),
        );
        println!(
            "| {encoding:?} | {capture_ms:.3} ms | {encode_ms:.2} ms | {write_ms:.2} ms | {read_ms:.2} ms | {apply_ms:.2} ms | {} KiB | {} KiB |",
            stats.body_bytes / 1024,
            bytes.len() / 1024
        );
    }
    println!(
        "\nThe frame's copy: {} KiB of values for {} entities.",
        image.bytes() / 1024,
        image.entities()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
