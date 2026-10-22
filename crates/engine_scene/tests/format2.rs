//! Scene format 2 (ADR 0108): strings and containers in the saves' grammar, structs of one
//! unnamed field written as that field, asset references by path and id, the scene's own
//! id, and files of format 1 still loading.

#![allow(clippy::unwrap_used)]

use engine_ecs::{Entity, World, reflect_component};
use engine_reflect::Reflect;
use engine_scene::{
    AssetId, AssetPaths, AssetRef, Name, SaveOptions, SceneId, SceneRegistry, load, read_value,
    save, save_with, write_value,
};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq, Default, Reflect)]
struct Item {
    id: u32,
    count: u16,
}

#[derive(Debug, Clone, PartialEq, Default, Reflect)]
struct Inventory {
    owner: String,
    items: Vec<Item>,
    weights: Vec<f32>,
    tags: Vec<String>,
    flags: BTreeMap<String, bool>,
    counts: HashMap<u32, f32>,
    best: Option<u32>,
    spare: Option<Item>,
    note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
struct Health(f32);

#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
struct Look {
    mesh: AssetRef,
    material: AssetRef,
    sound: AssetRef,
}

#[derive(Debug, Clone, PartialEq, Default, Reflect)]
struct Links {
    targets: Vec<Entity>,
}

reflect_component!(Inventory, Health, Look, Links);

fn registry() -> SceneRegistry {
    let mut r = SceneRegistry::with_engine_types();
    r.register::<Inventory>()
        .register::<Health>()
        .register::<Look>()
        .register::<Links>();
    r
}

fn crate_inventory() -> Inventory {
    Inventory {
        owner: String::from("Ofelia \"O\""),
        items: vec![Item { id: 3, count: 2 }],
        weights: vec![0.5, 1.0],
        tags: vec![String::from("red"), String::from("fast")],
        flags: BTreeMap::from([(String::from("met the smith"), true)]),
        counts: HashMap::from([(9, 1.5), (2, 0.25)]),
        best: Some(7),
        spare: None,
        note: Some(String::from("x = y")),
    }
}

const CRATE_FILE: AssetId = AssetId::from_u128(5);

#[test]
fn exact_text_of_containers_names_and_assets() {
    let mut w = World::new();
    let look = Look {
        mesh: AssetRef::part(CRATE_FILE, "Crate"),
        material: AssetRef::NONE,
        sound: AssetRef::file(AssetId::from_u128(6)),
    };
    w.spawn((
        SceneId::from_u128(1),
        Health(80.0),
        crate_inventory(),
        look,
        Name::new("Crate"),
    ));
    let mut paths = AssetPaths::new();
    paths.insert(look.mesh, "props/crate.gltf#Crate");
    let options = SaveOptions {
        id: Some(AssetId::from_u128(9)),
        assets: Some(&paths),
        prefabs: None,
    };
    let mut out = String::new();
    save_with(&mut w, &registry(), &options, &mut out).unwrap();
    let expected = "\
[scene version=4 id=00000000000000000000000000000009]

[entity 00000000000000000000000000000001]
Health = 80
Inventory.owner = \"Ofelia \\\"O\\\"\"
Inventory.items.0.id = 3
Inventory.items.0.count = 2
Inventory.weights = [0.5, 1]
Inventory.tags = [\"red\", \"fast\"]
Inventory.flags[\"met the smith\"] = true
Inventory.counts[2] = 0.25
Inventory.counts[9] = 1.5
Inventory.best = 7
Inventory.spare = none
Inventory.note = \"x = y\"
Look.mesh = asset(\"props/crate.gltf#Crate\", 00000000000000000000000000000005)
Look.material = none
Look.sound = asset(\"\", 00000000000000000000000000000006)
Name = \"Crate\"
";
    assert_eq!(out, expected);

    // Loading gives the values, the id and the paths back; saving again the same text.
    let mut w2 = World::new();
    let report = load(&mut w2, &registry(), &out).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.version, 4);
    assert_eq!(report.id, Some(AssetId::from_u128(9)));
    let e = report.entities[0];
    assert_eq!(w2.get::<Inventory>(e), Some(&crate_inventory()));
    assert_eq!(w2.get::<Health>(e), Some(&Health(80.0)));
    assert_eq!(w2.get::<Look>(e), Some(&look));
    assert_eq!(w2.get::<Name>(e).map(Name::as_str), Some("Crate"));
    assert_eq!(report.assets.get(look.mesh), Some("props/crate.gltf#Crate"));
    let mut again = String::new();
    let options = SaveOptions {
        id: report.id,
        assets: Some(&report.assets),
        prefabs: None,
    };
    save_with(&mut w2, &registry(), &options, &mut again).unwrap();
    assert_eq!(again, out);
}

#[test]
fn empty_and_filled_containers_round_trip() {
    let r = registry();
    let mut w = World::new();
    let a = w.spawn((SceneId::from_u128(1), Inventory::default()));
    let b = w.spawn((
        SceneId::from_u128(2),
        Inventory {
            spare: Some(Item { id: 1, count: 0 }),
            items: vec![Item { id: 4, count: 1 }, Item { id: 5, count: 6 }],
            ..crate_inventory()
        },
        Links {
            targets: vec![a, Entity::NULL],
        },
    ));
    let mut text = String::new();
    save(&mut w, &r, &mut text).unwrap();
    assert!(text.contains("Inventory.items = []"), "{text}");
    assert!(text.contains("Inventory.flags = {}"), "{text}");
    assert!(text.contains("Inventory.best = none"), "{text}");
    assert!(text.contains("Inventory.spare.id = 1"), "{text}");
    assert!(
        text.contains("Links.targets = [@00000000000000000000000000000001, @none]"),
        "{text}"
    );
    let mut w2 = World::new();
    let report = load(&mut w2, &r, &text).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let (a2, b2) = (report.entities[0], report.entities[1]);
    assert_eq!(w2.get::<Inventory>(a2), w.get::<Inventory>(a));
    assert_eq!(w2.get::<Inventory>(b2), w.get::<Inventory>(b));
    assert_eq!(w2.get::<Links>(b2).unwrap().targets, [a2, Entity::NULL]);
}

#[test]
fn references_without_paths_keep_their_part() {
    let mut w = World::new();
    let look = Look {
        mesh: AssetRef::part(CRATE_FILE, "Lid"),
        ..Look::default()
    };
    w.spawn((SceneId::from_u128(1), look));
    let mut text = String::new();
    save(&mut w, &registry(), &mut text).unwrap();
    assert!(text.contains("asset(\"#~"), "{text}");
    let mut w2 = World::new();
    let report = load(&mut w2, &registry(), &text).unwrap();
    assert_eq!(w2.get::<Look>(report.entities[0]), Some(&look));
}

#[test]
fn format_1_files_still_load() {
    // Format 1 wrote a struct of one unnamed field through `.0` and had no scene id.
    let text = "\
[scene version=1]

[entity 0000000000000000000000000000000a]
Health.0 = 5
Transform.translation = (1, 2, 3)
";
    let mut w = World::new();
    let report = load(&mut w, &registry(), text).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.version, 1);
    assert_eq!(report.id, None);
    assert_eq!(w.get::<Health>(report.entities[0]), Some(&Health(5.0)));
}

#[test]
fn bad_values_name_their_line() {
    let r = registry();
    let head = "[scene version=2]\n[entity 0000000000000000000000000000000a]\n";
    let cases = [
        ("Look.mesh = asset(\"x\", 12)\n", "asset("),
        ("Look.mesh = crate.gltf\n", "asset("),
        ("Inventory.items.x.id = 1\n", "list index"),
        ("Inventory.counts[\"a\"] = 1\n", "number"),
        ("Inventory.owner = no quotes\n", "string"),
        ("Inventory = 3\n", "Inventory.<field>"),
    ];
    for (line, needle) in cases {
        let text = format!("{head}{line}");
        let mut w = World::new();
        let e = load(&mut w, &r, &text).unwrap_err();
        assert_eq!(e.line, 3, "{line:?}: {e}");
        assert!(e.message.contains(needle), "{line:?}: {e}");
        assert_eq!(w.len(), 0, "{line:?}: nothing kept");
    }
    let mut w = World::new();
    let bad_id = "[scene version=2 id=zz]\n";
    let e = load(&mut w, &r, bad_id).unwrap_err();
    assert!(e.message.contains("scene id"), "{e}");
}

#[test]
fn the_programs_types_need_no_registration() {
    let r = SceneRegistry::from_program();
    for name in ["Transform", "Name", "Inventory", "Health", "Look", "Links"] {
        assert!(r.contains(name), "{name}");
    }
    assert!(
        !r.contains("Parent") && !r.contains("Children"),
        "the hierarchy's links are runtime data"
    );
    // Read and written through the program's list, no Rust type behind the registry.
    let mut w = World::new();
    w.spawn((
        SceneId::from_u128(1),
        crate_inventory(),
        Health(3.0),
        Name::new("Crate"),
    ));
    let mut text = String::new();
    save(&mut w, &r, &mut text).unwrap();
    let mut w2 = World::new();
    let report = load(&mut w2, &r, &text).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let e = report.entities[0];
    assert_eq!(w2.get::<Inventory>(e), Some(&crate_inventory()));
    assert_eq!(w2.get::<Health>(e), Some(&Health(3.0)));
    assert_eq!(w2.get::<Name>(e).map(Name::as_str), Some("Crate"));
    // The same text as a registry given the types by hand.
    let mut by_hand = String::new();
    save(&mut w, &registry(), &mut by_hand).unwrap();
    assert_eq!(text, by_hand);
}

#[test]
fn a_component_is_added_by_name_at_its_default_with_what_it_requires() {
    static GAUGE: engine_reflect::TypeInfo = engine_reflect::TypeInfo {
        name: "SceneTestGauge",
        size: 4,
        align: 4,
        kind: engine_reflect::TypeKind::Primitive(engine_reflect::Primitive::F32),
        attrs: engine_reflect::Attrs::NONE,
    };
    // A script's component, made at run time: its default bytes.
    // SAFETY: an f32, plain data, its default one.
    unsafe {
        engine_ecs::add_runtime_type(engine_ecs::RuntimeType {
            info: &GAUGE,
            component: "script::SceneTestGauge".into(),
            layout: core::alloc::Layout::new::<f32>(),
            defaults: 0.5f32.to_le_bytes().into(),
            source: "scripts/gauge.luau".into(),
        });
    }
    let r = SceneRegistry::from_program();
    let mut w = World::new();
    let e = w.spawn((SceneId::from_u128(1),));
    // The program's list, the engine's own types, and the script's.
    assert!(r.insert_default(&mut w, e, "Inventory"));
    assert_eq!(w.get::<Inventory>(e), Some(&Inventory::default()));
    assert!(r.insert_default(&mut w, e, "Transform"));
    assert!(
        w.get::<engine_ecs::GlobalTransform>(e).is_some(),
        "what a transform requires"
    );
    assert!(r.insert_default(&mut w, e, "SceneTestGauge"));
    let id = w.components().id_by_name("script::SceneTestGauge").unwrap();
    // SAFETY: the gauge is an f32.
    let level = unsafe { w.get_raw(e, id).unwrap().cast::<f32>().read() };
    assert_eq!(level, 0.5);
    assert!(!r.insert_default(&mut w, e, "Nothing"));
    let gone = w.spawn(());
    w.despawn(gone);
    assert!(!r.insert_default(&mut w, gone, "Health"));
}

#[derive(Debug, Clone, PartialEq, Default, Reflect)]
struct Paint {
    color: [f32; 4],
    shine: f32,
    texture: AssetRef,
    layers: Vec<String>,
}

#[test]
fn files_of_one_value_round_trip() {
    let paint = Paint {
        color: [0.8, 0.15, 0.1, 1.0],
        shine: 0.4,
        texture: AssetRef::file(AssetId::from_u128(3)),
        layers: vec![String::from("base")],
    };
    let mut paths = AssetPaths::new();
    paths.insert(paint.texture, "textures/rust.png");
    let mut out = String::new();
    write_value(
        "paint",
        1,
        Some(AssetId::from_u128(8)),
        &paint,
        Some(&paths),
        &mut out,
    );
    let expected = "\
[paint version=1 id=00000000000000000000000000000008]

color = [0.8, 0.15, 0.1, 1]
shine = 0.4
texture = asset(\"textures/rust.png\", 00000000000000000000000000000003)
layers = [\"base\"]
";
    assert_eq!(out, expected);
    let mut back = Paint::default();
    let report = read_value("paint", 1, &out, &mut back).unwrap();
    assert_eq!(back, paint);
    assert_eq!(report.id, Some(AssetId::from_u128(8)));
    assert_eq!(report.assets.get(paint.texture), Some("textures/rust.png"));
    // Another kind or a newer version is refused; a gone field warns.
    let e = read_value("material", 1, &out, &mut Paint::default()).unwrap_err();
    assert!(e.message.contains("[material"), "{e}");
    let e = read_value("paint", 1, "[paint version=2]\n", &mut Paint::default()).unwrap_err();
    assert!(e.message.contains("newer"), "{e}");
    let warned = read_value(
        "paint",
        1,
        "[paint version=1]\nglow = 3\n",
        &mut Paint::default(),
    );
    assert_eq!(warned.unwrap().warnings.len(), 1);
}

/// Undo's text form of a component that is not plain data (`write_lines_with`,
/// `read_lines`): every container, entity references by scene id, asset references
/// without paths.
#[test]
fn a_components_lines_round_trip_with_entity_ids() {
    use engine_scene::text::ReadRefs;
    use engine_scene::{read_lines, write_lines_with};

    let mut world = World::new();
    let (a, b) = (world.spawn(()), world.spawn(()));
    let (id_a, id_b) = (SceneId::new(), SceneId::new());
    let ids = |e: Entity| {
        if e == a {
            Some(id_a)
        } else if e == b {
            Some(id_b)
        } else {
            None
        }
    };
    struct Back {
        a: (SceneId, Entity),
        b: (SceneId, Entity),
    }
    impl ReadRefs for Back {
        fn entity(&mut self, label: &str) -> Result<Entity, String> {
            let id = SceneId::parse(label).ok_or_else(|| label.to_owned())?;
            Ok([self.a, self.b]
                .into_iter()
                .find(|&(i, _)| i == id)
                .map_or(Entity::NULL, |(_, e)| e))
        }
    }
    let mut refs = Back {
        a: (id_a, a),
        b: (id_b, b),
    };
    fn lines<T: Reflect>(value: &T, ids: &dyn Fn(Entity) -> Option<SceneId>) -> String {
        let mut out = String::new();
        // SAFETY: a live `T`, described by its type.
        unsafe {
            write_lines_with(
                T::TYPE_INFO,
                (value as *const T).cast(),
                ids,
                None,
                &mut out,
            );
        }
        out
    }

    let links = Links {
        targets: vec![b, Entity::NULL, a],
    };
    let text = lines(&links, &ids);
    assert_eq!(text, format!("targets = [@{id_b}, @none, @{id_a}]\n"));
    let mut back = Links::default();
    // SAFETY: a live `Links`.
    let warnings =
        unsafe { read_lines(Links::TYPE_INFO, (&raw mut back).cast(), &text, &mut refs) }.unwrap();
    assert!(warnings.is_empty());
    assert_eq!(back, links);

    let inventory = crate_inventory();
    let text = lines(&inventory, &ids);
    let mut back = Inventory::default();
    // SAFETY: a live `Inventory`.
    unsafe {
        read_lines(
            Inventory::TYPE_INFO,
            (&raw mut back).cast(),
            &text,
            &mut refs,
        )
    }
    .unwrap();
    assert_eq!(back, inventory, "{text}");

    let look = Look {
        mesh: AssetRef::part(CRATE_FILE, "Lid"),
        material: AssetRef::file(CRATE_FILE),
        sound: AssetRef::NONE,
    };
    let text = lines(&look, &ids);
    let mut back = Look::default();
    // SAFETY: a live `Look`.
    unsafe { read_lines(Look::TYPE_INFO, (&raw mut back).cast(), &text, &mut refs) }.unwrap();
    assert_eq!(back, look, "{text}");

    // A value written whole, a field gone, a bad value with its line.
    let name = Name::new("Door \"A\"");
    let text = lines(&name, &ids);
    assert_eq!(text, " = \"Door \\\"A\\\"\"\n");
    let mut back = Name::default();
    // SAFETY: a live `Name`.
    unsafe { read_lines(Name::TYPE_INFO, (&raw mut back).cast(), &text, &mut refs) }.unwrap();
    assert_eq!(back, name);
    let mut back = Look::default();
    // SAFETY: a live `Look`.
    let warnings = unsafe {
        read_lines(
            Look::TYPE_INFO,
            (&raw mut back).cast(),
            "glow = 3\n",
            &mut refs,
        )
    }
    .unwrap();
    assert_eq!(warnings.len(), 1);
    // SAFETY: a live `Look`.
    let e = unsafe {
        read_lines(
            Look::TYPE_INFO,
            (&raw mut back).cast(),
            "sound = none\nmesh = 7\n",
            &mut refs,
        )
    }
    .unwrap_err();
    assert_eq!(e.line, 2, "{e}");
}
