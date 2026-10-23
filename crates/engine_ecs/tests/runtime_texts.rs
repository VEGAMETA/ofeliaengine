//! Texts of component types made at run time (a script's text props, ADR 0114 E7-6a): a
//! value owns its strings, dropped with it, and moved by a relayout. The live bytes of the
//! process come back to where they were: nothing leaks, nothing is freed twice.

#![allow(clippy::unwrap_used)]

use engine_ecs::{Entity, Relayout, RuntimeType, TextField, World};
use engine_reflect::{Attrs, Field, Reflect, TypeInfo, TypeKind};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

/// The process's live heap bytes.
static LIVE: AtomicIsize = AtomicIsize::new(0);

struct Counting;

// SAFETY: forwards to the system allocator, counting what it hands out and takes back.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: as the caller of `alloc` promises.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: as the caller of `dealloc` promises.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        // SAFETY: as the caller of `realloc` promises.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

static OLD_FIELDS: [Field; 2] = [
    Field {
        name: "count",
        offset: 0,
        ty: <f32 as Reflect>::TYPE_INFO,
        attrs: Attrs::NONE,
    },
    Field {
        name: "sound",
        offset: 8,
        ty: <String as Reflect>::TYPE_INFO,
        attrs: Attrs::NONE,
    },
];
static OLD: TypeInfo = TypeInfo {
    name: "Chime",
    size: 32,
    align: 8,
    kind: TypeKind::Struct(&OLD_FIELDS),
    attrs: Attrs::NONE,
};
static OLD_TEXTS: [TextField; 1] = [TextField {
    offset: 8,
    default: "ui/chime.ogg",
}];
static NEW_FIELDS: [Field; 3] = [
    Field {
        name: "label",
        offset: 0,
        ty: <String as Reflect>::TYPE_INFO,
        attrs: Attrs::NONE,
    },
    Field {
        name: "sound",
        offset: 24,
        ty: <String as Reflect>::TYPE_INFO,
        attrs: Attrs::NONE,
    },
    Field {
        name: "count",
        offset: 48,
        ty: <f32 as Reflect>::TYPE_INFO,
        attrs: Attrs::NONE,
    },
];
static NEW: TypeInfo = TypeInfo {
    name: "Chime",
    size: 56,
    align: 8,
    kind: TypeKind::Struct(&NEW_FIELDS),
    attrs: Attrs::NONE,
};
static NEW_TEXTS: [TextField; 2] = [
    TextField {
        offset: 0,
        default: "a bell",
    },
    TextField {
        offset: 24,
        default: "ui/bell.ogg",
    },
];

/// A layout's default bytes: zeros, its texts empty strings.
fn defaults(size: usize, texts: &[TextField]) -> Box<[u8]> {
    let mut bytes = vec![0u8; size];
    for t in texts {
        // SAFETY: room for a `String` at the text's offset; an empty one owns nothing.
        unsafe {
            bytes
                .as_mut_ptr()
                .add(t.offset)
                .cast::<String>()
                .write_unaligned(String::new());
        }
    }
    bytes.into_boxed_slice()
}

fn runtime(component: &str, info: &'static TypeInfo, texts: &'static [TextField]) -> RuntimeType {
    RuntimeType {
        info,
        component: component.into(),
        layout: Layout::from_size_align(info.size, info.align).unwrap(),
        defaults: defaults(info.size, texts),
        texts,
        source: "".into(),
    }
}

/// The text at `offset` of `e`'s value of `id`.
fn text(world: &World, e: Entity, id: engine_ecs::ComponentId, offset: usize) -> String {
    let ptr = world.get_raw(e, id).unwrap();
    // SAFETY: a live value whose text field is at `offset`.
    unsafe { engine_reflect::read_str(ptr.as_ptr().add(offset)) }.to_owned()
}

#[test]
fn a_runtime_types_texts_are_its_values_own() {
    let old_type = runtime("script::Chime", &OLD, &OLD_TEXTS);
    let new_type = runtime("script::Chime#2", &NEW, &NEW_TEXTS);
    let before = LIVE.load(Ordering::Relaxed);
    let mut world = World::new();
    let old = old_type.id(&mut world).unwrap();
    assert!(!world.components().is_pod(old), "texts are no plain data");
    let mut room = [0u8; 32];
    let entities: Vec<Entity> = (0..4)
        .map(|n| {
            let e = world.spawn(());
            // SAFETY: room for one value; `insert_raw` moves it into the world.
            unsafe {
                old_type.write_default(room.as_mut_ptr());
                room[0..4].copy_from_slice(&(n as f32).to_le_bytes());
                assert!(world.insert_raw(e, old, room.as_ptr()));
            }
            e
        })
        .collect();
    assert_eq!(text(&world, entities[0], old, 8), "ui/chime.ogg");
    // One's text changed; one despawned, one's component taken off, one replaced.
    let ptr = world.get_raw_mut(entities[1], old).unwrap();
    // SAFETY: the live value's `String` at its text field.
    unsafe { engine_reflect::write_str(ptr.as_ptr().add(8), "ui/gong.ogg") };
    assert!(world.despawn(entities[2]));
    world.remove_raw(entities[3], old);
    // SAFETY: as above, the value taken by the world, the old one dropped.
    unsafe {
        old_type.write_default(room.as_mut_ptr());
        assert!(world.insert_raw(entities[0], old, room.as_ptr()));
    }
    // A relayout: `sound` and `count` keep their values, `label` is new.
    let new = new_type.id(&mut world).unwrap();
    let r = Relayout::between(
        old,
        &OLD,
        new,
        &NEW,
        new_type.defaults.clone(),
        new_type.texts,
    );
    assert_eq!(r.moves.iter().filter(|m| m.text).count(), 1);
    for &e in &entities[..2] {
        let ptr = world.get_raw_mut(e, old).unwrap();
        let mut moved = [0u8; 56];
        // SAFETY: the live value of the old layout, room for one of the new; the world takes
        // the new, and drops what the old kept.
        unsafe {
            r.take(ptr.as_ptr(), moved.as_mut_ptr());
            assert!(world.insert_raw(e, new, moved.as_ptr()));
        }
        world.remove_raw(e, old);
    }
    assert_eq!(text(&world, entities[1], new, 24), "ui/gong.ogg", "moved");
    assert_eq!(
        text(&world, entities[1], new, 0),
        "a bell",
        "the new text's default"
    );
    assert_eq!(text(&world, entities[0], new, 24), "ui/chime.ogg");
    let count = world.get_raw(entities[1], new).unwrap();
    // SAFETY: the value's `f32` at 48.
    assert_eq!(unsafe { count.as_ptr().add(48).cast::<f32>().read() }, 1.0);
    drop((r, entities));
    drop(world);
    assert_eq!(
        LIVE.load(Ordering::Relaxed),
        before,
        "every text freed once"
    );
}
