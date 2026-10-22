#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::*;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Pos(f32, f32);
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Vel(f32, f32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Id(u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Marker;
pod_component!(Pos, Vel, Id, Marker);

/// Counts live instances through the `Arc`'s strong count.
struct Tracked(#[allow(dead_code)] Arc<()>);
component!(Tracked);

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Big([u8; 20_000]);
pod_component!(Big);

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C, align(64))]
struct Aligned(u64);
component!(Aligned);

#[test]
fn resources_are_listed_and_dropped_by_type() {
    struct Score;
    struct Level;
    let mut w = World::new();
    w.insert_resource(Score);
    w.insert_resource(Level);
    let _ = w.remove_resource::<Level>();
    // Registered once, now empty: not listed.
    let types: Vec<core::any::TypeId> = w.resource_types().collect();
    assert_eq!(types, [core::any::TypeId::of::<Score>()]);
    assert!(w.remove_resource_of(core::any::TypeId::of::<Score>()));
    assert!(!w.remove_resource_of(core::any::TypeId::of::<Score>()));
    assert!(w.resource::<Score>().is_none() && w.resource_types().next().is_none());
}

#[test]
fn spawn_get_insert_remove_despawn() {
    let mut w = World::new();
    let es: Vec<Entity> = (0..1000)
        .map(|i| w.spawn((Id(i), Pos(i as f32, 0.0))))
        .collect();
    assert_eq!(w.len(), 1000);
    for (i, &e) in es.iter().enumerate() {
        assert_eq!(w.get::<Id>(e), Some(&Id(i as u32)));
        assert_eq!(w.get::<Pos>(e), Some(&Pos(i as f32, 0.0)));
        assert!(w.get::<Vel>(e).is_none());
    }
    let rc = Arc::new(());
    for &e in es.iter().step_by(3) {
        assert!(w.insert(e, Tracked(rc.clone())));
        assert!(w.insert(e, Vel(1.0, 2.0)));
    }
    assert_eq!(Arc::strong_count(&rc), 1 + 334);
    for &e in es.iter().step_by(6) {
        assert!(w.remove::<Tracked>(e).is_some());
    }
    assert_eq!(Arc::strong_count(&rc), 1 + 167);
    for (i, &e) in es.iter().enumerate() {
        assert_eq!(w.get::<Id>(e), Some(&Id(i as u32)), "{i}");
        assert_eq!(w.has::<Vel>(e), i % 3 == 0);
        assert_eq!(w.has::<Tracked>(e), i % 3 == 0 && i % 6 != 0);
    }
    // Replacing a component drops the old value.
    let replaced = es[3];
    assert!(w.insert(replaced, Tracked(rc.clone())));
    assert_eq!(Arc::strong_count(&rc), 1 + 167);
    for &e in &es[..500] {
        assert!(w.despawn(e));
        assert!(!w.despawn(e));
        assert!(!w.contains(e));
        assert!(w.get::<Id>(e).is_none());
        assert!(!w.insert(e, Id(0)));
    }
    assert_eq!(w.len(), 500);
    for (i, &e) in es.iter().enumerate().skip(500) {
        assert_eq!(w.get::<Id>(e), Some(&Id(i as u32)));
    }
    // Reused slots get a new generation.
    let reused = w.spawn((Id(7),));
    assert_eq!(reused.index(), es[499].index());
    assert_ne!(reused.generation(), es[499].generation());
    drop(w);
    assert_eq!(Arc::strong_count(&rc), 1);
}

#[test]
fn queries_visit_every_match_once() {
    let mut w = World::new();
    for i in 0..5000u32 {
        match i % 4 {
            0 => w.spawn((Id(i), Pos(i as f32, 0.0))),
            1 => w.spawn((Id(i), Pos(i as f32, 0.0), Vel(1.0, 0.0))),
            2 => w.spawn((Id(i), Vel(0.0, 1.0))),
            _ => w.spawn((Id(i), Pos(i as f32, 0.0), Marker)),
        };
    }
    let mut q = QueryState::<(&mut Pos, &Vel)>::new(&mut w);
    q.for_each(&mut w, |(p, v)| p.0 += v.0);
    let mut sum = 0u64;
    let mut ids = QueryState::<(&Id, &Pos)>::new(&mut w);
    ids.for_each(&mut w, |(id, p)| {
        let expected = id.0 as f32 + if id.0 % 4 == 1 { 1.0 } else { 0.0 };
        assert_eq!(p.0, expected);
        sum += u64::from(id.0);
    });
    let expected: u64 = (0..5000u64).filter(|i| i % 4 != 2).sum();
    assert_eq!(sum, expected);

    let mut count = 0;
    QueryState::<&Id, With<Marker>>::new(&mut w).for_each(&mut w, |_| count += 1);
    assert_eq!(count, 1250);
    count = 0;
    QueryState::<&Id, (Without<Pos>, With<Vel>)>::new(&mut w).for_each(&mut w, |_| count += 1);
    assert_eq!(count, 1250);
    count = 0;
    QueryState::<(Entity, Option<&Vel>)>::new(&mut w).for_each(&mut w, |(e, v)| {
        assert_eq!(v.is_some(), matches!(w_id_mod(e), 1 | 2));
        count += 1;
    });
    assert_eq!(count, 5000);

    // Chunk iteration gives equal-length arrays.
    let mut rows = 0;
    QueryState::<(&Id, &Pos)>::new(&mut w).for_each_chunk(&mut w, |(ids, ps)| {
        assert_eq!(ids.len(), ps.len());
        rows += ids.len();
    });
    assert_eq!(rows, 3750);

    // A writing iterator (marks chunks while items are alive), then a filtered one.
    let mut moving = QueryState::<(&mut Pos, &Vel)>::new(&mut w);
    let mut n = 0;
    for (p, v) in moving.iter(&mut w) {
        p.1 += v.1;
        n += 1;
    }
    assert_eq!(n, 1250);
    let mut changed = QueryState::<&mut Pos, Changed<Pos>>::new(&mut w);
    assert_eq!(changed.iter(&mut w).count(), 3750);
    assert_eq!(
        changed.iter(&mut w).count(),
        0,
        "a query's own writes are not news to it"
    );

    // Iterator and read-only paths agree with `for_each`.
    let mut via_iter: Vec<u32> = ids.iter(&mut w).map(|(id, _)| id.0).collect();
    let mut via_readonly = Vec::new();
    ids.for_each_readonly(&w, |(id, _)| via_readonly.push(id.0));
    assert_eq!(via_iter, via_readonly);
    via_iter.sort_unstable();
    assert_eq!(via_iter.len(), 3750);
}

/// Entities were spawned with `Id(i)` in index order in `queries_visit_every_match_once`.
fn w_id_mod(e: Entity) -> u32 {
    e.index() % 4
}

#[test]
fn get_through_query() {
    let mut w = World::new();
    let a = w.spawn((Pos(1.0, 2.0), Vel(3.0, 4.0)));
    let b = w.spawn((Pos(5.0, 6.0),));
    let mut q = QueryState::<(&mut Pos, &Vel)>::new(&mut w);
    let (p, v) = q.get(&mut w, a).unwrap();
    p.0 += v.0;
    assert!(q.get(&mut w, b).is_none());
    assert_eq!(w.get::<Pos>(a), Some(&Pos(4.0, 2.0)));
    let mut r = QueryState::<&Pos>::new(&mut w);
    assert_eq!(r.get_readonly(&w, b), Some(&Pos(5.0, 6.0)));
    w.despawn(b);
    assert!(r.get_readonly(&w, b).is_none());
}

#[test]
#[should_panic(expected = "mutably more than once")]
fn aliasing_queries_are_rejected() {
    let mut w = World::new();
    QueryState::<(&mut Pos, &Pos)>::new(&mut w);
}

#[test]
#[should_panic(expected = "another world")]
fn query_state_is_bound_to_its_world() {
    let mut a = World::new();
    let mut b = World::new();
    let mut q = QueryState::<&Pos>::new(&mut a);
    q.for_each(&mut b, |_| {});
}

#[test]
fn changed_tracks_writes_per_chunk() {
    let mut w = World::new();
    let es: Vec<Entity> = (0..2000u32)
        .map(|i| w.spawn((Id(i), Pos(0.0, 0.0))))
        .collect();
    let capacity = w.archetypes()[w.archetypes().len() - 1].chunk_capacity(0);
    let mut changed = QueryState::<&Id, Changed<Pos>>::new(&mut w);
    let mut n = 0;
    changed.for_each(&mut w, |_| n += 1);
    assert_eq!(n, 2000, "spawns count as changes");
    n = 0;
    changed.for_each(&mut w, |_| n += 1);
    assert_eq!(n, 0);
    // One write marks its whole chunk.
    w.get_mut::<Pos>(es[0]).unwrap().0 = 1.0;
    changed.for_each(&mut w, |_| n += 1);
    assert_eq!(n, capacity);
    // Reading does not count; writing another component does not count.
    n = 0;
    QueryState::<&Pos>::new(&mut w).for_each(&mut w, |_| {});
    QueryState::<&mut Id>::new(&mut w).for_each(&mut w, |_| {});
    changed.for_each(&mut w, |_| n += 1);
    assert_eq!(n, 0);
    // A mutable query marks every chunk it visits.
    QueryState::<&mut Pos>::new(&mut w).for_each(&mut w, |_| {});
    changed.for_each(&mut w, |_| n += 1);
    assert_eq!(n, 2000);
}

#[test]
fn commands_apply_in_order() {
    let mut w = World::new();
    let existing = w.spawn((Id(1),));
    let rc = Arc::new(());
    let mut c = Commands::new();
    let a = c.spawn((Id(2), Tracked(rc.clone())));
    c.insert(a, Pos(1.0, 1.0));
    c.insert(existing, Vel(0.0, 1.0));
    c.remove::<Tracked>(a);
    let b = c.spawn((Id(3),));
    c.despawn(b);
    assert_eq!(c.len(), 6);
    assert_eq!(Arc::strong_count(&rc), 2);
    w.apply(&mut c);
    assert!(c.is_empty());
    assert_eq!(Arc::strong_count(&rc), 1);
    assert_eq!(w.len(), 2);
    assert_eq!(w.get::<Vel>(existing), Some(&Vel(0.0, 1.0)));
    let mut found = Vec::new();
    QueryState::<(&Id, Option<&Pos>)>::new(&mut w)
        .for_each(&mut w, |(id, p)| found.push((id.0, p.copied())));
    found.sort_by_key(|f| f.0);
    assert_eq!(found, vec![(1, None), (2, Some(Pos(1.0, 1.0)))]);

    // Dropping an unapplied buffer drops its values.
    let mut c = Commands::new();
    c.spawn((Tracked(rc.clone()),));
    c.insert(existing, Tracked(rc.clone()));
    assert_eq!(Arc::strong_count(&rc), 3);
    drop(c);
    assert_eq!(Arc::strong_count(&rc), 1);
}

#[test]
fn big_aligned_and_zero_sized_components() {
    let mut w = World::new();
    let e = w.spawn((Big([7; 20_000]), Aligned(5), Marker));
    let f = w.spawn((Big([9; 20_000]), Aligned(6), Marker));
    assert_eq!(w.get::<Big>(e).unwrap().0[19_999], 7);
    assert_eq!(w.get::<Big>(f).unwrap().0[0], 9);
    let aligned = w.get::<Aligned>(f).unwrap();
    assert_eq!(aligned, &Aligned(6));
    assert_eq!(core::ptr::from_ref(aligned) as usize % 64, 0);
    assert!(w.get::<Marker>(e).is_some());
    w.despawn(e);
    assert_eq!(w.get::<Big>(f).unwrap().0[0], 9);
    assert_eq!(w.get::<Aligned>(f), Some(&Aligned(6)));
}

#[test]
fn dynamic_components() {
    let mut w = World::new();
    // SAFETY: a plain 12-byte value of three f32s.
    let id = unsafe {
        w.register_dynamic(ComponentDesc {
            name: "script::Color".into(),
            layout: core::alloc::Layout::new::<[f32; 3]>(),
            drop: None,
            pod: true,
            type_info: None,
        })
    }
    .unwrap();
    // SAFETY: as above.
    let duplicate = unsafe {
        w.register_dynamic(ComponentDesc {
            name: "script::Color".into(),
            layout: core::alloc::Layout::new::<u8>(),
            drop: None,
            pod: true,
            type_info: None,
        })
    };
    assert!(matches!(duplicate, Err(RegisterError::DuplicateName(_))));
    let e = w.spawn((Id(1),));
    let color = [0.25f32, 0.5, 1.0];
    // SAFETY: `color` is a valid value of the component's layout; it is `Copy`.
    assert!(unsafe { w.insert_raw(e, id, color.as_ptr().cast()) });
    let ptr = w.get_raw(e, id).unwrap();
    // SAFETY: the component's layout.
    assert_eq!(unsafe { ptr.cast::<[f32; 3]>().read() }, color);
    assert_eq!(w.get::<Id>(e), Some(&Id(1)));
    assert_eq!(w.components().name(id), Some("script::Color"));
    assert!(w.remove_raw(e, id));
    assert!(w.get_raw(e, id).is_none());
}

fn churn(w: &mut World, seed: u64, steps: usize) {
    // Miri runs the same paths, just fewer times.
    let steps = if cfg!(miri) { steps / 20 } else { steps };
    let mut rng = seed;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut live: Vec<Entity> = Vec::new();
    let mut movement = QueryState::<(&mut Pos, &Vel)>::new(w);
    for step in 0..steps {
        match next() % 6 {
            0 | 1 => live.push(w.spawn((Id(step as u32), Pos(0.0, step as f32)))),
            2 if !live.is_empty() => {
                let e = live.swap_remove((next() % live.len() as u64) as usize);
                w.despawn(e);
            }
            3 if !live.is_empty() => {
                let e = live[(next() % live.len() as u64) as usize];
                w.insert(e, Vel(1.0, (step % 7) as f32));
            }
            4 if !live.is_empty() => {
                let e = live[(next() % live.len() as u64) as usize];
                w.remove::<Vel>(e);
            }
            _ => {
                movement.for_each(w, |(p, v)| {
                    p.0 += v.0 * 0.5;
                    p.1 = p.1 * 0.99 + v.1;
                });
                w.advance_sim_tick();
            }
        }
    }
}

#[test]
fn identical_calls_give_identical_state() {
    let mut a = World::new();
    let mut b = World::new();
    churn(&mut a, 0x1234_5678, 20_000);
    churn(&mut b, 0x1234_5678, 20_000);
    assert_eq!(a.state_hash(), b.state_hash());
    let mut c = World::new();
    churn(&mut c, 0x1234_5679, 20_000);
    assert_ne!(a.state_hash(), c.state_hash());
}

#[test]
fn iteration_order_ignores_archetype_creation_history() {
    // `b` creates the archetypes in the opposite order, then ends in the same state.
    let mut a = World::new();
    let mut b = World::new();
    let tmp = b.spawn((Vel(0.0, 0.0), Marker));
    b.spawn((Pos(0.0, 0.0), Vel(0.0, 0.0), Marker));
    b.clear();
    let _ = tmp;
    for w in [&mut a, &mut b] {
        w.spawn((Pos(1.0, 0.0), Vel(0.0, 0.0), Marker));
        w.spawn((Vel(2.0, 0.0), Marker));
        w.spawn((Pos(3.0, 0.0),));
    }
    let order = |w: &mut World| {
        let mut out = Vec::new();
        QueryState::<&Vel>::new(w).for_each(w, |v| out.push(v.0));
        out
    };
    assert_eq!(order(&mut a), order(&mut b));
}

#[test]
fn snapshot_restore_resimulates_identically() {
    let mut w = World::new();
    churn(&mut w, 99, 5_000);
    let mut snap = Snapshot::new();
    w.save(&mut snap).unwrap();
    let saved_hash = w.state_hash();
    let saved_len = w.len();
    churn(&mut w, 7, 5_000);
    let first = w.state_hash();
    assert_ne!(first, saved_hash);

    w.restore(&snap).unwrap();
    assert_eq!(w.state_hash(), saved_hash);
    assert_eq!(w.len(), saved_len);
    churn(&mut w, 7, 5_000);
    assert_eq!(
        w.state_hash(),
        first,
        "re-simulation after rollback diverged"
    );

    // Restoring twice, and into archetypes created after the snapshot.
    w.spawn((Marker, Id(0), Vel(0.0, 0.0), Pos(0.0, 0.0)));
    w.restore(&snap).unwrap();
    assert_eq!(w.state_hash(), saved_hash);

    // Snapshots belong to their world.
    let mut other = World::new();
    assert_eq!(other.restore(&snap), Err(SnapshotError::WrongWorld));
    assert_eq!(w.restore(&Snapshot::new()), Err(SnapshotError::WrongWorld));
}

#[test]
fn snapshot_rejects_non_plain_data() {
    let mut w = World::new();
    let e = w.spawn((Id(1), Tracked(Arc::new(()))));
    let mut snap = Snapshot::new();
    assert!(matches!(
        w.save(&mut snap),
        Err(SnapshotError::NotPlainData { .. })
    ));
    w.remove::<Tracked>(e);
    w.save(&mut snap).unwrap();
    let rc = Arc::new(());
    w.insert(e, Tracked(rc.clone()));
    w.restore(&snap).unwrap();
    assert_eq!(
        Arc::strong_count(&rc),
        1,
        "restore drops values of newer rows"
    );
    assert!(!w.has::<Tracked>(e));
    assert_eq!(w.get::<Id>(e), Some(&Id(1)));
}

#[test]
fn parallel_iteration_matches_sequential() {
    let pool = engine_core::jobs::ThreadPool::builder()
        .workers(3)
        .build()
        .unwrap();
    let mut a = World::new();
    churn(&mut a, 5, 30_000);
    // Enough rows for several chunks even when Miri shortens the churn.
    a.spawn_batch((0..1_500u32).map(|i| (Id(i), Pos(i as f32, 1.0), Vel(0.5, 0.25))));
    let mut snap = Snapshot::new();
    a.save(&mut snap).unwrap();
    let step = |p: &mut Pos, v: &Vel| {
        p.0 = p.0 * 0.5 + v.0;
        p.1 += v.1.sin();
    };
    let mut q = QueryState::<(&mut Pos, &Vel)>::new(&mut a);
    q.for_each(&mut a, |(p, v)| step(p, v));
    let sequential = a.state_hash();
    a.restore(&snap).unwrap();
    q.par_for_each(&mut a, &pool, |(p, v)| step(p, v));
    assert_eq!(a.state_hash(), sequential);

    // Per-chunk partial sums combined in chunk order are deterministic.
    let chunks = q.entity_count(&a).div_ceil(1).max(1);
    let partial: Vec<std::sync::atomic::AtomicU64> = (0..chunks)
        .map(|_| std::sync::atomic::AtomicU64::new(0))
        .collect();
    q.par_for_each_chunk(&mut a, &pool, |position, (ps, _)| {
        partial[position].store(ps.len() as u64, std::sync::atomic::Ordering::Relaxed);
    });
    let visited: u64 = partial
        .iter()
        .map(|p| p.load(std::sync::atomic::Ordering::Relaxed))
        .sum();
    assert_eq!(visited as usize, q.entity_count(&a));
    assert!(q.last_parallel_chunks() > 1);
}

#[test]
fn many_chunks_across_size_classes() {
    let n: u32 = if cfg!(miri) { 3_000 } else { 60_000 };
    let mut w = World::new();
    let es: Vec<Entity> = (0..n)
        .map(|i| w.spawn((Id(i), Pos(i as f32, 0.0))))
        .collect();
    let arch = w
        .archetypes()
        .iter()
        .find(|a| a.len() == n as usize)
        .unwrap();
    let capacities: Vec<usize> = (0..arch.chunk_count())
        .map(|c| arch.chunk_capacity(c))
        .collect();
    assert!(capacities[0] < capacities[1] && (cfg!(miri) || capacities[1] < capacities[3]));
    let mut snap = Snapshot::new();
    w.save(&mut snap).unwrap();
    let hash = w.state_hash();
    // Despawn two thirds in a scattered order; the rest must be intact.
    for (k, &e) in es.iter().enumerate() {
        if k % 3 != 0 {
            assert!(w.despawn(e));
        }
    }
    for (k, &e) in es.iter().enumerate() {
        assert_eq!(
            w.get::<Id>(e).map(|id| id.0),
            (k % 3 == 0).then_some(k as u32)
        );
    }
    let mut sum = 0u64;
    QueryState::<&Id>::new(&mut w).for_each(&mut w, |id| sum += u64::from(id.0));
    assert_eq!(sum, (0..u64::from(n)).filter(|k| k % 3 == 0).sum());
    w.restore(&snap).unwrap();
    assert_eq!(w.state_hash(), hash);
    for (k, &e) in es.iter().enumerate() {
        assert_eq!(w.get::<Pos>(e), Some(&Pos(k as f32, 0.0)));
    }
}

#[test]
fn hierarchy_links_are_plain_data_and_ordered() {
    let mut w = World::new();
    let root = w.spawn((Id(0),));
    let kids: Vec<Entity> = (1..=4).map(|i| w.spawn((Id(i),))).collect();
    for &k in &kids {
        w.set_parent(k, Some(root)).unwrap();
    }
    assert_eq!(w.children(root).collect::<Vec<_>>(), kids);
    assert_eq!(w.parent(kids[2]), Some(root));
    // Detach from the middle, the ends, and re-attach at the end.
    w.set_parent(kids[1], None).unwrap();
    assert_eq!(
        w.children(root).collect::<Vec<_>>(),
        vec![kids[0], kids[2], kids[3]]
    );
    w.set_parent(kids[0], None).unwrap();
    w.set_parent(kids[3], None).unwrap();
    assert_eq!(w.children(root).collect::<Vec<_>>(), vec![kids[2]]);
    w.set_parent(kids[0], Some(root)).unwrap();
    assert_eq!(w.children(root).collect::<Vec<_>>(), vec![kids[2], kids[0]]);
    // Cycles are rejected; grandchildren work.
    w.set_parent(kids[3], Some(kids[0])).unwrap();
    assert_eq!(
        w.set_parent(root, Some(kids[3])),
        Err(HierarchyError::Cycle)
    );
    assert_eq!(w.set_parent(root, Some(root)), Err(HierarchyError::Cycle));
    // Snapshots carry the links.
    let mut snap = Snapshot::new();
    w.save(&mut snap).unwrap();
    let hash = w.state_hash();
    w.set_parent(kids[2], None).unwrap();
    w.restore(&snap).unwrap();
    assert_eq!(w.state_hash(), hash);
    assert_eq!(w.children(root).collect::<Vec<_>>(), vec![kids[2], kids[0]]);
    // Recursive despawn removes the subtree only.
    assert_eq!(w.despawn_recursive(kids[0]), 2);
    assert!(!w.contains(kids[3]));
    assert_eq!(w.children(root).collect::<Vec<_>>(), vec![kids[2]]);
    assert_eq!(w.get::<Children>(root).map(|c| c.len()), Some(1));
    w.set_parent(kids[2], None).unwrap();
    assert!(w.get::<Children>(root).is_none());
    assert_eq!(w.len(), 3);
}

#[test]
fn reflected_components_expose_their_fields() {
    use crate::reflect::{Reflect, Value, read};

    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
    #[repr(C)]
    struct Stamina {
        #[reflect(max = 10.0)]
        current: f32,
        regen: f32,
    }
    reflect_component!(pod Stamina);

    let mut w = World::new();
    let e = w.spawn((
        Stamina {
            current: 4.0,
            regen: 0.5,
        },
        Id(3),
    ));
    let id = w.component_id::<Stamina>().unwrap();
    let info = w.components().type_info(id).unwrap();
    assert_eq!(info.name, "Stamina");
    assert!(
        w.components()
            .type_info(w.component_id::<Id>().unwrap())
            .is_none()
    );
    // The editor's path: component pointer + field offset.
    let (offset, ty, attrs) = info.path("regen").unwrap();
    let base = w.get_raw(e, id).unwrap();
    // SAFETY: `base` points to a live `Stamina`; the path resolved inside its description.
    let v = unsafe { read(base.as_ptr().add(offset), ty) };
    assert_eq!(v, Some(Value::Float(0.5)));
    assert_eq!(attrs.max, None);
    assert_eq!(info.field("current").unwrap().attrs.max, Some(10.0));
    // Hierarchy components are described too (links hidden from the inspector).
    let parent = w
        .component_id::<Parent>()
        .unwrap_or_else(|| w.register::<Parent>());
    let pinfo = w.components().type_info(parent).unwrap();
    assert!(pinfo.field("prev").unwrap().attrs.hidden);
}

#[test]
fn large_chunks_stagger_columns_within_a_page() {
    let mut w = World::new();
    // Enough rows to reach every size class, including 1 MiB chunks.
    for i in 0..200_000u32 {
        w.spawn((Pos(i as f32, 0.0), Vel(1.0, 0.0), Id(i)));
    }
    let archetype = w
        .archetypes
        .iter()
        .find(|a| a.len() == 200_000)
        .expect("one archetype");
    assert!(archetype.chunk_count() > 7, "reaches the 1 MiB class");
    for chunk in 0..archetype.chunk_count() {
        let starts: Vec<usize> = (0..archetype.columns.len())
            .map(|c| archetype.column_ptr(chunk, c) as usize)
            .collect();
        for (c, &s) in starts.iter().enumerate() {
            assert_eq!(s % 16, 0, "column {c} aligned");
        }
        if chunk > 0 {
            // Staggered classes: no two columns share a start offset within a page.
            let mut in_page: Vec<usize> = starts.iter().map(|s| s % 4096).collect();
            in_page.sort_unstable();
            in_page.dedup();
            assert_eq!(in_page.len(), starts.len(), "chunk {chunk}");
        }
    }
    let mut q = QueryState::<(&mut Pos, &Vel)>::new(&mut w);
    q.for_each(&mut w, |(p, v)| p.0 += v.0);
    let mut sum = 0.0f64;
    q.for_each(&mut w, |(p, _)| sum += f64::from(p.0));
    assert_eq!(
        sum,
        (0..200_000u32).map(|i| f64::from(i) + 1.0).sum::<f64>()
    );
}

mod schedule {
    use crate::*;
    use engine_core::jobs::ThreadPool;

    #[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
    #[repr(C)]
    struct P(f32);
    #[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
    #[repr(C)]
    struct V(f32);
    #[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
    #[repr(C)]
    struct Hp(f32);
    #[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
    #[repr(C)]
    struct Age(u32);
    pod_component!(P, V, Hp, Age);

    struct Dt(f32);
    #[derive(Default)]
    struct Score(u64);

    fn build() -> Schedule {
        let mut s = Schedule::new();
        s.add(system(
            "move",
            |w, a| (a.query::<(&mut P, &V), ()>(w), a.read::<Dt>(w)),
            |(q, dt), ctx| {
                let dt = ctx.res(dt).unwrap().0;
                ctx.for_each(q, |(p, v)| p.0 += v.0 * dt);
            },
        ));
        s.add(system(
            "decay",
            |w, a| a.query::<&mut Hp, ()>(w),
            |q, ctx| ctx.for_each(q, |h| h.0 *= 0.99),
        ));
        s.add(system(
            "age and spawn",
            |w, a| a.query::<(Entity, &mut Age), ()>(w),
            |q, ctx| {
                let mut spawn = 0;
                let mut dead = Vec::new();
                ctx.for_each(q, |(e, age)| {
                    age.0 += 1;
                    if age.0 % 7 == 0 {
                        spawn += 1;
                    }
                    if age.0 > 20 {
                        dead.push(e);
                    }
                });
                for _ in 0..spawn {
                    ctx.commands().spawn((P(0.0), V(1.0), Hp(10.0), Age(0)));
                }
                for e in dead {
                    ctx.commands().despawn(e);
                }
            },
        ));
        s.add(system(
            "score",
            |w, a| (a.query::<&P, ()>(w), a.write::<Score>(w)),
            |(q, score), ctx| {
                let mut sum = 0.0f64;
                ctx.for_each(q, |p| sum += f64::from(p.0));
                ctx.res_mut(score).unwrap().0 = sum.to_bits();
            },
        ));
        s
    }

    /// Entities in the test world (small under Miri, which interprets every access).
    const N: u32 = if cfg!(miri) { 100 } else { 2000 };

    fn world() -> World {
        let mut w = World::new();
        w.insert_resource(Dt(0.5));
        w.insert_resource(Score::default());
        for i in 0..N {
            w.spawn((P(i as f32), V(1.0 + (i % 3) as f32), Hp(100.0), Age(i % 13)));
        }
        w
    }

    #[test]
    fn levels_follow_conflicts_in_order() {
        let mut w = world();
        let mut s = build();
        s.initialize(&mut w);
        // move (writes P) conflicts with score (reads P); decay and aging are independent.
        assert_eq!(
            s.levels(),
            vec![vec!["move", "decay", "age and spawn"], vec!["score"]]
        );
        s.add_exclusive("sync", |_| {});
        s.add(system("after", |w, a| a.query::<&Hp, ()>(w), |_, _| {}));
        s.initialize(&mut w);
        let levels = s.levels();
        assert_eq!(levels[2], vec!["sync"]);
        assert_eq!(levels[3], vec!["after"]);
    }

    #[test]
    fn parallel_runs_equal_serial_runs() {
        let pool = ThreadPool::builder().workers(4).build().unwrap();
        let (ticks, repeats) = if cfg!(miri) { (3, 1) } else { (50, 5) };
        for _ in 0..repeats {
            let (mut a, mut b) = (world(), world());
            let (mut sa, mut sb) = (build(), build());
            for tick in 0..ticks {
                sa.run(&mut a, None);
                sb.run(&mut b, Some(&pool));
                assert_eq!(a.state_hash(), b.state_hash(), "tick {tick}");
                assert_eq!(
                    a.resource::<Score>().unwrap().0,
                    b.resource::<Score>().unwrap().0
                );
            }
            assert!(a.len() != N as usize, "commands spawned and despawned");
        }
    }

    #[test]
    #[should_panic(expected = "declared by another system")]
    fn a_query_is_bound_to_its_system() {
        use std::sync::{Arc, Mutex};
        let mut w = world();
        let stolen: Arc<Mutex<Option<SystemQuery<&P>>>> = Arc::default();
        let put = stolen.clone();
        let mut s = Schedule::new();
        s.add(system(
            "owner",
            move |w, a| {
                *put.lock().unwrap() = Some(a.query::<&P, ()>(w));
            },
            |_, _| {},
        ));
        let take = stolen.clone();
        s.add(system(
            "thief",
            |_, _| (),
            move |_, ctx| {
                if let Some(q) = take.lock().unwrap().as_mut() {
                    ctx.for_each(q, |_| {});
                }
            },
        ));
        s.run(&mut w, None);
    }
}

mod transforms {
    use super::{Marker, Pos, Vel};
    use crate::*;
    use engine_core::math::{Quat, Vec3};

    fn spawn(w: &mut World, t: Transform) -> Entity {
        w.spawn((t, GlobalTransform::IDENTITY))
    }

    fn pos(w: &World, e: Entity) -> Vec3 {
        w.get::<GlobalTransform>(e).unwrap().translation()
    }

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-5
    }

    #[test]
    fn world_transforms_follow_the_hierarchy() {
        let mut w = World::new();
        let mut p = TransformPropagation::new(&mut w);
        let root = spawn(
            &mut w,
            Transform::from_xyz(10.0, 0.0, 0.0)
                .with_rotation(Quat::from_rotation_y(core::f32::consts::FRAC_PI_2)),
        );
        let arm = spawn(
            &mut w,
            Transform::from_xyz(0.0, 0.0, -2.0).with_scale(Vec3::splat(2.0)),
        );
        let hand = spawn(&mut w, Transform::from_xyz(1.0, 0.0, 0.0));
        let loose = spawn(&mut w, Transform::from_xyz(0.0, 5.0, 0.0));
        w.set_parent(arm, Some(root)).unwrap();
        w.set_parent(hand, Some(arm)).unwrap();
        assert!(p.run(&mut w));
        // Rotating +90° about Y maps -Z to -X: the arm sits at (8, 0, 0).
        assert!(
            close(pos(&w, arm), Vec3::new(8.0, 0.0, 0.0)),
            "{}",
            pos(&w, arm)
        );
        // The hand: 1 along the arm's X (scaled by 2, rotated to -Z) → (8, 0, -2).
        assert!(
            close(pos(&w, hand), Vec3::new(8.0, 0.0, -2.0)),
            "{}",
            pos(&w, hand)
        );
        assert!(close(pos(&w, loose), Vec3::new(0.0, 5.0, 0.0)));

        // Nothing changed: the pass does no work.
        assert!(!p.run(&mut w));

        // Moving the root moves the whole subtree.
        w.get_mut::<Transform>(root).unwrap().translation.x = 0.0;
        assert!(p.run(&mut w));
        assert!(close(pos(&w, hand), Vec3::new(-2.0, 0.0, -2.0)));

        // Reparenting the hand to the loose entity.
        w.set_parent(hand, Some(loose)).unwrap();
        assert!(p.run(&mut w));
        assert!(close(pos(&w, hand), Vec3::new(1.0, 5.0, 0.0)));

        // Detached: the hand's local transform is its world transform.
        w.set_parent(hand, None).unwrap();
        assert!(p.run(&mut w));
        assert!(close(pos(&w, hand), Vec3::new(1.0, 0.0, 0.0)));
    }

    #[test]
    fn deep_chains_and_many_roots() {
        let mut w = World::new();
        let mut p = TransformPropagation::new(&mut w);
        let mut tips = Vec::new();
        for r in 0..50 {
            let mut parent = spawn(&mut w, Transform::from_xyz(r as f32, 0.0, 0.0));
            for _ in 0..40 {
                let child = spawn(&mut w, Transform::from_xyz(0.0, 1.0, 0.0));
                w.set_parent(child, Some(parent)).unwrap();
                parent = child;
            }
            tips.push(parent);
        }
        p.run(&mut w);
        for (r, &tip) in tips.iter().enumerate() {
            assert!(close(pos(&w, tip), Vec3::new(r as f32, 40.0, 0.0)));
        }
    }

    /// Small deterministic generator (xorshift64*).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() >> 33) as usize % n.max(1)
        }
        fn unit(&mut self) -> f32 {
            (self.next() >> 40) as f32 / (1u64 << 24) as f32
        }
        fn transform(&mut self) -> Transform {
            Transform::from_xyz(
                self.unit() * 8.0 - 4.0,
                self.unit() * 8.0 - 4.0,
                self.unit() * 8.0 - 4.0,
            )
            .with_rotation(Quat::from_euler(
                engine_core::math::EulerRot::XYZ,
                self.unit() * 6.0,
                self.unit() * 6.0,
                self.unit() * 6.0,
            ))
            .with_scale(Vec3::new(
                0.5 + self.unit(),
                0.5 + self.unit(),
                0.5 + self.unit(),
            ))
        }
    }

    /// One edit of a world; returns the entity it spawned, if any.
    fn apply(w: &mut World, op: usize, e: Entity, other: Entity, t: Transform) -> Option<Entity> {
        match op {
            0 => return Some(w.spawn((t, GlobalTransform::IDENTITY))),
            // A root with a world transform only: an input for its children.
            1 => return Some(w.spawn((GlobalTransform(t.to_affine()),))),
            _ if !w.contains(e) => {}
            2 | 3 => {
                let child = w.spawn((t, GlobalTransform::IDENTITY));
                let _ = w.set_parent(child, Some(e));
                return Some(child);
            }
            4..=6 => {
                if let Some(local) = w.get_mut::<Transform>(e) {
                    *local = t;
                }
            }
            7 => {
                let _ = w.set_parent(e, w.contains(other).then_some(other));
            }
            8 => {
                let _ = w.set_parent(e, None);
            }
            9 => {
                // Breaks the chain below `e`, or reconnects it.
                if w.remove::<Transform>(e).is_none() {
                    w.insert(e, t);
                }
            }
            10 => {
                // Leaves its children with a dead parent.
                w.despawn(e);
            }
            11 => {
                w.despawn_recursive(e);
            }
            12 => {
                // Moves it to another archetype.
                if w.remove::<Marker>(e).is_none() {
                    w.insert(e, Marker);
                }
            }
            _ => {
                // A world transform written by hand: kept on roots without a local
                // transform, overwritten everywhere a full pass reaches.
                if let Some(g) = w.get_mut::<GlobalTransform>(e) {
                    g.0 = t.to_affine();
                }
            }
        }
        None
    }

    #[test]
    fn incremental_propagation_equals_full_propagation() {
        for seed in 1..=40u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut a = World::new();
            let mut b = World::new();
            let mut incremental = TransformPropagation::new(&mut a);
            let mut full = TransformPropagation::new(&mut b);
            let mut entities: Vec<Entity> = Vec::new();
            for step in 0..60 {
                let edits = 1 + rng.below(if step == 0 { 200 } else { 12 });
                for _ in 0..edits {
                    let op = if entities.is_empty() {
                        0
                    } else if rng.below(10) == 0 {
                        rng.below(2)
                    } else {
                        2 + rng.below(13)
                    };
                    let e = entities.get(rng.below(entities.len())).copied();
                    let other = entities.get(rng.below(entities.len())).copied();
                    let (e, other) = (e.unwrap_or(Entity::NULL), other.unwrap_or(Entity::NULL));
                    let t = rng.transform();
                    let spawned = apply(&mut a, op, e, other, t);
                    assert_eq!(spawned, apply(&mut b, op, e, other, t));
                    entities.extend(spawned);
                }
                incremental.run(&mut a);
                full.run_full(&mut b);
                let stats = incremental.stats();
                assert!(stats.written <= stats.recomputed, "{stats:?}");
                for &e in &entities {
                    assert_eq!(a.contains(e), b.contains(e));
                    let (ga, gb) = (a.get::<GlobalTransform>(e), b.get::<GlobalTransform>(e));
                    assert_eq!(
                        ga.map(bytemuck::bytes_of),
                        gb.map(bytemuck::bytes_of),
                        "seed {seed}, step {step}, entity {e:?}"
                    );
                }
                // Everything else equal too, to the byte (determinism, ADR 0019).
                assert_eq!(a.state_hash(), b.state_hash(), "seed {seed}, step {step}");
            }
        }
    }

    #[test]
    fn parallel_propagation_equals_serial_propagation() {
        let pool = engine_core::jobs::ThreadPool::builder()
            .workers(4)
            .build()
            .unwrap();
        for seed in 1..=4u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut a = World::new();
            let mut b = World::new();
            let mut serial = TransformPropagation::new(&mut a);
            let mut parallel = TransformPropagation::new(&mut b);
            let mut entities: Vec<Entity> = Vec::new();
            let mut roots: Vec<Entity> = Vec::new();
            // Enough roots for the parallel path, with subtrees of any depth; then every
            // step moves every root and edits the world at random.
            for k in 0..9000 {
                let (op, parent) = if k < 3000 {
                    (0, Entity::NULL)
                } else {
                    (2, entities[rng.below(entities.len())])
                };
                let t = rng.transform();
                let spawned = apply(&mut a, op, parent, Entity::NULL, t);
                assert_eq!(spawned, apply(&mut b, op, parent, Entity::NULL, t));
                if k < 3000 {
                    roots.extend(spawned);
                }
                entities.extend(spawned);
            }
            for step in 0..12 {
                for &root in &roots {
                    let t = rng.transform();
                    for w in [&mut a, &mut b] {
                        if let Some(local) = w.get_mut::<Transform>(root) {
                            *local = t;
                        }
                    }
                }
                for _ in 0..20 {
                    let op = 2 + rng.below(13);
                    let e = entities[rng.below(entities.len())];
                    let other = entities[rng.below(entities.len())];
                    let t = rng.transform();
                    let spawned = apply(&mut a, op, e, other, t);
                    assert_eq!(spawned, apply(&mut b, op, e, other, t));
                    entities.extend(spawned);
                }
                serial.run(&mut a);
                parallel.run_parallel(&mut b, &pool);
                assert_eq!(serial.stats(), parallel.stats(), "seed {seed}, step {step}");
                for &e in &entities {
                    let (ga, gb) = (a.get::<GlobalTransform>(e), b.get::<GlobalTransform>(e));
                    assert_eq!(
                        ga.map(bytemuck::bytes_of),
                        gb.map(bytemuck::bytes_of),
                        "seed {seed}, step {step}, entity {e:?}"
                    );
                }
                assert_eq!(a.state_hash(), b.state_hash(), "seed {seed}, step {step}");
            }
        }
    }

    #[test]
    fn unchanged_world_transforms_are_not_marked() {
        let mut w = World::new();
        let mut p = TransformPropagation::new(&mut w);
        // Two archetypes: still props, and movers (with a marker).
        let still: Vec<Entity> = (0..1000)
            .map(|i| spawn(&mut w, Transform::from_xyz(i as f32, 0.0, 0.0)))
            .collect();
        let movers: Vec<Entity> = (0..10)
            .map(|i| {
                w.spawn((
                    Transform::from_xyz(0.0, i as f32, 0.0),
                    GlobalTransform::IDENTITY,
                    Marker,
                ))
            })
            .collect();
        let mut changed = QueryState::<Entity, Changed<GlobalTransform>>::new(&mut w);
        p.run(&mut w);
        assert_eq!(changed.iter(&mut w).count(), 1010);
        // Nothing moved: no chunk scanned, nothing marked.
        assert!(!p.run(&mut w));
        assert_eq!(p.stats().chunks, 0);
        assert_eq!(changed.iter(&mut w).count(), 0);
        // One mover moved: only its chunk is visited downstream.
        w.get_mut::<Transform>(movers[3]).unwrap().translation.z = 5.0;
        assert!(p.run(&mut w));
        assert_eq!(p.stats().written, 1);
        assert_eq!(changed.iter(&mut w).count(), 10);
        // A transform rewritten with its own value marks nothing downstream.
        let same = *w.get::<Transform>(still[7]).unwrap();
        *w.get_mut::<Transform>(still[7]).unwrap() = same;
        assert!(!p.run(&mut w));
        assert_eq!(p.stats().written, 0);
        assert_eq!(changed.iter(&mut w).count(), 0);
    }

    #[test]
    fn one_moved_root_recomputes_its_chunk_and_its_subtree() {
        let mut w = World::new();
        let mut p = TransformPropagation::new(&mut w);
        let mut roots = Vec::new();
        for r in 0..200 {
            let root = spawn(&mut w, Transform::from_xyz(r as f32, 0.0, 0.0));
            roots.push(root);
            for c in 0..10 {
                let child = spawn(&mut w, Transform::from_xyz(0.0, c as f32, 0.0));
                w.set_parent(child, Some(root)).unwrap();
            }
        }
        p.run(&mut w);
        w.get_mut::<Transform>(roots[150]).unwrap().translation.y = 3.0;
        assert!(p.run(&mut w));
        let stats = p.stats();
        // The root and its 10 children change; the other roots of its chunk and their
        // direct children are recomputed, nothing else.
        assert_eq!(stats.written, 11, "{stats:?}");
        assert!(stats.recomputed < 2200 / 2, "{stats:?}");
    }

    #[test]
    fn removals_are_logged_for_tracked_components() {
        let mut w = World::new();
        w.track_removals::<Marker>();
        let a = w.spawn((Pos(0.0, 0.0), Marker));
        let b = w.spawn((Pos(1.0, 0.0), Marker));
        let c = w.spawn((Pos(2.0, 0.0),));
        w.despawn(a);
        w.remove::<Marker>(b);
        w.despawn(c); // never had one
        w.insert(b, Pos(3.0, 0.0)); // keeps no marker
        let mut seen = Vec::new();
        w.drain_removed::<Marker>(|e| seen.push(e));
        assert_eq!(seen, [a, b]);
        seen.clear();
        w.drain_removed::<Marker>(|e| seen.push(e));
        assert!(seen.is_empty());
        // Untracked components log nothing.
        let d = w.spawn((Vel(0.0, 0.0),));
        w.despawn(d);
        w.drain_removed::<Vel>(|e| seen.push(e));
        assert!(seen.is_empty());
        // Clear and restore are not logged: the epoch tells mirrors to resynchronize.
        let epoch = w.resync_epoch();
        w.clear();
        assert_ne!(w.resync_epoch(), epoch);
    }

    #[test]
    fn untracked_writes_are_invisible_until_marked() {
        let mut w = World::new();
        let e = w.spawn((Pos(0.0, 0.0),));
        let mut changed = QueryState::<Entity, Changed<Pos>>::new(&mut w);
        assert_eq!(changed.iter(&mut w).count(), 1);
        w.get_mut_untracked::<Pos>(e).unwrap().0 = 5.0;
        assert_eq!(changed.iter(&mut w).count(), 0);
        assert!(w.mark_changed::<Pos>(e));
        assert_eq!(changed.iter(&mut w).count(), 1);
        assert_eq!(w.get::<Pos>(e), Some(&Pos(5.0, 0.0)));
        assert!(!w.mark_changed::<Vel>(e));
    }

    #[test]
    fn or_filters_visit_chunks_where_any_changed() {
        let mut w = World::new();
        let e = w.spawn((Pos(0.0, 0.0), Vel(0.0, 0.0)));
        let f = w.spawn((Pos(0.0, 0.0), Vel(0.0, 0.0), Marker));
        let mut q = QueryState::<Entity, Or<(Changed<Pos>, Changed<Vel>)>>::new(&mut w);
        assert_eq!(q.iter(&mut w).count(), 2);
        assert_eq!(q.iter(&mut w).count(), 0);
        w.get_mut::<Vel>(e).unwrap().0 = 1.0;
        assert_eq!(q.iter(&mut w).collect::<Vec<_>>(), [e]);
        w.get_mut::<Pos>(f).unwrap().0 = 1.0;
        assert_eq!(q.iter(&mut w).collect::<Vec<_>>(), [f]);
    }

    #[test]
    fn transform_padding_is_hidden_and_not_saved() {
        let info = <Transform as reflect::Reflect>::TYPE_INFO;
        assert!(!info.field("translation").unwrap().attrs.hidden);
        let pad = info.field("_padding").unwrap();
        assert!(pad.attrs.hidden && pad.attrs.skip);
    }
}

#[test]
fn set_parent_at_inserts_at_an_index() {
    let mut w = World::new();
    let p = w.spawn((Pos(0.0, 0.0),));
    let kids: Vec<Entity> = (0..4).map(|i| w.spawn((Pos(i as f32, 0.0),))).collect();
    for &k in &kids {
        w.set_parent(k, Some(p)).unwrap();
    }
    // Move the last child to the front, then the first to position 2.
    w.set_parent_at(kids[3], Some(p), 0).unwrap();
    assert_eq!(
        w.children(p).collect::<Vec<_>>(),
        [kids[3], kids[0], kids[1], kids[2]]
    );
    w.set_parent_at(kids[3], Some(p), 2).unwrap();
    assert_eq!(
        w.children(p).collect::<Vec<_>>(),
        [kids[0], kids[1], kids[3], kids[2]]
    );
    w.set_parent_at(kids[0], Some(p), 99).unwrap();
    assert_eq!(
        w.children(p).collect::<Vec<_>>(),
        [kids[1], kids[3], kids[2], kids[0]]
    );
    // Any index past the end is the end (`usize::MAX` too).
    w.set_parent_at(kids[1], Some(p), usize::MAX).unwrap();
    w.set_parent_at(kids[1], Some(p), 0).unwrap();
    assert_eq!(
        w.children(p).collect::<Vec<_>>(),
        [kids[1], kids[3], kids[2], kids[0]]
    );
    assert_eq!(w.child_index(kids[2]), Some(2));
    // Walking backwards through the links agrees.
    let last = w.get::<Children>(p).map(|c| c.last).unwrap();
    assert_eq!(last, kids[0]);
    let mut back = vec![last];
    let mut cur = last;
    while let Some(prev) = w
        .get::<Parent>(cur)
        .map(|l| l.prev)
        .filter(|e| *e != Entity::NULL)
    {
        back.push(prev);
        cur = prev;
    }
    back.reverse();
    assert_eq!(back, w.children(p).collect::<Vec<_>>());
}

#[test]
fn raw_chunk_access_reads_and_writes_plain_data_by_layout() {
    let mut w = World::new();
    let es: Vec<Entity> = (0..2000)
        .map(|i| w.spawn((Id(i), Pos(i as f32, 1.0))))
        .collect();
    w.spawn((Tracked(Arc::new(())), Id(9)));
    let pos = w.component_id::<Pos>().unwrap();
    let tracked = w.component_id::<Tracked>().unwrap();
    let mut seen = 0;
    let mut targets = Vec::new();
    for (ai, arch) in w.archetypes().iter().enumerate() {
        if let Some(col) = arch.column_index(tracked) {
            assert!(arch.pod_column(0, col).is_none(), "not plain data");
        }
        let Some(col) = arch.column_index(pos) else {
            continue;
        };
        for chunk in 0..arch.chunk_count() {
            let bytes = arch.pod_column(chunk, col).unwrap();
            let values: &[Pos] = bytemuck::cast_slice(bytes);
            assert_eq!(values.len(), arch.chunk_len(chunk));
            for (e, p) in arch.chunk_entities(chunk).iter().zip(values) {
                assert_eq!(w.get::<Pos>(*e), Some(p));
                seen += 1;
            }
            targets.push((ai, chunk, col));
        }
    }
    assert_eq!(seen, 2000);
    // Writing by layout, with and without change marking.
    let mut changed = QueryState::<&Pos, Changed<Pos>>::new(&mut w);
    changed.for_each(&mut w, |_| {});
    for &(ai, chunk, col) in &targets {
        let bytes = w.pod_column_mut(ai, chunk, col, false).unwrap();
        for p in bytemuck::cast_slice_mut::<u8, Pos>(bytes) {
            p.1 = 2.0;
        }
    }
    let mut n = 0;
    changed.for_each(&mut w, |_| n += 1);
    assert_eq!(n, 0, "unmarked writes are invisible to Changed");
    let (ai, chunk, col) = targets[0];
    w.pod_column_mut(ai, chunk, col, true).unwrap();
    changed.for_each(&mut w, |_| n += 1);
    assert!(n > 0, "marked writes are seen");
    assert_eq!(w.get::<Pos>(es[1500]), Some(&Pos(1500.0, 2.0)));
    // `get_raw_mut` writes one entity and marks it.
    let p = w.get_raw_mut(es[3], pos).unwrap();
    // SAFETY: a live `Pos` of this entity; `&mut World` is not used while writing.
    unsafe { p.cast::<Pos>().as_ptr().write(Pos(-1.0, -1.0)) };
    assert_eq!(w.get::<Pos>(es[3]), Some(&Pos(-1.0, -1.0)));
    assert!(w.pod_column_mut(99, 0, 0, false).is_none());
}
