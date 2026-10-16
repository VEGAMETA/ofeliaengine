//! Strings, lists, optional values and maps (ADR 0104 step 1): what a game's state is made
//! of beyond fixed fields (an inventory, a quest's name, flags by key).
//!
//! Their contents have no fixed offsets, so each description carries functions for its
//! concrete type: lengths, the storage, resizing with defaults, inserting. Serialization
//! walks them through those; [`TypeInfo::visit_leaves`] skips them.

use crate::{Attrs, Reflect, TypeInfo, TypeKind};
use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher, Hash};

/// A list, `Vec<T>`: its elements lie one after another, `element.size` apart.
#[derive(Debug)]
pub struct ListInfo {
    /// The element type.
    pub element: &'static TypeInfo,
    /// The number of elements.
    pub len: unsafe fn(*const u8) -> usize,
    /// The first element (dangling when empty).
    pub items: unsafe fn(*const u8) -> *const u8,
    /// The first element, writable.
    pub items_mut: unsafe fn(*mut u8) -> *mut u8,
    /// Resizes to the length given, new elements `Default`.
    pub resize: unsafe fn(*mut u8, usize),
}

/// An optional value, `Option<T>`.
#[derive(Debug)]
pub struct OptionInfo {
    /// The value's type.
    pub inner: &'static TypeInfo,
    /// The value, or null for `None`.
    pub get: unsafe fn(*const u8) -> *const u8,
    /// `true`: makes it `Some` (a default value unless one is there) and returns the value;
    /// `false`: makes it `None` and returns null.
    pub set: unsafe fn(*mut u8, bool) -> *mut u8,
}

/// A map, `BTreeMap<K, V>` or `HashMap<K, V, S>`.
#[derive(Debug)]
pub struct MapInfo {
    /// The key type.
    pub key: &'static TypeInfo,
    /// The value type.
    pub value: &'static TypeInfo,
    /// Whether [`Self::for_each`] visits the keys in order (a `BTreeMap`). A `HashMap`'s
    /// order is arbitrary: writers that need a stable order sort.
    pub ordered: bool,
    /// The number of entries.
    pub len: unsafe fn(*const u8) -> usize,
    /// Calls the function with each entry's key and value.
    pub for_each: unsafe fn(*const u8, &mut dyn FnMut(*const u8, *const u8)),
    /// Removes every entry.
    pub clear: unsafe fn(*mut u8),
    /// Makes a default key, lets the function fill it (returning `false` abandons it),
    /// inserts it with a default value unless the key is there, and returns the value;
    /// null when abandoned.
    pub insert: unsafe fn(*mut u8, &mut dyn FnMut(*mut u8) -> bool) -> *mut u8,
}

/// The text of the `String` at `ptr`.
///
/// # Safety
///
/// `ptr` points to a live `String` that outlives `'a` unchanged.
pub unsafe fn read_str<'a>(ptr: *const u8) -> &'a str {
    // SAFETY: guaranteed by the caller.
    unsafe { (*ptr.cast::<String>()).as_str() }
}

/// Replaces the text of the `String` at `ptr`, reusing its allocation.
///
/// # Safety
///
/// `ptr` points to a live `String` nothing else borrows.
pub unsafe fn write_str(ptr: *mut u8, text: &str) {
    // SAFETY: guaranteed by the caller.
    let s = unsafe { &mut *ptr.cast::<String>() };
    s.clear();
    s.push_str(text);
}

impl Reflect for String {
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "String",
        size: core::mem::size_of::<String>(),
        align: core::mem::align_of::<String>(),
        kind: TypeKind::String,
        attrs: Attrs::NONE,
    };
}

unsafe fn list_len<T>(p: *const u8) -> usize {
    // SAFETY: the description's contract: `p` is a live `Vec<T>`.
    unsafe { (*p.cast::<Vec<T>>()).len() }
}

unsafe fn list_items<T>(p: *const u8) -> *const u8 {
    // SAFETY: as above.
    unsafe { (*p.cast::<Vec<T>>()).as_ptr().cast() }
}

unsafe fn list_items_mut<T>(p: *mut u8) -> *mut u8 {
    // SAFETY: as above, and not borrowed elsewhere.
    unsafe { (*p.cast::<Vec<T>>()).as_mut_ptr().cast() }
}

unsafe fn list_resize<T: Default>(p: *mut u8, len: usize) {
    // SAFETY: as above.
    unsafe { (*p.cast::<Vec<T>>()).resize_with(len, T::default) }
}

impl<T: Reflect + Default> Reflect for Vec<T> {
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "list",
        size: core::mem::size_of::<Vec<T>>(),
        align: core::mem::align_of::<Vec<T>>(),
        kind: TypeKind::List(&ListInfo {
            element: T::TYPE_INFO,
            len: list_len::<T>,
            items: list_items::<T>,
            items_mut: list_items_mut::<T>,
            resize: list_resize::<T>,
        }),
        attrs: Attrs::NONE,
    };
}

unsafe fn option_get<T>(p: *const u8) -> *const u8 {
    // SAFETY: the description's contract: `p` is a live `Option<T>`.
    match unsafe { &*p.cast::<Option<T>>() } {
        Some(v) => core::ptr::from_ref(v).cast(),
        None => core::ptr::null(),
    }
}

unsafe fn option_set<T: Default>(p: *mut u8, some: bool) -> *mut u8 {
    // SAFETY: as above, and not borrowed elsewhere.
    let o = unsafe { &mut *p.cast::<Option<T>>() };
    if some {
        core::ptr::from_mut(o.get_or_insert_with(T::default)).cast()
    } else {
        *o = None;
        core::ptr::null_mut()
    }
}

impl<T: Reflect + Default> Reflect for Option<T> {
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "option",
        size: core::mem::size_of::<Option<T>>(),
        align: core::mem::align_of::<Option<T>>(),
        kind: TypeKind::Option(&OptionInfo {
            inner: T::TYPE_INFO,
            get: option_get::<T>,
            set: option_set::<T>,
        }),
        attrs: Attrs::NONE,
    };
}

unsafe fn btree_len<K, V>(p: *const u8) -> usize {
    // SAFETY: the description's contract: `p` is a live `BTreeMap<K, V>`.
    unsafe { (*p.cast::<BTreeMap<K, V>>()).len() }
}

unsafe fn btree_for_each<K, V>(p: *const u8, f: &mut dyn FnMut(*const u8, *const u8)) {
    // SAFETY: as above.
    for (k, v) in unsafe { &*p.cast::<BTreeMap<K, V>>() } {
        f(core::ptr::from_ref(k).cast(), core::ptr::from_ref(v).cast());
    }
}

unsafe fn btree_clear<K, V>(p: *mut u8) {
    // SAFETY: as above, and not borrowed elsewhere.
    unsafe { (*p.cast::<BTreeMap<K, V>>()).clear() }
}

unsafe fn btree_insert<K: Ord + Default, V: Default>(
    p: *mut u8,
    fill: &mut dyn FnMut(*mut u8) -> bool,
) -> *mut u8 {
    let mut key = K::default();
    if !fill(core::ptr::from_mut(&mut key).cast()) {
        return core::ptr::null_mut();
    }
    // SAFETY: as above.
    let map = unsafe { &mut *p.cast::<BTreeMap<K, V>>() };
    core::ptr::from_mut(map.entry(key).or_default()).cast()
}

impl<K: Reflect + Default + Ord, V: Reflect + Default> Reflect for BTreeMap<K, V> {
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "map",
        size: core::mem::size_of::<BTreeMap<K, V>>(),
        align: core::mem::align_of::<BTreeMap<K, V>>(),
        kind: TypeKind::Map(&MapInfo {
            key: K::TYPE_INFO,
            value: V::TYPE_INFO,
            ordered: true,
            len: btree_len::<K, V>,
            for_each: btree_for_each::<K, V>,
            clear: btree_clear::<K, V>,
            insert: btree_insert::<K, V>,
        }),
        attrs: Attrs::NONE,
    };
}

unsafe fn hash_len<K, V, S>(p: *const u8) -> usize {
    // SAFETY: the description's contract: `p` is a live `HashMap<K, V, S>`.
    unsafe { (*p.cast::<HashMap<K, V, S>>()).len() }
}

unsafe fn hash_for_each<K, V, S>(p: *const u8, f: &mut dyn FnMut(*const u8, *const u8)) {
    // SAFETY: as above.
    for (k, v) in unsafe { &*p.cast::<HashMap<K, V, S>>() } {
        f(core::ptr::from_ref(k).cast(), core::ptr::from_ref(v).cast());
    }
}

unsafe fn hash_clear<K, V, S>(p: *mut u8) {
    // SAFETY: as above, and not borrowed elsewhere.
    unsafe { (*p.cast::<HashMap<K, V, S>>()).clear() }
}

unsafe fn hash_insert<K: Eq + Hash + Default, V: Default, S: BuildHasher>(
    p: *mut u8,
    fill: &mut dyn FnMut(*mut u8) -> bool,
) -> *mut u8 {
    let mut key = K::default();
    if !fill(core::ptr::from_mut(&mut key).cast()) {
        return core::ptr::null_mut();
    }
    // SAFETY: as above.
    let map = unsafe { &mut *p.cast::<HashMap<K, V, S>>() };
    core::ptr::from_mut(map.entry(key).or_default()).cast()
}

impl<K, V, S> Reflect for HashMap<K, V, S>
where
    K: Reflect + Default + Eq + Hash,
    V: Reflect + Default,
    S: BuildHasher + Default + 'static,
{
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "map",
        size: core::mem::size_of::<HashMap<K, V, S>>(),
        align: core::mem::align_of::<HashMap<K, V, S>>(),
        kind: TypeKind::Map(&MapInfo {
            key: K::TYPE_INFO,
            value: V::TYPE_INFO,
            ordered: false,
            len: hash_len::<K, V, S>,
            for_each: hash_for_each::<K, V, S>,
            clear: hash_clear::<K, V, S>,
            insert: hash_insert::<K, V, S>,
        }),
        attrs: Attrs::NONE,
    };
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::{Field, Reflect};

    #[derive(Reflect, Default, Debug, PartialEq, Clone)]
    struct Item {
        id: u32,
        count: u16,
    }

    #[derive(Reflect, Default)]
    #[reflect(version = 3)]
    struct Bag {
        owner: String,
        items: Vec<Item>,
        best: Option<Item>,
        flags: BTreeMap<String, bool>,
        counts: HashMap<u32, Vec<u8>>,
        gold: u64,
    }

    fn field(name: &str) -> &'static Field {
        Bag::TYPE_INFO.field(name).unwrap()
    }

    /// The `u32` or `u16` at `p`.
    unsafe fn put(p: *mut u8, f: &Field, v: u32) {
        // SAFETY: the caller's value holds the field.
        unsafe {
            match f.ty.size {
                4 => p.add(f.offset).cast::<u32>().write_unaligned(v),
                _ => p.add(f.offset).cast::<u16>().write_unaligned(v as u16),
            }
        }
    }

    #[test]
    fn containers_are_described_with_their_contents() {
        assert_eq!(Bag::TYPE_INFO.attrs.version, 3);
        assert!(matches!(field("owner").ty.kind, TypeKind::String));
        let TypeKind::List(list) = field("items").ty.kind else {
            panic!("a list");
        };
        assert_eq!(list.element.name, "Item");
        let TypeKind::Option(option) = field("best").ty.kind else {
            panic!("an option");
        };
        assert_eq!(option.inner.name, "Item");
        let TypeKind::Map(flags) = field("flags").ty.kind else {
            panic!("a map");
        };
        assert!(flags.ordered && matches!(flags.key.kind, TypeKind::String));
        let TypeKind::Map(counts) = field("counts").ty.kind else {
            panic!("a map");
        };
        assert!(!counts.ordered && matches!(counts.value.kind, TypeKind::List(_)));
        // Only fixed fields are leaves.
        let mut leaves = Vec::new();
        Bag::TYPE_INFO.visit_leaves(&mut |path, _, _, _| leaves.push(path.to_string()));
        assert_eq!(leaves, ["gold"]);
    }

    #[test]
    fn contents_are_read_and_written_through_the_descriptions() {
        let mut bag = Bag::default();
        let base = core::ptr::from_mut(&mut bag).cast::<u8>();
        let item = |name: &str| Item::TYPE_INFO.field(name).unwrap();
        // SAFETY: every pointer below is a field of `bag`, described by its own type.
        unsafe {
            write_str(base.add(field("owner").offset), "Ofelia");
            assert_eq!(read_str(base.add(field("owner").offset)), "Ofelia");

            let TypeKind::List(list) = field("items").ty.kind else {
                unreachable!()
            };
            let items = base.add(field("items").offset);
            (list.resize)(items, 2);
            assert_eq!((list.len)(items), 2);
            let first = (list.items_mut)(items);
            put(first.add(list.element.size), item("count"), 7);
            put(first, item("id"), 11);

            let TypeKind::Option(option) = field("best").ty.kind else {
                unreachable!()
            };
            let best = base.add(field("best").offset);
            assert!((option.get)(best).is_null());
            put((option.set)(best, true), item("id"), 5);

            let TypeKind::Map(flags) = field("flags").ty.kind else {
                unreachable!()
            };
            let map = base.add(field("flags").offset);
            for key in ["b", "a"] {
                let value = (flags.insert)(map, &mut |k| {
                    write_str(k, key);
                    true
                });
                value.write(1);
            }
            assert!((flags.insert)(map, &mut |_| false).is_null(), "abandoned");
            let mut keys = Vec::new();
            (flags.for_each)(map, &mut |k, v| keys.push((read_str(k).to_owned(), *v)));
            assert_eq!(keys, [("a".to_owned(), 1), ("b".to_owned(), 1)], "in order");

            let TypeKind::Map(counts) = field("counts").ty.kind else {
                unreachable!()
            };
            let map = base.add(field("counts").offset);
            let value = (counts.insert)(map, &mut |k| {
                k.cast::<u32>().write(9);
                true
            });
            let TypeKind::List(bytes) = counts.value.kind else {
                unreachable!()
            };
            (bytes.resize)(value, 3);
            assert_eq!((counts.len)(map), 1);
            (counts.clear)(map);
            assert_eq!((counts.len)(map), 0);
        }
        assert_eq!(bag.owner, "Ofelia");
        assert_eq!(
            bag.items,
            [Item { id: 11, count: 0 }, Item { id: 0, count: 7 }]
        );
        assert_eq!(bag.best, Some(Item { id: 5, count: 0 }));
        assert_eq!(bag.flags.len(), 2);
        assert!(bag.counts.is_empty());
        // Back to none.
        let TypeKind::Option(option) = field("best").ty.kind else {
            unreachable!()
        };
        // SAFETY: as above.
        unsafe {
            let best = core::ptr::from_mut(&mut bag)
                .cast::<u8>()
                .add(field("best").offset);
            assert!((option.set)(best, false).is_null());
        }
        assert_eq!(bag.best, None);
    }
}
