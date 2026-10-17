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
