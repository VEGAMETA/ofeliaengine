//! The program's list of reflected types (ADR 0104 step 1): filled by the linker from every
//! crate, found by name, and enough to make components and resources from a file.

#![allow(clippy::unwrap_used)]

use engine_ecs::reflect::{Reflect, write_str};
use engine_ecs::{
    ReflectedKind, World, find_reflected, reflect_component, reflect_resource, reflected_types,
};

#[derive(Reflect, Default, Clone, Copy, PartialEq, Debug)]
struct Health {
    current: f32,
    max: f32,
}
reflect_component!(Health);

/// No `Default`: listed, but it cannot be made from a file.
#[derive(Reflect)]
struct Marker {
    tag: u32,
}
reflect_component!(Marker);

#[derive(Reflect, Default)]
struct Score {
    points: u32,
    best: String,
}
reflect_resource!(Score);

#[test]
fn reflected_types_are_listed_from_every_crate() {
    let names: Vec<&str> = reflected_types().iter().map(|t| t.info.name).collect();
    // This crate's and engine_ecs's own.
    for name in [
        "Health",
        "Marker",
        "Score",
        "Parent",
        "Children",
        "Transform",
    ] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "no two types share a name");
    assert!(find_reflected("Nothing").is_none());
}

#[test]
fn a_component_is_made_and_inserted_from_its_listing() {
    let mut world = World::new();
    let e = world.spawn((engine_ecs::Transform::default(),));
    let ReflectedKind::Component { id, default } = find_reflected("Health").unwrap().kind else {
        panic!("a component");
    };
    let id = id(&mut world);
    assert_eq!(Some(id), world.component_id::<Health>());
    let write = default().expect("Health has a default");
    let mut value = core::mem::MaybeUninit::<Health>::uninit();
    let max = Health::TYPE_INFO.field("max").unwrap().offset;
    // SAFETY: `value` is a `Health`'s memory; the default is written before the field and
    // the value is moved into the world once.
    unsafe {
        write(value.as_mut_ptr().cast());
        value
            .as_mut_ptr()
            .cast::<u8>()
            .add(max)
            .cast::<f32>()
            .write(80.0);
        assert!(world.insert_raw(e, id, value.as_ptr().cast()));
    }
    assert_eq!(
        world.get::<Health>(e),
        Some(&Health {
            current: 0.0,
            max: 80.0
        })
    );
    let ReflectedKind::Component { default, .. } = find_reflected("Marker").unwrap().kind else {
        panic!("a component");
    };
    assert!(default().is_none(), "Marker has no default");
}

#[test]
fn a_resource_is_made_read_and_written_through_its_listing() {
    let mut world = World::new();
    let ReflectedKind::Resource {
        get,
        get_mut,
        insert_default,
    } = find_reflected("Score").unwrap().kind
    else {
        panic!("a resource");
    };
    assert!(get(&world).is_none());
    insert_default().unwrap()(&mut world);
    let best = Score::TYPE_INFO.field("best").unwrap().offset;
    let ptr = get_mut(&mut world).unwrap();
    // SAFETY: `ptr` is the world's `Score`; `best` is its `String`.
    unsafe { write_str(ptr.add(best), "Ofelia") };
    assert_eq!(world.resource::<Score>().unwrap().best, "Ofelia");
    assert!(get(&world).is_some());
}
