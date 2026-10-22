//! Scene format: exact text, round trips, hierarchy order, schema changes and errors.

#![allow(clippy::unwrap_used)]

use engine_core::math::{Quat, Vec3};
use engine_ecs::{Entity, GlobalTransform, Transform, World, reflect_component};
use engine_reflect::Reflect;
use engine_scene::{RootOrder, SceneId, SceneRegistry, assign_ids, load, save};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
enum Team {
    #[default]
    Red,
    Blue,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
#[repr(C)]
struct Stats {
    speed: f64,
    level: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
#[repr(C)]
struct Unit {
    health: f32,
    alive: bool,
    team: Team,
    stats: Stats,
    ammo: [u16; 3],
    target: Entity,
    #[reflect(skip)]
    cache: u32,
}

impl Default for Unit {
    fn default() -> Self {
        Self {
            health: 100.0,
            alive: true,
            team: Team::Red,
            stats: Stats::default(),
            ammo: [0; 3],
            target: Entity::NULL,
            cache: 7,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
struct Marker;

reflect_component!(Unit, Marker);

fn registry() -> SceneRegistry {
    let mut r = SceneRegistry::with_engine_types();
    r.register::<Unit>().register::<Marker>();
    r
}

fn id(n: u128) -> SceneId {
    SceneId::from_u128(n)
}

#[test]
fn exact_text_of_a_small_scene() {
    let mut w = World::new();
    let car = w.spawn((id(2), Transform::from_xyz(10.0, 0.0, 0.5), Marker));
    let wheel = w.spawn((
        id(1),
        Transform::from_xyz(1.0, -0.5, 0.0).with_rotation(Quat::from_xyzw(0.0, 0.0, 0.0, 1.0)),
    ));
    w.set_parent(wheel, Some(car)).unwrap();
    let mut out = String::new();
    assert_eq!(save(&mut w, &registry(), &mut out).unwrap(), 2);
    let expected = "\
[scene version=4]

[entity 00000000000000000000000000000002]
Marker = {}
Transform.rotation = (0, 0, 0, 1)
Transform.translation = (10, 0, 0.5)
Transform.scale = (1, 1, 1)

[entity 00000000000000000000000000000001]
parent = 00000000000000000000000000000002
Transform.rotation = (0, 0, 0, 1)
Transform.translation = (1, -0.5, 0)
Transform.scale = (1, 1, 1)
";
    assert_eq!(out, expected);
}

#[test]
fn round_trip_keeps_values_order_and_references() {
    let r = registry();
    let mut w = World::new();
    let root = w.spawn((id(10), Transform::from_xyz(0.1, 0.2, 0.3)));
    let mut kids = Vec::new();
    // Ids in descending order: the file must follow child order, not id order.
    for i in 0..5u128 {
        let k = w.spawn((
            id(100 - i),
            Transform::from_scale(Vec3::splat(i as f32 + 0.25)),
        ));
        w.set_parent(k, Some(root)).unwrap();
        kids.push(k);
    }
    let unit = Unit {
        health: 0.1,
        alive: false,
        team: Team::Blue,
        stats: Stats {
            speed: 1.0 / 3.0,
            level: 9,
        },
        ammo: [1, 2, 65535],
        target: kids[3],
        cache: 1234,
    };
    w.insert(root, unit);
    let mut text = String::new();
    save(&mut w, &r, &mut text).unwrap();
    assert!(text.contains("Unit.team = Blue"), "{text}");
    assert!(text.contains("Unit.ammo = [1, 2, 65535]"), "{text}");
    assert!(
        text.contains("Unit.target = @00000000000000000000000000000061"),
        "{text}"
    );
    assert!(!text.contains("cache"), "skipped fields are not saved");

    let mut w2 = World::new();
    let report = load(&mut w2, &r, &text).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let root2 = report.entities[0];
    let kids2: Vec<Entity> = w2.children(root2).collect();
    assert_eq!(kids2.len(), 5);
    for (a, b) in kids.iter().zip(&kids2) {
        assert_eq!(
            w.get::<SceneId>(*a),
            w2.get::<SceneId>(*b),
            "child order kept"
        );
        assert_eq!(w.get::<Transform>(*a), w2.get::<Transform>(*b));
        assert!(w2.get::<GlobalTransform>(*b).is_some());
    }
    let u2 = *w2.get::<Unit>(root2).unwrap();
    assert_eq!(u2.target, kids2[3], "entity reference resolved");
    assert_eq!(u2.cache, 7, "skipped field loads as default");
    assert_eq!(
        Unit {
            target: kids[3],
            cache: 1234,
            ..u2
        },
        unit
    );
    assert_eq!(
        w.get::<Transform>(root),
        w2.get::<Transform>(root2),
        "bit-exact floats"
    );

    // Saving the loaded world gives the same text.
    let mut again = String::new();
    save(&mut w2, &r, &mut again).unwrap();
    assert_eq!(again, text);
}

#[test]
fn schema_changes_load_with_defaults_and_warnings() {
    let text = "\
# a comment
[scene version=1]
[entity 0000000000000000000000000000000a]
Unit.health = 5
Unit.mana = 3
Unit.stats.level = 2
";
    let mut w = World::new();
    let report = load(&mut w, &registry(), text).unwrap();
    let u = *w.get::<Unit>(report.entities[0]).unwrap();
    assert_eq!(u.health, 5.0);
    assert!(u.alive, "missing field keeps the default");
    assert_eq!(u.stats.level, 2);
    assert_eq!(report.warnings.len(), 1);
    assert_eq!(report.warnings[0].0, 5);
}

#[test]
fn errors_name_the_line_and_leave_the_world_untouched() {
    let r = registry();
    let cases = [
        ("", 0, "header"),
        ("[scene version=9]\n", 1, "newer"),
        (
            "[scene version=1]\nUnit.health = 1\n",
            2,
            "before the first",
        ),
        ("[scene version=1]\n[entity 1]\n", 2, "32 hex"),
        (
            "[scene version=1]\n[entity 0000000000000000000000000000000a]\nGhost.x = 1\n",
            3,
            "unknown component",
        ),
        (
            "[scene version=1]\n[entity 0000000000000000000000000000000a]\nUnit.health = lots\n",
            3,
            "number",
        ),
        (
            "[scene version=1]\n[entity 0000000000000000000000000000000a]\nUnit.team = Green\n",
            3,
            "variant",
        ),
        (
            "[scene version=1]\n[entity 0000000000000000000000000000000a]\nTransform.scale = (1, 2)\n",
            3,
            "3 numbers",
        ),
        (
            "[scene version=1]\n[entity 0000000000000000000000000000000a]\nparent = 0000000000000000000000000000000b\n",
            3,
            "not in this scene",
        ),
        (
            "[scene version=1]\n[entity 0000000000000000000000000000000a]\n[entity 0000000000000000000000000000000a]\n",
            3,
            "twice",
        ),
    ];
    for (text, line, needle) in cases {
        let mut w = World::new();
        let e = load(&mut w, &r, text).unwrap_err();
        assert_eq!(e.line, line, "{text:?}: {e}");
        assert!(e.message.contains(needle), "{text:?}: {e}");
        assert_eq!(w.len(), 0, "{text:?}: nothing kept");
    }
}

#[test]
fn ids_are_assigned_once_and_unsaved_parents_make_roots() {
    let mut w = World::new();
    let runtime = w.spawn((Transform::IDENTITY,));
    let child = w.spawn((id(5), Transform::IDENTITY));
    w.set_parent(child, Some(runtime)).unwrap();
    let mut text = String::new();
    assert_eq!(save(&mut w, &registry(), &mut text).unwrap(), 1);
    assert!(!text.contains("parent"), "{text}");
    assert_eq!(assign_ids(&mut w), 1);
    assert_eq!(assign_ids(&mut w), 0);
    assert_ne!(SceneId::new(), SceneId::new());
}

#[test]
fn roots_keep_their_order_and_their_blocks_stay_in_id_order() {
    let mut w = World::new();
    let a = w.spawn((id(1), Transform::IDENTITY, RootOrder(2048)));
    w.spawn((id(2), Transform::IDENTITY, RootOrder(1024)));
    w.spawn((id(3), Transform::IDENTITY));
    // A child's order is not written: children keep their parent's child order.
    let child = w.spawn((id(4), Transform::IDENTITY, RootOrder(7)));
    w.set_parent(child, Some(a)).unwrap();
    let mut out = String::new();
    save(&mut w, &registry(), &mut out).unwrap();
    let keys: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("[entity") || l.starts_with("order") || l.starts_with("parent"))
        .collect();
    assert_eq!(
        keys,
        [
            "[entity 00000000000000000000000000000001]",
            "order = 2048",
            "[entity 00000000000000000000000000000004]",
            "parent = 00000000000000000000000000000001",
            "[entity 00000000000000000000000000000002]",
            "order = 1024",
            "[entity 00000000000000000000000000000003]",
        ]
    );
    // Read back: the roots' orders, none for the root without one or the child.
    let mut w2 = World::new();
    let report = load(&mut w2, &registry(), &out).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.version, 4);
    let order = |i: usize| w2.get::<RootOrder>(report.entities[i]).copied();
    assert_eq!(
        [order(0), order(1), order(2), order(3)],
        [Some(RootOrder(2048)), None, Some(RootOrder(1024)), None]
    );
}

#[test]
fn an_order_on_a_child_warns_and_format_2_still_loads() {
    let text = "\
[scene version=3]

[entity 00000000000000000000000000000001]
Transform.translation = (1, 0, 0)

[entity 00000000000000000000000000000002]
parent = 00000000000000000000000000000001
order = 5
";
    let mut w = World::new();
    let report = load(&mut w, &registry(), text).unwrap();
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert_eq!(report.warnings[0].0, 8);
    assert!(w.get::<RootOrder>(report.entities[1]).is_none());
    // Version 2 has no order: its roots load without one.
    let v2 = "[scene version=2]\n\n[entity 00000000000000000000000000000001]\nTransform.translation = (1, 0, 0)\n";
    let mut w = World::new();
    let report = load(&mut w, &registry(), v2).unwrap();
    assert_eq!(report.version, 2);
    assert!(w.get::<RootOrder>(report.entities[0]).is_none());
    // A bad number is an error on its line.
    let bad = "[scene version=3]\n\n[entity 00000000000000000000000000000001]\norder = -1\n";
    let e = load(&mut World::new(), &registry(), bad).unwrap_err();
    assert_eq!(e.line, 4, "{e}");
}
