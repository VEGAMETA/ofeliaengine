//! Script components replicated by name (ADR 0114 §8 rule 5, E7-6b): found among the
//! types made at run time when a session starts, their plain props sent with their
//! precision, a client's value starting from the script's defaults (its texts and the
//! props not replicated).

#![allow(clippy::unwrap_used, missing_docs)]

use crate::common::*;
use engine_ecs::{ComponentId, Entity, RuntimeType, TextField, World};
use engine_net::{NetSchema, Replicated, ServerSettings};
use engine_reflect::{Attrs, Field, Reflect, TypeInfo, TypeKind};

static FIELDS: [Field; 4] = [
    Field {
        name: "lift",
        offset: 0,
        ty: <f32 as Reflect>::TYPE_INFO,
        attrs: Attrs {
            net_quantum: Some(0.01),
            ..Attrs::NONE
        },
    },
    Field {
        name: "opened",
        offset: 4,
        ty: <bool as Reflect>::TYPE_INFO,
        attrs: Attrs::NONE,
    },
    Field {
        name: "speed",
        offset: 8,
        ty: <f32 as Reflect>::TYPE_INFO,
        attrs: Attrs {
            net_skip: true,
            ..Attrs::NONE
        },
    },
    Field {
        name: "sound",
        offset: 16,
        ty: <String as Reflect>::TYPE_INFO,
        attrs: Attrs::NONE,
    },
];
static DOOR: TypeInfo = TypeInfo {
    name: "NetTestDoor",
    size: 40,
    align: 8,
    kind: TypeKind::Struct(&FIELDS),
    attrs: Attrs::NONE,
};
static TEXTS: [TextField; 1] = [TextField {
    offset: 16,
    default: "door/creak.ogg",
}];

/// What a script host makes of `NetTestDoor = component("NetTestDoor", { lift = 0, opened =
/// false, speed = prop.number(2, { net_skip = true }), sound = "door/creak.ogg" })`.
fn door() -> RuntimeType {
    let mut defaults = vec![0u8; 40];
    defaults[8..12].copy_from_slice(&2.0f32.to_le_bytes());
    // SAFETY: room for a `String` at the text's offset; an empty one owns nothing.
    unsafe {
        defaults
            .as_mut_ptr()
            .add(16)
            .cast::<String>()
            .write_unaligned(String::new());
    }
    RuntimeType {
        info: &DOOR,
        component: "script::NetTestDoor".into(),
        layout: core::alloc::Layout::from_size_align(40, 8).unwrap(),
        defaults: defaults.into_boxed_slice(),
        texts: &TEXTS,
        source: "scripts/door.luau".into(),
    }
}

fn door_schema() -> NetSchema {
    schema()
        .replicate_script("NetTestDoor")
        .replicate_script("NetTestNotDeclared")
}

fn f32_at(world: &World, e: Entity, id: ComponentId, at: usize) -> f32 {
    let base = world.get_raw(e, id).unwrap();
    // SAFETY: an f32 of the door at `at`.
    unsafe { base.as_ptr().add(at).cast::<f32>().read_unaligned() }
}

fn text_at(world: &World, e: Entity, id: ComponentId) -> String {
    let base = world.get_raw(e, id).unwrap();
    // SAFETY: the door's `sound`, a `String` at 16.
    unsafe { engine_reflect::read_str(base.as_ptr().add(16)) }.to_owned()
}

#[test]
fn a_script_component_replicates_by_name_from_its_defaults() {
    let t = door();
    // SAFETY: plain data but for its one text, a `String` the glue drops.
    unsafe { engine_ecs::add_runtime_type(t.clone()) };
    let mut sim = Sim::with_schema(
        7,
        conditions(20, 2, 0.0),
        ServerSettings::default(),
        door_schema,
    );
    let c = sim.add_client();
    // The host's door: open, lifted, fast, slamming.
    let id = t.id(&mut sim.server.world).unwrap();
    let mut value = [0u8; 40];
    // SAFETY: room for one door; `insert_raw` moves it into the world.
    unsafe {
        t.write_default(value.as_mut_ptr());
        value[0..4].copy_from_slice(&1.25f32.to_le_bytes());
        value[4] = 1;
        value[8..12].copy_from_slice(&5.0f32.to_le_bytes());
        engine_reflect::write_str(value.as_mut_ptr().add(16), "door/slam.ogg");
    }
    let door = sim.server.world.spawn((Replicated::new(),));
    // SAFETY: as above.
    assert!(unsafe { sim.server.world.insert_raw(door, id, value.as_ptr()) });
    sim.run(1.0);
    let client = &sim.clients[c].world;
    let cid = client
        .components()
        .id_by_name("script::NetTestDoor")
        .unwrap();
    let seen: Vec<Entity> = iter_replicated(client)
        .into_iter()
        .map(|(e, _)| e)
        .filter(|&e| client.get_raw(e, cid).is_some())
        .collect();
    assert_eq!(seen.len(), 1, "the door came");
    let e = seen[0];
    assert!(
        (f32_at(client, e, cid, 0) - 1.25).abs() < 0.006,
        "its lift, to a hundredth"
    );
    let opened = client.get_raw(e, cid).unwrap();
    // SAFETY: the door's `opened`, a bool at 4.
    assert_eq!(unsafe { opened.as_ptr().add(4).read() }, 1);
    assert_eq!(
        f32_at(client, e, cid, 8),
        2.0,
        "a skipped prop keeps its default"
    );
    assert_eq!(
        text_at(client, e, cid),
        "door/creak.ogg",
        "texts are not sent"
    );
    // The host lowers it: the client follows.
    let base = sim.server.world.get_raw_mut(door, id).unwrap();
    // SAFETY: the door's `lift`.
    unsafe { base.as_ptr().cast::<f32>().write_unaligned(0.5) };
    sim.run(0.5);
    let client = &sim.clients[c].world;
    assert!((f32_at(client, e, cid, 0) - 0.5).abs() < 0.006);
    assert_eq!(
        text_at(&sim.server.world, door, id),
        "door/slam.ogg",
        "the host's own text kept"
    );
}
