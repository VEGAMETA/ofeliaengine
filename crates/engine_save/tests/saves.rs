//! Saves of a small game world (ADR 0104): written and read back in both encodings, applied
//! to a level loaded again, kept through damage and power loss.

#![allow(clippy::unwrap_used, clippy::float_cmp)]

use engine_core::math::Vec3;
use engine_ecs::reflect::Reflect;
use engine_ecs::{Entity, QueryState, Transform, World, reflect_component, reflect_resource};
use engine_save::{
    CaptureOptions, Encoding, LoadedSave, SaveHeader, SceneId, Transient, apply, capture, decode,
    dump, encode, read_save, write_save,
};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Not declared plain data: written in the frame through reflection.
#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq)]
struct Health {
    current: f32,
    max: f32,
}
reflect_component!(Health);

/// Plain data: copied in the frame, encoded behind it.
#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq)]
#[repr(C)]
struct Speed {
    value: f32,
    boost: u32,
}

#[derive(Reflect, Default, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Item {
    id: u32,
    count: u16,
}

#[derive(Reflect, Default, Clone, Debug, PartialEq)]
struct Inventory {
    owner: String,
    items: Vec<Item>,
    best: Option<Item>,
    flags: BTreeMap<String, bool>,
    counts: HashMap<u32, Vec<u8>>,
    follow: Entity,
    #[reflect(skip)]
    cache: u64,
}
reflect_component!(Inventory);

/// Made again on load: never saved.
#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq)]
#[reflect(skip)]
struct Glow {
    phase: f32,
}
reflect_component!(Glow);

#[derive(Reflect, Default, Debug, PartialEq)]
struct Score {
    points: u32,
    best: String,
}
reflect_resource!(Score);

#[derive(Reflect, Default, Debug, PartialEq)]
struct PhysicsState {
    gravity: Vec3,
    steps: u64,
}

// SAFETY: `repr(C)`, an `f32` and a `u32`: no padding, every bit pattern valid.
unsafe impl engine_ecs::bytemuck::Zeroable for Speed {}
// SAFETY: as above; `Copy`.
unsafe impl engine_ecs::bytemuck::Pod for Speed {}
reflect_component!(pod Speed);

fn id(n: u128) -> SceneId {
    SceneId::from_u128(n)
}

fn find(world: &mut World, wanted: SceneId) -> Entity {
    let mut q = QueryState::<(Entity, &SceneId)>::new(world);
    q.iter_readonly(world)
        .find(|(_, s)| **s == wanted)
        .map(|(e, _)| e)
        .unwrap()
}

/// The level as it loads: three entities with ids.
fn level(world: &mut World) {
    world.spawn((
        id(1),
        Transform::from_xyz(1.0, 0.0, 0.0),
        Health {
            current: 100.0,
            max: 100.0,
        },
    ));
    world.spawn((id(2), Transform::from_xyz(2.0, 0.0, 0.0)));
    world.spawn((
        id(3),
        Transform::from_xyz(3.0, 0.0, 0.0),
        Speed {
            value: 1.0,
            boost: 0,
        },
    ));
}

/// Play: the door is hurt, the crate destroyed, a hero spawned under the barrel.
fn play(world: &mut World) -> Entity {
    let door = find(world, id(1));
    world.get_mut::<Health>(door).unwrap().current = 37.5;
    let krate = find(world, id(2));
    world.despawn(krate);
    let barrel = find(world, id(3));
    world.get_mut::<Speed>(barrel).unwrap().boost = 7;
    let mut flags = BTreeMap::new();
    flags.insert("met the smith".to_owned(), true);
    flags.insert("a = b \"quoted\"".to_owned(), false);
    let mut counts = HashMap::new();
    counts.insert(9, vec![1, 2, 3]);
    counts.insert(4, Vec::new());
    let hero = world.spawn((
        Transform::from_xyz(0.0, 1.5, 0.0),
        Inventory {
            owner: "Офелия".to_owned(),
            items: vec![Item { id: 3, count: 2 }, Item { id: 11, count: 1 }],
            best: Some(Item { id: 11, count: 1 }),
            flags,
            counts,
            follow: door,
            cache: 99,
        },
    ));
    world.set_parent(hero, Some(barrel)).unwrap();
    // An effect: not saved.
    world.spawn((Transient, Transform::default()));
    world.spawn((Glow { phase: 0.5 },));
    world.insert_resource(Score {
        points: 1200,
        best: "Ofelia".to_owned(),
    });
    world.set_sim_tick(3600);
    hero
}

fn level_ids() -> HashSet<SceneId> {
    [id(1), id(2), id(3)].into_iter().collect()
}

fn take(world: &World) -> engine_save::SaveImage {
    let ids = level_ids();
    let mut image = capture(world, &CaptureOptions { level: Some(&ids) });
    image.set_state(
        "physics",
        &PhysicsState {
            gravity: Vec3::new(0.0, -3.7, 0.0),
            steps: 42,
        },
    );
    image
}

fn check_loaded(world: &mut World, save: &LoadedSave) {
    let report = apply(world, save).unwrap();
    assert_eq!(
        (report.matched, report.spawned, report.destroyed),
        (2, 1, 1),
        "{report:?}"
    );
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    let door = find(world, id(1));
    assert_eq!(world.get::<Health>(door).unwrap().current, 37.5);
    let barrel = find(world, id(3));
    assert_eq!(
        world.get::<Speed>(barrel),
        Some(&Speed {
            value: 1.0,
            boost: 7
        })
    );
    // The crate is gone, the hero back under the barrel, following the door.
    let mut q = QueryState::<(Entity, &Inventory)>::new(world);
    let (hero, inv) = q
        .iter_readonly(world)
        .next()
        .map(|(e, i)| (e, i.clone()))
        .unwrap();
    assert_eq!(world.parent(hero), Some(barrel));
    assert_eq!(inv.owner, "Офелия");
    assert_eq!(
        inv.items,
        [Item { id: 3, count: 2 }, Item { id: 11, count: 1 }]
    );
    assert_eq!(inv.best, Some(Item { id: 11, count: 1 }));
    assert_eq!(inv.flags.len(), 2);
    assert!(!inv.flags["a = b \"quoted\""]);
    assert_eq!(inv.counts[&9], [1, 2, 3]);
    assert!(inv.counts[&4].is_empty());
    assert_eq!(inv.follow, door);
    assert_eq!(inv.cache, 0, "skipped fields are not saved");
    assert_eq!(world.get::<Transform>(hero).unwrap().translation.y, 1.5);
    let mut glows = QueryState::<&Glow>::new(world);
    assert_eq!(
        glows.iter_readonly(world).count(),
        0,
        "skipped types are not saved"
    );
    assert_eq!(
        world.resource::<Score>(),
        Some(&Score {
            points: 1200,
            best: "Ofelia".to_owned()
        })
    );
    assert_eq!(world.sim_tick(), 3600);
    let physics: PhysicsState = save.state("physics").unwrap().unwrap();
    assert_eq!(physics.gravity, Vec3::new(0.0, -3.7, 0.0));
    assert_eq!(physics.steps, 42);
    assert!(save.state::<PhysicsState>("nothing").unwrap().is_none());
}

#[test]
fn a_played_level_comes_back_from_both_encodings() {
    let mut world = World::new();
    level(&mut world);
    play(&mut world);
    let image = take(&world);
    for encoding in [Encoding::Binary, Encoding::Text] {
        let header = SaveHeader {
            slot: "quick".to_owned(),
            ..SaveHeader::default()
        };
        let bytes = encode(&image, encoding, &header).unwrap();
        let save = decode(&bytes).unwrap();
        let mut again = World::new();
        level(&mut again);
        check_loaded(&mut again, &save);
    }
}

#[test]
fn the_text_is_readable_and_the_binary_dumps_to_the_same_text() {
    let mut world = World::new();
    level(&mut world);
    play(&mut world);
    let image = take(&world);
    let header = SaveHeader::default();
    let text = dump(&encode(&image, Encoding::Text, &header).unwrap()).unwrap();
    let binary = dump(&encode(&image, Encoding::Binary, &header).unwrap()).unwrap();
    assert_eq!(text, binary);
    for line in [
        "[save version=1 tick=3600]",
        "[entity 00000000000000000000000000000001]",
        "Health.current = 37.5",
        "Inventory.owner = \"Офелия\"",
        "Inventory.items.1.id = 11",
        "Inventory.flags[\"met the smith\"] = true",
        "Inventory.counts[4] = []",
        "Inventory.counts[9] = [1, 2, 3]",
        "Inventory.follow = @00000000000000000000000000000001",
        "Speed.boost = 7",
        "[resource Score]",
        "best = \"Ofelia\"",
        "[state physics PhysicsState]",
        "gravity = (0, -3.7, 0)",
        "[destroyed]",
        "00000000000000000000000000000002",
    ] {
        assert!(text.lines().any(|l| l == line), "`{line}` in\n{text}");
    }
    assert!(!text.contains("cache") && !text.contains("Glow"), "{text}");
}

#[test]
fn a_text_save_edited_by_hand_loads_as_it_is() {
    let mut world = World::new();
    level(&mut world);
    play(&mut world);
    let header = SaveHeader::default();
    let text = dump(&encode(&take(&world), Encoding::Text, &header).unwrap()).unwrap();
    // The developer fixes a state: the door's health, a new flag, an item added.
    let edited = text
        .replace("Health.current = 37.5", "Health.current = 80")
        .replace(
            "Inventory.items.1.count = 1",
            "Inventory.items.1.count = 1\nInventory.items.2.id = 5\nInventory.flags[\"fixed\"] = true",
        );
    let save = engine_save::decode(&wrap_text(&edited)).unwrap();
    let mut again = World::new();
    level(&mut again);
    apply(&mut again, &save).unwrap();
    let door = find(&mut again, id(1));
    assert_eq!(again.get::<Health>(door).unwrap().current, 80.0);
    let mut q = QueryState::<&Inventory>::new(&mut again);
    let inv = q.iter_readonly(&again).next().unwrap().clone();
    assert_eq!(inv.items.len(), 3);
    assert_eq!(inv.items[2], Item { id: 5, count: 0 });
    assert_eq!(inv.flags.get("fixed"), Some(&true));
    // A typo is an error with its line.
    let broken = text.replace("Health.current = 37.5", "Health.current = thirty");
    match engine_save::decode(&wrap_text(&broken)) {
        Ok(save) => {
            let mut w = World::new();
            level(&mut w);
            let e = apply(&mut w, &save).unwrap_err();
            assert!(e.to_string().contains("thirty"), "{e}");
        }
        Err(e) => panic!("{e}"),
    }
}

/// A text body put into a save file, as a development build writes it.
fn wrap_text(text: &str) -> Vec<u8> {
    engine_save::text_file(text, &SaveHeader::default()).unwrap()
}

#[test]
fn excluded_entities_are_not_made_and_references_to_them_are_none() {
    let mut world = World::new();
    level(&mut world);
    let hero = play(&mut world);
    let door = find(&mut world, id(1));
    let mut image = take(&world);
    image.exclude(door);
    let save = decode(&encode(&image, Encoding::Binary, &SaveHeader::default()).unwrap()).unwrap();
    let mut again = World::new();
    let report = apply(&mut again, &save).unwrap();
    assert_eq!(
        report.spawned, 2,
        "the barrel and the hero, without a level: {report:?}"
    );
    let mut q = QueryState::<&Inventory>::new(&mut again);
    assert_eq!(q.iter_readonly(&again).next().unwrap().follow, Entity::NULL);
    let _ = hero;
}

#[test]
fn saves_are_swapped_in_and_the_previous_one_kept() {
    let dir = std::env::temp_dir().join(format!("engine_save_files_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = engine_save::slot_path(&dir, "Slot 1: the castle");
    assert!(
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".save")
    );
    assert_ne!(
        path,
        engine_save::slot_path(&dir, "Slot 1_ the castle"),
        "names never share a file"
    );
    let mut world = World::new();
    level(&mut world);
    let first = capture(&world, &CaptureOptions::default());
    let header = |slot: &str, ms| SaveHeader {
        slot: slot.to_owned(),
        created_unix_ms: ms,
        play_ms: 61_000,
        ..SaveHeader::default()
    };
    let stats = write_save(&path, &first, Encoding::Binary, &header("first", 1)).unwrap();
    assert!(stats.file_bytes > 0 && stats.body_bytes > 0);
    play(&mut world);
    write_save(&path, &take(&world), Encoding::Text, &header("second", 2)).unwrap();
    let (h, save) = read_save(&path).unwrap();
    assert_eq!((h.slot.as_str(), h.play_ms), ("second", 61_000));
    assert_eq!(h.engine_version, env!("CARGO_PKG_VERSION"));
    assert!(save.entities() >= 3);
    // Damage the newest: the previous one is read.
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x40;
    std::fs::write(&path, &bytes).unwrap();
    let (h, _) = read_save(&path).unwrap();
    assert_eq!(h.slot, "first");
    let other = engine_save::slot_path(&dir, "other");
    write_save(&other, &first, Encoding::Binary, &header("other", 3)).unwrap();
    let slots = engine_save::list_slots(&dir);
    assert_eq!(
        slots[0].header.slot, "other",
        "newest first, the damaged one left out"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn damaged_files_are_refused_without_a_panic() {
    let mut world = World::new();
    level(&mut world);
    play(&mut world);
    let bytes = encode(&take(&world), Encoding::Binary, &SaveHeader::default()).unwrap();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..500 {
        let mut damaged = bytes.clone();
        let flips = 1 + next() % 4;
        for _ in 0..flips {
            let at = (next() % damaged.len() as u64) as usize;
            damaged[at] ^= 1 << (next() % 8);
        }
        assert!(decode(&damaged).is_err(), "the checksum catches it");
        let cut = (next() % bytes.len() as u64) as usize;
        assert!(decode(&bytes[..cut]).is_err());
    }
}
