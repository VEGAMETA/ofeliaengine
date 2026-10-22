//! Prefab instances (ADR 0108's E5): a scene placed in another, its overrides applied, and
//! written back as what it changes.

#![allow(clippy::unwrap_used)]

use engine_ecs::{Entity, World, reflect_component};
use engine_reflect::Reflect;
use engine_scene::{
    AssetId, LoadOptions, LoadReport, Name, PrefabInstance, PrefabPart, Prefabs, SaveOptions,
    SceneId, SceneRegistry, load, load_with, save_with,
};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
struct Health(u32);

#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
struct Follows {
    target: Entity,
}

impl Default for Follows {
    fn default() -> Self {
        Self {
            target: Entity::NULL,
        }
    }
}

reflect_component!(Health, Follows);

fn registry() -> SceneRegistry {
    let mut r = SceneRegistry::with_engine_types();
    r.register::<Health>().register::<Follows>();
    r
}

/// Prefab texts by id, as a project's files would give them.
#[derive(Default)]
struct Files(HashMap<AssetId, String>);

impl Prefabs for Files {
    fn text(&self, prefab: AssetId, path: &str) -> Result<String, String> {
        self.0
            .get(&prefab)
            .cloned()
            .ok_or_else(|| format!("{path}: no such file"))
    }
}

fn id(n: u128) -> SceneId {
    SceneId::from_u128(n)
}

fn asset(n: u128) -> AssetId {
    AssetId::from_u128(n)
}

/// `scene` loaded with `files`' prefabs.
fn load_scene(scene: &str, files: &Files) -> (World, LoadReport) {
    let mut world = World::new();
    let report = load_with(
        &mut world,
        &registry(),
        scene,
        &LoadOptions {
            prefabs: Some(files),
        },
    )
    .unwrap();
    (world, report)
}

/// `world` saved as scene `scene`, its instances against `files`' prefabs.
fn save_scene(world: &mut World, report: &LoadReport, files: &Files) -> String {
    let mut out = String::new();
    save_with(
        world,
        &registry(),
        &SaveOptions {
            id: report.id,
            assets: Some(&report.assets),
            prefabs: Some(files),
        },
        &mut out,
    )
    .unwrap();
    out
}

fn entity(world: &mut World, report: &LoadReport, id: SceneId) -> Entity {
    report
        .entities
        .iter()
        .copied()
        .find(|&e| world.get::<SceneId>(e) == Some(&id))
        .unwrap_or_else(|| panic!("no entity {id}"))
}

fn name(world: &World, e: Entity) -> &str {
    world.get::<Name>(e).map_or("", Name::as_str)
}

/// A crate of one root and a lid under it.
fn crate_prefab(health: u32, lid: &str) -> String {
    format!(
        "[scene version=4 id={a}]

[entity {root}]
Health = {health}
Name = \"Crate\"
Transform.rotation = (0, 0, 0, 1)
Transform.translation = (0, 0, 0)
Transform.scale = (1, 1, 1)

[entity {lid_id}]
parent = {root}
Name = \"{lid}\"
",
        a = asset(0xa1),
        root = id(1),
        lid_id = id(2),
    )
}

fn crate_scene() -> String {
    format!(
        "[scene version=4 id={s}]

[instance {i}]
prefab = asset(\"props/crate.scene\", {a})
Transform.translation = (3, 0, 1)

[override {i}/{lid}]
Name = \"Open lid\"
",
        s = asset(0xb1),
        i = id(0x10),
        a = asset(0xa1),
        lid = id(2),
    )
}

#[test]
fn an_instance_places_its_prefab_and_saves_only_what_it_changes() {
    let mut files = Files::default();
    files.0.insert(asset(0xa1), crate_prefab(100, "Lid"));
    let scene = crate_scene();
    let (mut world, report) = load_scene(&scene, &files);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.prefabs, [asset(0xa1)]);
    // The prefab's root is the instance's entity: its id, the prefab's values, the
    // instance's line over them.
    let root = entity(&mut world, &report, id(0x10));
    assert_eq!(name(&world, root), "Crate");
    assert_eq!(world.get::<Health>(root), Some(&Health(100)));
    assert_eq!(
        world
            .get::<engine_ecs::Transform>(root)
            .unwrap()
            .translation
            .x,
        3.0
    );
    assert_eq!(
        world.get::<PrefabInstance>(root).map(|i| i.prefab.file),
        Some(asset(0xa1))
    );
    // The lid: a derived id, the override's name, a part of the instance.
    let lid = entity(&mut world, &report, SceneId::derive(id(0x10), id(2)));
    assert_eq!(name(&world, lid), "Open lid");
    assert_eq!(world.parent(lid), Some(root));
    assert_eq!(
        world.get::<PrefabPart>(lid),
        Some(&PrefabPart {
            instance: id(0x10),
            local: id(2),
        })
    );
    // Saved back as written: only what the instance changes.
    assert_eq!(save_scene(&mut world, &report, &files), scene);
    // A value changed on the root, then the lid deleted.
    world.get_mut::<Health>(root).unwrap().0 = 50;
    world.despawn(lid);
    let saved = save_scene(&mut world, &report, &files);
    assert!(
        saved.contains("Health = 50\nTransform.translation"),
        "{saved}"
    );
    assert!(saved.contains(&format!("remove = @{}\n", id(2))), "{saved}");
    assert!(!saved.contains("[override"), "{saved}");
    // And read back the same.
    let (mut again, report) = load_scene(&saved, &files);
    let root = entity(&mut again, &report, id(0x10));
    assert_eq!(again.get::<Health>(root), Some(&Health(50)));
    assert_eq!(report.entities.len(), 1, "the lid stays deleted");
}

#[test]
fn a_prefab_changed_reaches_its_instances_but_not_their_overrides() {
    let mut files = Files::default();
    files.0.insert(asset(0xa1), crate_prefab(120, "Hatch"));
    let (mut world, report) = load_scene(&crate_scene(), &files);
    let root = entity(&mut world, &report, id(0x10));
    let lid = entity(&mut world, &report, SceneId::derive(id(0x10), id(2)));
    assert_eq!(
        world.get::<Health>(root),
        Some(&Health(120)),
        "the prefab's"
    );
    assert_eq!(name(&world, lid), "Open lid", "the override's");
}

#[test]
fn instances_inside_instances_take_paths() {
    let (wheel, car) = (asset(0xc1), asset(0xc2));
    let mut files = Files::default();
    files.0.insert(
        wheel,
        format!(
            "[scene version=4 id={wheel}]

[entity {w}]
Name = \"Wheel\"

[entity {bolt}]
parent = {w}
Name = \"Bolt\"
",
            w = id(0x31),
            bolt = id(0x32),
        ),
    );
    files.0.insert(
        car,
        format!(
            "[scene version=4 id={car}]

[entity {c}]
Name = \"Car\"

[instance {j1}]
prefab = asset(\"wheel.scene\", {wheel})
parent = {c}

[instance {j2}]
prefab = asset(\"wheel.scene\", {wheel})
parent = {c}

[override {j2}/{bolt}]
Name = \"Spare bolt\"
",
            c = id(0x21),
            j1 = id(0x22),
            j2 = id(0x23),
            bolt = id(0x32),
        ),
    );
    let scene = format!(
        "[scene version=4 id={s}]

[instance {i}]
prefab = asset(\"car.scene\", {car})

[override {i}/{j1}/{bolt}]
Name = \"Gold bolt\"
",
        s = asset(0xb2),
        i = id(0x40),
        j1 = id(0x22),
        bolt = id(0x32),
    );
    let (mut world, report) = load_scene(&scene, &files);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.entities.len(), 5, "the car, two wheels, two bolts");
    let car_root = entity(&mut world, &report, id(0x40));
    assert_eq!(name(&world, car_root), "Car");
    // A wheel is the instance's entity inside the car: its id derived from the car's.
    let j1 = entity(&mut world, &report, SceneId::derive(id(0x40), id(0x22)));
    assert_eq!(name(&world, j1), "Wheel");
    assert_eq!(world.parent(j1), Some(car_root));
    // Each bolt by its path: the scene's override, and the car's own.
    let gold = SceneId::of_path(&[id(0x40), id(0x22), id(0x32)]);
    let spare = SceneId::of_path(&[id(0x40), id(0x23), id(0x32)]);
    let gold = entity(&mut world, &report, gold);
    let spare = entity(&mut world, &report, spare);
    assert_eq!(name(&world, gold), "Gold bolt");
    assert_eq!(name(&world, spare), "Spare bolt");
    assert_eq!(world.parent(gold), Some(j1));
    // Saved back as written.
    assert_eq!(save_scene(&mut world, &report, &files), scene);
}

#[test]
fn what_cannot_be_placed_is_kept_as_written() {
    // A prefab nobody has, with an override of it; and a crate whose override names an
    // entity the prefab no longer has.
    let scene = format!(
        "[scene version=4 id={s}]

[instance {gone}]
prefab = asset(\"props/gone.scene\", {g})
Transform.translation = (1, 2, 3)
Health = 7

[override {gone}/{x}]
Name = \"Still here\"

[instance {i}]
prefab = asset(\"props/crate.scene\", {a})

[override {i}/{x}]
Name = \"Ghost\"
",
        s = asset(0xb3),
        gone = id(0x50),
        g = asset(0xdd),
        x = id(0x99),
        i = id(0x60),
        a = asset(0xa1),
    );
    let mut files = Files::default();
    files.0.insert(asset(0xa1), crate_prefab(100, "Lid"));
    let (mut world, report) = load_scene(&scene, &files);
    assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    assert!(report.warnings[0].1.contains("cannot be placed"));
    assert!(report.warnings[1].1.contains("is kept as written"));
    // The instance whose prefab is missing: its entity alone, placed by its own lines.
    let missing = entity(&mut world, &report, id(0x50));
    assert!(world.get::<PrefabInstance>(missing).unwrap().is_missing());
    assert_eq!(world.get::<Health>(missing), Some(&Health(7)));
    // Written back as they were.
    assert_eq!(save_scene(&mut world, &report, &files), scene);
    // Loaded without prefabs at all, and saved without: the same text.
    let mut plain = World::new();
    let report = load(&mut plain, &registry(), &scene).unwrap();
    let mut out = String::new();
    save_with(
        &mut plain,
        &registry(),
        &SaveOptions {
            id: report.id,
            assets: Some(&report.assets),
            prefabs: None,
        },
        &mut out,
    )
    .unwrap();
    assert_eq!(out, scene);
}

#[test]
fn a_prefab_placing_itself_is_refused() {
    let looped = asset(0xe1);
    let mut files = Files::default();
    files.0.insert(
        looped,
        format!(
            "[scene version=4 id={looped}]

[entity {r}]
Name = \"Loop\"

[instance {inner}]
prefab = asset(\"loop.scene\", {looped})
parent = {r}
",
            r = id(1),
            inner = id(2),
        ),
    );
    let scene = format!(
        "[scene version=4 id={s}]

[instance {i}]
prefab = asset(\"loop.scene\", {looped})
",
        s = asset(0xb4),
        i = id(0x70),
    );
    let (mut world, report) = load_scene(&scene, &files);
    assert!(
        report
            .warnings
            .iter()
            .any(|(_, w)| w.contains("would place itself")),
        "{:?}",
        report.warnings
    );
    // The outer instance placed once, the inner kept as an instance not placed.
    let inner = entity(&mut world, &report, SceneId::derive(id(0x70), id(2)));
    assert!(world.get::<PrefabInstance>(inner).unwrap().is_missing());
    assert_eq!(report.entities.len(), 2);
}

#[test]
fn references_into_an_instance_are_written_as_paths() {
    let mut files = Files::default();
    files.0.insert(asset(0xa1), crate_prefab(100, "Lid"));
    let scene = format!(
        "[scene version=4 id={s}]

[entity {f}]
Follows.target = @{i}/{lid}
Name = \"Fan\"

[instance {i}]
prefab = asset(\"props/crate.scene\", {a})
",
        s = asset(0xb5),
        f = id(0x05),
        i = id(0x10),
        lid = id(2),
        a = asset(0xa1),
    );
    let (mut world, report) = load_scene(&scene, &files);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let fan = entity(&mut world, &report, id(0x05));
    let lid = entity(&mut world, &report, SceneId::derive(id(0x10), id(2)));
    assert_eq!(world.get::<Follows>(fan).unwrap().target, lid);
    assert_eq!(save_scene(&mut world, &report, &files), scene);
}

#[test]
fn a_prefab_of_several_roots_is_held_by_the_instance() {
    let pair = asset(0xf1);
    let mut files = Files::default();
    files.0.insert(
        pair,
        format!(
            "[scene version=4 id={pair}]

[entity {a}]
Name = \"Left\"

[entity {b}]
Name = \"Right\"
",
            a = id(1),
            b = id(2),
        ),
    );
    let scene = format!(
        "[scene version=4 id={s}]

[instance {i}]
prefab = asset(\"pair.scene\", {pair})
Name = \"Pair\"
",
        s = asset(0xb6),
        i = id(0x80),
    );
    let (mut world, report) = load_scene(&scene, &files);
    let holder = entity(&mut world, &report, id(0x80));
    assert_eq!(name(&world, holder), "Pair");
    let left = entity(&mut world, &report, SceneId::derive(id(0x80), id(1)));
    assert_eq!(world.parent(left), Some(holder));
    assert_eq!(name(&world, left), "Left");
    assert_eq!(save_scene(&mut world, &report, &files), scene);
}

#[test]
fn a_variant_is_a_scene_whose_root_is_an_instance() {
    let (door, red_door) = (asset(0x91), asset(0x92));
    let mut files = Files::default();
    files.0.insert(
        door,
        format!(
            "[scene version=4 id={door}]

[entity {d}]
Health = 10
Name = \"Door\"
",
            d = id(1),
        ),
    );
    files.0.insert(
        red_door,
        format!(
            "[scene version=4 id={red_door}]

[instance {v}]
prefab = asset(\"door.scene\", {door})
Name = \"Red door\"
",
            v = id(2),
        ),
    );
    let scene = format!(
        "[scene version=4 id={s}]

[instance {i}]
prefab = asset(\"red_door.scene\", {red_door})
Health = 3
",
        s = asset(0xb7),
        i = id(0x90),
    );
    let (mut world, report) = load_scene(&scene, &files);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.entities.len(), 1, "one door, the variant's root");
    let root = entity(&mut world, &report, id(0x90));
    assert_eq!(name(&world, root), "Red door");
    assert_eq!(world.get::<Health>(root), Some(&Health(3)));
    assert_eq!(
        world.get::<PrefabInstance>(root).unwrap().prefab.file,
        red_door,
        "the instance it shows is the outermost"
    );
    assert_eq!(save_scene(&mut world, &report, &files), scene);
}
