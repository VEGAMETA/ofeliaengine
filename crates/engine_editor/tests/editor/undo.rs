//! Undo and redo: edits, spawns, despawned subtrees, reparenting, sibling order, entity
//! references, components that are not plain data (kept as scene lines), merging, cancel,
//! budget, and a randomized walk checked against scene text.

#![allow(clippy::unwrap_used)]

use engine_ecs::{Entity, Transform, World, reflect_component};
use engine_editor::History;
use engine_reflect::Reflect;
use engine_scene::{Name, SceneId, SceneRegistry, save};

#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
#[repr(C)]
struct Follow {
    target: Entity,
    distance: f32,
}

impl Default for Follow {
    fn default() -> Self {
        Self {
            target: Entity::NULL,
            distance: 1.0,
        }
    }
}

// Plain data, so undo can record it byte-wise.
// SAFETY: two plain fields, no padding (`Entity` is two u32s, then an f32).
unsafe impl engine_ecs::bytemuck::Zeroable for Follow {}
// SAFETY: as above.
unsafe impl engine_ecs::bytemuck::Pod for Follow {}
reflect_component!(pod Follow);

/// Not plain data: a string, entity references in a list, an optional value.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
struct Tag {
    label: String,
    friends: Vec<Entity>,
    level: Option<u32>,
}

reflect_component!(Tag);

fn registry() -> SceneRegistry {
    let mut r = SceneRegistry::with_engine_types();
    r.register::<Follow>().register::<Tag>();
    r
}

fn text(world: &mut World) -> String {
    let mut s = String::new();
    save(world, &registry(), &mut s).unwrap();
    s
}

fn spawn(world: &mut World, x: f32) -> Entity {
    world.spawn((SceneId::new(), Transform::from_xyz(x, 0.0, 0.0)))
}

#[test]
fn spawn_edit_reparent_despawn_and_back() {
    let mut w = World::new();
    let mut h = History::new();
    let initial = text(&mut w);

    h.begin("Create", None);
    let root = spawn(&mut w, 0.0);
    h.touch_new(&mut w, root);
    let kids: Vec<Entity> = (0..3).map(|i| spawn(&mut w, i as f32)).collect();
    for &k in &kids {
        h.touch_new(&mut w, k);
        w.set_parent(k, Some(root)).unwrap();
    }
    w.insert(
        root,
        Follow {
            target: kids[2],
            distance: 3.0,
        },
    );
    assert!(h.commit(&mut w));
    let created = text(&mut w);

    h.begin("Move", None);
    h.touch(&mut w, kids[0]);
    w.get_mut::<Transform>(kids[0]).unwrap().translation.y = 7.0;
    h.commit(&mut w);
    let moved = text(&mut w);

    h.begin("Reorder", None);
    h.touch(&mut w, kids[2]);
    w.set_parent_at(kids[2], Some(root), 0).unwrap();
    h.commit(&mut w);
    let reordered = text(&mut w);

    h.begin("Delete", None);
    h.delete_subtree(&mut w, root);
    h.commit(&mut w);
    assert_eq!(text(&mut w), initial);

    assert_eq!(h.undo(&mut w).as_deref(), Some("Delete"));
    assert_eq!(
        text(&mut w),
        reordered,
        "subtree, order and reference restored"
    );
    let root2 = find(&mut w);
    let follow = *w.get::<Follow>(root2).unwrap();
    assert_eq!(
        w.children(root2).next(),
        Some(follow.target),
        "reference follows the respawned child"
    );
    assert_eq!(h.undo(&mut w).as_deref(), Some("Reorder"));
    assert_eq!(text(&mut w), moved);
    assert_eq!(h.undo(&mut w).as_deref(), Some("Move"));
    assert_eq!(text(&mut w), created);
    assert_eq!(h.undo(&mut w).as_deref(), Some("Create"));
    assert_eq!(text(&mut w), initial);
    assert!(h.undo(&mut w).is_none());

    for expected in [&created, &moved, &reordered, &initial] {
        h.redo(&mut w).unwrap();
        assert_eq!(&text(&mut w), expected);
    }
    assert!(!h.can_redo());
}

/// The entity with a `Follow` component (the root in this test).
fn find(w: &mut World) -> Entity {
    let mut q = engine_ecs::QueryState::<(Entity, &Follow)>::new(w);
    q.iter_readonly(w).next().unwrap().0
}

/// The entity with this name.
fn named(w: &mut World, name: &str) -> Entity {
    let mut q = engine_ecs::QueryState::<(Entity, &Name)>::new(w);
    q.iter_readonly(w)
        .find(|(_, n)| n.as_str() == name)
        .unwrap()
        .0
}

#[test]
fn components_that_are_not_plain_data_undo_as_scene_lines() {
    let mut w = World::new();
    let mut h = History::new();
    let a = spawn(&mut w, 0.0);
    let b = spawn(&mut w, 1.0);
    w.insert(a, Name::new("A"));
    w.insert(
        b,
        Tag {
            label: String::from("b \"quoted\""),
            friends: vec![a, Entity::NULL],
            level: Some(2),
        },
    );
    let start = text(&mut w);

    h.begin("Rename", None);
    h.touch(&mut w, a);
    w.get_mut::<Name>(a).unwrap().0 = String::from("Door");
    assert!(h.commit(&mut w));
    let renamed = text(&mut w);
    assert!(renamed.contains("Name = \"Door\""), "{renamed}");

    // `b` refers to `a` through a list in a component that is not plain data.
    h.begin("Delete", None);
    h.delete_subtree(&mut w, a);
    assert!(h.commit(&mut w));
    h.undo(&mut w);
    assert_eq!(text(&mut w), renamed);
    let a2 = named(&mut w, "Door");
    assert_ne!(a2, a, "a new handle for the restored entity");
    assert_eq!(
        w.get::<Tag>(b).unwrap().friends,
        [a2, Entity::NULL],
        "the reference follows the restored entity"
    );

    // Adding one and undoing removes it.
    h.begin("Name b", None);
    h.touch(&mut w, b);
    w.insert(b, Name::new("B"));
    assert!(h.commit(&mut w));
    h.undo(&mut w);
    assert!(w.get::<Name>(b).is_none());

    h.undo(&mut w);
    assert_eq!(text(&mut w), start);
    h.redo(&mut w).unwrap();
    assert_eq!(text(&mut w), renamed);
    // The deletion's redo went with the new step; naming `b` is redone.
    h.redo(&mut w).unwrap();
    assert_eq!(w.get::<Name>(b).map(Name::as_str), Some("B"));
    assert_eq!(h.skipped_non_pod(), 0, "everything here is recorded");

    // Drags of a value that is not plain data merge like any other.
    let c = spawn(&mut w, 2.0);
    w.insert(c, Name::new("C0"));
    for i in 1..=5 {
        h.begin("Type", Some(9));
        h.touch(&mut w, c);
        w.get_mut::<Name>(c).unwrap().0 = format!("C{i}");
        h.commit(&mut w);
    }
    let steps = h.steps().count();
    h.undo(&mut w);
    assert_eq!(w.get::<Name>(c).unwrap().as_str(), "C0");
    assert_eq!(h.steps().count(), steps - 1);
}

#[test]
fn drags_merge_and_no_ops_are_dropped() {
    let mut w = World::new();
    let mut h = History::new();
    let e = spawn(&mut w, 0.0);
    for i in 1..=10 {
        h.begin("Drag", Some(42));
        h.touch(&mut w, e);
        w.get_mut::<Transform>(e).unwrap().translation.x = i as f32;
        h.commit(&mut w);
    }
    assert_eq!(h.steps().count(), 1);
    h.begin("Nothing", None);
    h.touch(&mut w, e);
    assert!(!h.commit(&mut w));
    h.undo(&mut w);
    assert_eq!(w.get::<Transform>(e).unwrap().translation.x, 0.0);
    h.redo(&mut w);
    assert_eq!(w.get::<Transform>(e).unwrap().translation.x, 10.0);

    // Dragging back to the start merges into nothing.
    h.begin("Drag", Some(7));
    h.touch(&mut w, e);
    w.get_mut::<Transform>(e).unwrap().translation.x = 3.0;
    h.commit(&mut w);
    h.begin("Drag", Some(7));
    h.touch(&mut w, e);
    w.get_mut::<Transform>(e).unwrap().translation.x = 10.0;
    h.commit(&mut w);
    assert_eq!(h.steps().count(), 1, "the net-zero drag is gone");
}

#[test]
fn cancel_restores_and_budget_drops_oldest() {
    let mut w = World::new();
    let mut h = History::with_budget(2_000);
    let e = spawn(&mut w, 1.0);
    h.begin("Scrub", None);
    h.touch(&mut w, e);
    w.get_mut::<Transform>(e).unwrap().translation.x = 99.0;
    let extra = spawn(&mut w, 5.0);
    h.touch_new(&mut w, extra);
    h.cancel(&mut w);
    assert_eq!(w.get::<Transform>(e).unwrap().translation.x, 1.0);
    assert!(!w.contains(extra));
    for i in 0..100 {
        h.begin("Step", None);
        h.touch(&mut w, e);
        w.get_mut::<Transform>(e).unwrap().translation.z = i as f32;
        h.commit(&mut w);
    }
    assert!(h.bytes() <= 2_000, "{}", h.bytes());
    assert!(h.steps().count() < 100);
}

#[test]
fn random_walk_undoes_and_redoes_exactly() {
    for seed in [0x9e37_79b9_7f4a_7c15u64, 1, 0xdead_beef, 0x1234_5678_9abc] {
        random_walk(seed);
    }
}

fn random_walk(seed: u64) {
    let mut w = World::new();
    let mut h = History::new();
    let mut rng = seed;
    let mut next = move |n: usize| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng % n.max(1) as u64) as usize
    };
    let mut texts = vec![text(&mut w)];
    let mut alive: Vec<Entity> = Vec::new();
    for step in 0..300 {
        alive.retain(|&e| w.contains(e));
        h.begin("step", None);
        match next(8) {
            0 | 1 => {
                let e = spawn(&mut w, step as f32);
                h.touch_new(&mut w, e);
                if !alive.is_empty() && next(2) == 0 {
                    let p = alive[next(alive.len())];
                    let _ = w.set_parent(e, Some(p));
                }
                alive.push(e);
            }
            2 if !alive.is_empty() => {
                let e = alive[next(alive.len())];
                h.delete_subtree(&mut w, e);
            }
            3 if !alive.is_empty() => {
                let e = alive[next(alive.len())];
                h.touch(&mut w, e);
                w.get_mut::<Transform>(e).unwrap().translation.y += 1.5;
            }
            4 if alive.len() >= 2 => {
                let (a, b) = (alive[next(alive.len())], alive[next(alive.len())]);
                h.touch(&mut w, a);
                let index = next(4);
                let _ = w.set_parent_at(a, Some(b), index); // cycles are refused
            }
            5 if !alive.is_empty() => {
                let e = alive[next(alive.len())];
                h.touch(&mut w, e);
                w.insert(e, Name::new(format!("entity {step}")));
            }
            6 if alive.len() >= 2 => {
                let (a, b) = (alive[next(alive.len())], alive[next(alive.len())]);
                h.touch(&mut w, a);
                w.insert(
                    a,
                    Tag {
                        label: format!("tag {step}"),
                        friends: vec![b, a],
                        level: (step % 2 == 0).then_some(step as u32),
                    },
                );
            }
            _ if alive.len() >= 2 => {
                let (a, b) = (alive[next(alive.len())], alive[next(alive.len())]);
                h.touch(&mut w, a);
                w.insert(
                    a,
                    Follow {
                        target: b,
                        distance: step as f32,
                    },
                );
            }
            _ => {}
        }
        if h.commit(&mut w) {
            texts.push(text(&mut w));
        }
    }
    let steps = texts.len() - 1;
    assert!(steps > 200, "{steps}");
    for i in (0..steps).rev() {
        h.undo(&mut w).unwrap();
        assert_eq!(
            text(&mut w),
            texts[i],
            "seed {seed}: after undoing step {}",
            i + 1
        );
    }
    for (i, t) in texts.iter().enumerate().skip(1) {
        h.redo(&mut w).unwrap();
        assert_eq!(&text(&mut w), t, "seed {seed}: after redoing step {i}");
    }
}

#[test]
fn duplicates_copy_subtrees_with_their_inner_references() {
    let mut w = World::new();
    let mut h = History::new();
    let outside = spawn(&mut w, 9.0);
    let root = spawn(&mut w, 1.0);
    let kid = spawn(&mut w, 2.0);
    let after = spawn(&mut w, 3.0);
    let parent = spawn(&mut w, 0.0);
    for e in [root, after] {
        w.set_parent(e, Some(parent)).unwrap();
    }
    w.set_parent(kid, Some(root)).unwrap();
    w.insert(root, Name::new("Root"));
    // References into the subtree (plain data and scene lines) and out of it.
    w.insert(
        root,
        Follow {
            target: kid,
            distance: 2.0,
        },
    );
    w.insert(
        kid,
        Tag {
            label: String::from("kid"),
            friends: vec![root, outside],
            level: None,
        },
    );
    let before = text(&mut w);
    let head = h.head();

    h.begin("Duplicate", None);
    let copy = h.duplicate_subtree(&mut w, root).unwrap();
    assert!(h.commit(&mut w));
    assert_ne!(h.head(), head);
    assert_ne!(w.get::<SceneId>(copy), w.get::<SceneId>(root));
    assert_eq!(w.parent(copy), Some(parent));
    assert_eq!(
        w.children(parent).collect::<Vec<_>>(),
        [root, copy, after],
        "the copy follows the original"
    );
    let copy_kid = w.children(copy).next().unwrap();
    assert_eq!(w.get::<Follow>(copy).unwrap().target, copy_kid);
    assert_eq!(
        w.get::<Tag>(copy_kid).unwrap().friends,
        [copy, outside],
        "inner references move to the copies, outer ones stay"
    );
    assert_eq!(w.get::<Name>(copy).map(Name::as_str), Some("Root"));
    assert_eq!(
        w.get::<Follow>(root).unwrap().target,
        kid,
        "the original is untouched"
    );

    h.undo(&mut w);
    assert_eq!(text(&mut w), before);
    assert_eq!(h.head(), head, "back to the state of the save");
    h.redo(&mut w);
    assert!(text(&mut w).len() > before.len());
}

/// A component of the game's own, not saved by scenes (no reflection): Stop takes it off.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Velocity(f32);
engine_ecs::component!(Velocity);

#[test]
fn play_puts_the_world_back_and_forgets_its_steps() {
    let mut w = World::new();
    let mut h = History::new();
    // The scene: a root with two children, one following the other, one with a tag.
    let root = spawn(&mut w, 0.0);
    let a = spawn(&mut w, 1.0);
    let b = spawn(&mut w, 2.0);
    w.set_parent(a, Some(root)).unwrap();
    w.set_parent(b, Some(root)).unwrap();
    w.insert(
        a,
        Follow {
            target: b,
            distance: 2.0,
        },
    );
    w.insert(b, Name::new("B"));
    w.insert(
        b,
        Tag {
            label: String::from("crate"),
            friends: vec![a],
            level: Some(1),
        },
    );
    // A step before Play, and the editor's own entity (no scene id).
    h.begin("Move", None);
    h.touch(&mut w, root);
    w.get_mut::<Transform>(root).unwrap().translation.y = 5.0;
    h.commit(&mut w);
    let camera = w.spawn((Transform::from_xyz(9.0, 9.0, 9.0),));
    let before = text(&mut w);

    let mark = h.begin_play();
    let snapshot = h.snapshot_world(&mut w);
    assert_eq!(snapshot.scene_entities(), 3);
    // Playing: things move, a component of the game's own comes, b goes, a bullet and a
    // scene entity the game loads come, and an edit is made (an undoable step of the play
    // world); undo does not reach the step made before Play.
    w.get_mut::<Transform>(a).unwrap().translation.x = 40.0;
    w.insert(a, Velocity(3.0));
    w.get_mut::<Tag>(b).unwrap().label = String::from("broken");
    let kids: Vec<Entity> = w.children(b).collect();
    assert!(kids.is_empty());
    let _ = w.set_parent(b, None);
    w.despawn(b);
    let bullet = w.spawn((Transform::from_xyz(0.0, 1.0, 0.0), Velocity(50.0)));
    let loaded = spawn(&mut w, 7.0);
    w.set_parent(loaded, Some(root)).unwrap();
    h.begin("Rename", None);
    h.touch(&mut w, a);
    w.insert(a, Name::new("Renamed while playing"));
    h.commit(&mut w);
    assert!(h.undo(&mut w).is_some(), "the play world's own step");
    assert!(
        h.undo(&mut w).is_none() && !h.can_undo(),
        "not the step before Play"
    );

    // Stop: the scene as it was, the editor's entity kept, the play's steps gone.
    h.restore_world(&mut w, &snapshot);
    h.end_play(mark);
    assert_eq!(text(&mut w), before);
    assert!(!w.contains(bullet) && !w.contains(loaded));
    assert!(
        w.get::<Velocity>(a).is_none(),
        "the game's own component went"
    );
    assert_eq!(
        w.get::<Transform>(camera).unwrap().translation.x,
        9.0,
        "the editor's own entity stays"
    );
    // b came back, a follows it again, and the children are in their order.
    let kids: Vec<Entity> = w.children(root).collect();
    assert_eq!(kids.len(), 2);
    assert_eq!(kids[0], a);
    assert_eq!(w.get::<Follow>(a).unwrap().target, kids[1]);
    assert_eq!(w.get::<Tag>(kids[1]).unwrap().friends, vec![a]);
    // The step before Play undoes as before.
    assert!(h.can_undo());
    assert_eq!(h.undo(&mut w).as_deref(), Some("Move"));
    assert_eq!(w.get::<Transform>(root).unwrap().translation.y, 0.0);
}

/// What entering and leaving Play costs the world: 10,000 scene entities (a transform, a
/// name, a tag with a string and a list), half of them moved while playing and 1,000
/// spawned. Run with `--ignored --nocapture`.
#[test]
#[ignore = "a measurement"]
fn measure_play_snapshot_and_restore() {
    let mut w = World::new();
    let mut h = History::new();
    let all: Vec<Entity> = (0..10_000)
        .map(|i| {
            let e = spawn(&mut w, i as f32);
            w.insert(e, Name::new(format!("Prop {i}")));
            w.insert(
                e,
                Tag {
                    label: String::from("prop"),
                    friends: Vec::new(),
                    level: Some(i % 7),
                },
            );
            e
        })
        .collect();
    let mut best = [f64::MAX; 2];
    for _ in 0..5 {
        let t = std::time::Instant::now();
        let mark = h.begin_play();
        let snapshot = h.snapshot_world(&mut w);
        let enter = t.elapsed().as_secs_f64() * 1e3;
        for &e in all.iter().step_by(2) {
            w.get_mut::<Transform>(e).unwrap().translation.y += 1.0;
        }
        for i in 0..1000 {
            w.spawn((Transform::from_xyz(i as f32, 0.0, 0.0), Velocity(1.0)));
        }
        let t = std::time::Instant::now();
        h.restore_world(&mut w, &snapshot);
        h.end_play(mark);
        let leave = t.elapsed().as_secs_f64() * 1e3;
        best = [best[0].min(enter), best[1].min(leave)];
        assert_eq!(w.get::<Transform>(all[0]).unwrap().translation.y, 0.0);
        println!(
            "snapshot {enter:.1} ms ({} KB), restore {leave:.1} ms",
            snapshot.bytes() / 1024
        );
    }
    println!("best of 5: Play {:.1} ms, Stop {:.1} ms", best[0], best[1]);
}

#[test]
fn changes_kept_from_play_enter_the_scene_as_one_step() {
    use engine_editor::PlayChange;
    let mut w = World::new();
    let mut h = History::new();
    let root = spawn(&mut w, 0.0);
    let a = spawn(&mut w, 1.0);
    let b = spawn(&mut w, 2.0);
    w.set_parent(a, Some(root)).unwrap();
    w.insert(b, Name::new("B"));
    let b_id = *w.get::<SceneId>(b).unwrap();
    let before = text(&mut w);
    let mark = h.begin_play();
    let snapshot = h.snapshot_world(&mut w);
    // Playing: a moves and gets the game's own component, b goes, an entity is made under
    // the root.
    w.get_mut::<Transform>(a).unwrap().translation.x = 40.0;
    w.insert(a, Velocity(3.0));
    let _ = w.set_parent(b, None);
    w.despawn(b);
    let c = w.spawn((
        Transform::from_xyz(5.0, 0.0, 0.0),
        Name::new("Made"),
        Velocity(1.0),
    ));
    w.set_parent(c, Some(root)).unwrap();
    let changes = h.changes_since(&mut w, &snapshot);
    assert!(
        changes.contains(&PlayChange::Changed {
            entity: a,
            components: vec!["Transform"],
        }),
        "the game's own component is not the scene's: {changes:?}"
    );
    assert!(changes.contains(&PlayChange::Gone { id: b_id }));
    assert!(changes.contains(&PlayChange::Made { entity: c }));
    assert_eq!(changes.len(), 3, "{changes:?}");

    // Stop, keeping a's move, the made entity and b's going.
    let kept = h.capture_kept(&mut w, &[a, c], &[b_id]);
    assert_eq!(kept.len(), 3);
    h.restore_world(&mut w, &snapshot);
    h.end_play(mark);
    assert_eq!(text(&mut w), before);
    h.begin("Keep Changes", None);
    h.keep(&mut w, &kept);
    assert!(h.commit(&mut w));
    assert_eq!(w.get::<Transform>(a).unwrap().translation.x, 40.0);
    assert!(w.get::<Velocity>(a).is_none());
    assert!(engine_scene::find_named(&mut w, "B").is_none());
    let made = engine_scene::find_named(&mut w, "Made").unwrap();
    assert!(w.get::<SceneId>(made).is_some() && w.get::<Velocity>(made).is_none());
    assert_eq!(w.parent(made), Some(root));
    assert_eq!(w.get::<Transform>(made).unwrap().translation.x, 5.0);
    // One step takes it all back.
    assert_eq!(h.undo(&mut w).as_deref(), Some("Keep Changes"));
    assert_eq!(text(&mut w), before);
}

#[test]
fn records_follow_a_components_new_layout_by_name() {
    use engine_ecs::{ComponentDesc, Relayout};
    use engine_reflect::{Attrs, Field, TypeInfo, TypeKind};
    static OLD_FIELDS: [Field; 2] = [
        Field {
            name: "lift",
            offset: 0,
            ty: <f32 as Reflect>::TYPE_INFO,
            attrs: Attrs::NONE,
        },
        Field {
            name: "target",
            offset: 8,
            ty: <Entity as Reflect>::TYPE_INFO,
            attrs: Attrs::NONE,
        },
    ];
    static NEW_FIELDS: [Field; 3] = [
        Field {
            name: "lift",
            offset: 0,
            ty: <f32 as Reflect>::TYPE_INFO,
            attrs: Attrs::NONE,
        },
        Field {
            name: "speed",
            offset: 4,
            ty: <f32 as Reflect>::TYPE_INFO,
            attrs: Attrs::NONE,
        },
        Field {
            name: "target",
            offset: 8,
            ty: <Entity as Reflect>::TYPE_INFO,
            attrs: Attrs::NONE,
        },
    ];
    static OLD: TypeInfo = TypeInfo {
        name: "UndoDoor",
        size: 16,
        align: 8,
        kind: TypeKind::Struct(&OLD_FIELDS),
        attrs: Attrs::NONE,
    };
    static NEW: TypeInfo = TypeInfo {
        name: "UndoDoor",
        size: 16,
        align: 8,
        kind: TypeKind::Struct(&NEW_FIELDS),
        attrs: Attrs::NONE,
    };
    let desc = |name: &str, info: &'static TypeInfo| ComponentDesc {
        name: name.into(),
        layout: core::alloc::Layout::from_size_align(16, 8).unwrap(),
        drop: None,
        pod: true,
        type_info: Some(info),
    };
    let mut w = World::new();
    // SAFETY: plain data of the described layouts.
    let old = unsafe { w.register_dynamic(desc("script::UndoDoor", &OLD)) }.unwrap();
    // SAFETY: as above.
    let new = unsafe { w.register_dynamic(desc("script::UndoDoor#2", &NEW)) }.unwrap();
    let value = |lift: f32, target: Entity| {
        let mut v = [0u8; 16];
        v[0..4].copy_from_slice(&lift.to_le_bytes());
        v[8..16].copy_from_slice(&target.to_bits().to_le_bytes());
        v
    };
    let friend = spawn(&mut w, 9.0);
    let door = spawn(&mut w, 0.0);
    // SAFETY: a value of the old layout, plain data.
    unsafe { w.insert_raw(door, old, value(1.0, friend).as_ptr()) };
    let mut history = History::new();
    history.begin("Raise", None);
    history.touch(&mut w, door);
    // SAFETY: as above.
    unsafe { w.insert_raw(door, old, value(3.0, friend).as_ptr()) };
    history.commit(&mut w);
    // The script's props changed: the world moves to the new layout, the history follows.
    let mut defaults = vec![0u8; 16];
    defaults[4..8].copy_from_slice(&2.0f32.to_le_bytes());
    defaults[8..16].copy_from_slice(&Entity::NULL.to_bits().to_le_bytes());
    let r = Relayout::between(old, &OLD, new, &NEW, defaults.into(), &[]);
    let now = r.apply(&value(3.0, friend));
    // SAFETY: a value of the new layout.
    unsafe { w.insert_raw(door, new, now.as_ptr()) };
    w.remove_raw(door, old);
    history.relayout(&r);
    // Undo puts back the value before the step, in the new layout, its reference resolved.
    history.undo(&mut w);
    assert!(
        w.get_raw(door, old).is_none(),
        "nothing of the old layout comes back"
    );
    let bytes = |w: &World| {
        let p = w.get_raw(door, new).unwrap().as_ptr();
        // SAFETY: the new layout's 16 bytes.
        unsafe { core::slice::from_raw_parts(p, 16) }.to_vec()
    };
    let undone = bytes(&w);
    assert_eq!(undone[0..4], 1.0f32.to_le_bytes(), "lift as it was");
    assert_eq!(undone[4..8], 2.0f32.to_le_bytes(), "speed: its default");
    assert_eq!(
        undone[8..16],
        friend.to_bits().to_le_bytes(),
        "the reference kept"
    );
    history.redo(&mut w);
    assert_eq!(bytes(&w)[0..4], 3.0f32.to_le_bytes());
}

/// A file's text is a step too (a material edited in the inspector, ADR 0120's E10-1c): a
/// drag's steps merge into one, undo and redo hand the texts back, a cancel puts the first
/// back, and a step that changes nothing is none.
#[test]
fn a_files_text_is_a_step_too() {
    let mut w = World::new();
    let mut h = History::new();
    let file = engine_scene::AssetId::new();
    let e = spawn(&mut w, 0.0);
    // A drag: three moves of one gesture, the file and an entity in the same step.
    for (k, text) in ["roughness = 0.6", "roughness = 0.7", "roughness = 0.8"]
        .into_iter()
        .enumerate()
    {
        h.begin("Edit roughness", Some(1));
        let before = if k == 0 {
            "roughness = 0.5"
        } else {
            "roughness = 0.0"
        };
        h.change_file(file, before, text);
        h.touch(&mut w, e);
        w.get_mut::<Transform>(e).unwrap().translation.x = k as f32 + 1.0;
        assert!(h.commit(&mut w));
    }
    assert_eq!(h.steps().count(), 1, "one gesture, one step");
    assert!(h.take_files().is_empty(), "nothing put back yet");
    assert_eq!(h.undo(&mut w).as_deref(), Some("Edit roughness"));
    let back = h.take_files();
    assert_eq!(back.len(), 1);
    assert_eq!(
        (back[0].0, &*back[0].1),
        (file, "roughness = 0.5"),
        "the first before"
    );
    assert_eq!(w.get::<Transform>(e).unwrap().translation.x, 0.0);
    assert!(h.redo(&mut w).is_some());
    let forward = h.take_files();
    assert_eq!(&*forward[0].1, "roughness = 0.8", "the last after");
    // Set back to what it was: no step.
    h.begin("Edit roughness", None);
    h.change_file(file, "roughness = 0.8", "roughness = 0.8");
    assert!(!h.commit(&mut w));
    // A cancel puts the text back.
    h.begin("Edit metallic", None);
    h.change_file(file, "metallic = 0", "metallic = 1");
    h.cancel(&mut w);
    assert_eq!(&*h.take_files()[0].1, "metallic = 0");
    assert_eq!(h.steps().count(), 1);
}
