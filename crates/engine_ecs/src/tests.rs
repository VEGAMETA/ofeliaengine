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
    assert_eq!(changed.iter(&mut w).count(), 0, "a query's own writes are not news to it");

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
    assert_eq!(w.children(root).collect::<Vec<_>>(), vec![kids[0], kids[2], kids[3]]);
    w.set_parent(kids[0], None).unwrap();
    w.set_parent(kids[3], None).unwrap();
    assert_eq!(w.children(root).collect::<Vec<_>>(), vec![kids[2]]);
    w.set_parent(kids[0], Some(root)).unwrap();
    assert_eq!(w.children(root).collect::<Vec<_>>(), vec![kids[2], kids[0]]);
    // Cycles are rejected; grandchildren work.
    w.set_parent(kids[3], Some(kids[0])).unwrap();
    assert_eq!(w.set_parent(root, Some(kids[3])), Err(HierarchyError::Cycle));
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
