//! Undo and redo: edits, spawns, despawned subtrees, reparenting, sibling order, entity
//! references, merging, cancel, budget, and a randomized walk checked against scene text.

#![allow(clippy::unwrap_used)]

use engine_ecs::{Entity, Transform, World, reflect_component};
use engine_editor::History;
use engine_reflect::Reflect;
use engine_scene::{SceneId, SceneRegistry, save};

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

fn registry() -> SceneRegistry {
    let mut r = SceneRegistry::with_engine_types();
    r.register::<Follow>();
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
        match next(6) {
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
