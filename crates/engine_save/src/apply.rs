//! Applying a save to a world (ADR 0104): the level's entities matched by id and brought
//! to their saved state, the spawned ones created again, the destroyed ones removed, the
//! hierarchy restored in the children's order, the resources replaced.

use crate::SaveError;
use crate::body::{LoadedKey, LoadedSave};
use crate::capture::IdBuild;
use engine_ecs::DefaultValue;
use engine_ecs::reflected::InsertDefault;
use engine_ecs::{
    Children, ComponentId, Entity, Parent, QueryState, ReflectedKind, World, find_reflected,
};
use engine_reflect::TypeInfo;
use engine_scene::SceneId;
use std::collections::HashMap;

/// How [`apply_with`] puts a save into a world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOptions {
    /// Match the save's entities with ids to the world's (the level loaded again); off,
    /// every entity is created anew, without its id (a game's own saves added to a world).
    pub match_level: bool,
    /// Remove the level's entities the save records as destroyed.
    pub destroy: bool,
    /// Replace the resources.
    pub resources: bool,
    /// Set the simulation tick.
    pub clock: bool,
}

impl Default for ApplyOptions {
    /// A game loaded: everything.
    fn default() -> Self {
        Self {
            match_level: true,
            destroy: true,
            resources: true,
            clock: true,
        }
    }
}

impl ApplyOptions {
    /// Only adds the save's entities to the world, touching nothing else: a character, a
    /// building or a scene a game saved for its own reasons.
    pub const fn add_only() -> Self {
        Self {
            match_level: false,
            destroy: false,
            resources: false,
            clock: false,
        }
    }
}

/// What applying a save did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LoadReport {
    /// Entities of the world matched by id and brought to their saved state.
    pub matched: usize,
    /// Entities created.
    pub spawned: usize,
    /// The level's entities removed (destroyed before the save).
    pub destroyed: usize,
    /// Components written.
    pub components: usize,
    /// Resources written.
    pub resources: usize,
    /// What did not fit the current program: types and fields gone, kinds changed,
    /// types without `Default`. Not fatal.
    pub notes: Vec<String>,
}

/// Room for one value of a type, aligned, freed without dropping what is in it (values
/// are moved out into the world).
struct Scratch {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl Scratch {
    fn new(info: &TypeInfo) -> Option<Self> {
        let layout = std::alloc::Layout::from_size_align(info.size.max(1), info.align).ok()?;
        // SAFETY: a non-zero size.
        let ptr = unsafe { std::alloc::alloc(layout) };
        (!ptr.is_null()).then_some(Self { ptr, layout })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout; the value in it was moved out or never made.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// A saved type resolved against the program, once per save.
enum Resolved {
    Component {
        info: &'static TypeInfo,
        id: ComponentId,
        default: DefaultValue,
        scratch: Scratch,
    },
    Resource {
        info: &'static TypeInfo,
        get_mut: fn(&mut World) -> Option<*mut u8>,
        insert_default: Option<InsertDefault>,
    },
    /// Not loadable (gone, or without `Default`); the note is given once.
    Skipped,
}

fn resolve(world: &mut World, name: &str, notes: &mut Vec<String>) -> Resolved {
    let Some(reflected) = find_reflected(name) else {
        // A type made at run time (a script's component): its latest layout.
        if let Some(t) = engine_ecs::runtime_types()
            .into_iter()
            .rev()
            .find(|t| t.info.name == name)
            && let Some(id) = t.id(world)
            && let Some(scratch) = Scratch::new(t.info)
        {
            return Resolved::Component {
                info: t.info,
                id,
                default: DefaultValue::Runtime(t),
                scratch,
            };
        }
        notes.push(format!(
            "`{name}` is not in the program; its values skipped"
        ));
        return Resolved::Skipped;
    };
    match reflected.kind {
        ReflectedKind::Component { id, default } => {
            let Some(default) = default() else {
                notes.push(format!("component `{name}` has no Default; not loaded"));
                return Resolved::Skipped;
            };
            match Scratch::new(reflected.info) {
                Some(scratch) => Resolved::Component {
                    info: reflected.info,
                    id: id(world),
                    default: DefaultValue::Written(default),
                    scratch,
                },
                None => Resolved::Skipped,
            }
        }
        ReflectedKind::Resource {
            get_mut,
            insert_default,
            ..
        } => {
            let insert_default = insert_default();
            if insert_default.is_none() {
                notes.push(format!(
                    "resource `{name}` has no Default; its saved fields written over the current value"
                ));
            }
            Resolved::Resource {
                info: reflected.info,
                get_mut,
                insert_default,
            }
        }
    }
}

/// Puts `save` into `world`: see the module documentation. A level loaded again before
/// it gives the stable entities to match; without one, everything is created.
///
/// # Errors
///
/// If the save's data is damaged, or a text save has a value it cannot read. The world
/// then holds what was applied before, every value in it valid.
pub fn apply(world: &mut World, save: &LoadedSave) -> Result<LoadReport, SaveError> {
    apply_with(world, save, &ApplyOptions::default())
}

/// [`apply`], with what it does chosen ([`ApplyOptions::add_only`] for a game's own
/// saves).
///
/// # Errors
///
/// As [`apply`].
pub fn apply_with(
    world: &mut World,
    save: &LoadedSave,
    options: &ApplyOptions,
) -> Result<LoadReport, SaveError> {
    engine_profiler::zone!("save::apply");
    let mut report = LoadReport::default();
    let mut stable: HashMap<SceneId, Entity, IdBuild> =
        HashMap::with_capacity_and_hasher(save.entities.len(), IdBuild::default());
    if options.match_level {
        let mut q = QueryState::<(Entity, &SceneId)>::new(world);
        for (e, &id) in q.iter_readonly(world) {
            stable.insert(id, e);
        }
    }
    // Every entity first, so references between them resolve.
    let mut entity_of: Vec<Entity> = Vec::with_capacity(save.entities.len());
    let mut matched: Vec<bool> = Vec::with_capacity(save.entities.len());
    for loaded in &save.entities {
        let (e, was_there) = match loaded.key {
            LoadedKey::Excluded => (Entity::NULL, false),
            LoadedKey::Stable(_) if !options.match_level => (world.spawn(()), false),
            LoadedKey::Stable(id) => match stable.get(&id) {
                Some(&e) => (e, true),
                None => (world.spawn((id,)), false),
            },
            LoadedKey::Local => (world.spawn(()), false),
        };
        if loaded.key != LoadedKey::Excluded {
            if was_there {
                report.matched += 1;
            } else {
                report.spawned += 1;
            }
        }
        entity_of.push(e);
        matched.push(was_there);
    }
    let entity = |i: u32| entity_of.get(i as usize).copied().unwrap_or(Entity::NULL);
    let label = |text: &str| -> Option<Entity> { save.labels.get(text).map(|&i| entity(i)) };
    let (parent_id, children_id) = (
        world.component_id::<Parent>(),
        world.component_id::<Children>(),
    );
    let mut notes = Vec::new();
    let mut resolved: Vec<Option<Resolved>> = (0..save.types.len()).map(|_| None).collect();
    let mut kept: Vec<ComponentId> = Vec::new();
    let mut gone: Vec<ComponentId> = Vec::new();
    for (i, loaded) in save.entities.iter().enumerate() {
        let e = entity_of[i];
        if e == Entity::NULL {
            continue;
        }
        kept.clear();
        for value in &loaded.components {
            let Some(slot) = resolved.get_mut(value.ty as usize) else {
                continue;
            };
            let r = slot.get_or_insert_with(|| {
                resolve(world, &save.types[value.ty as usize].name, &mut notes)
            });
            let Resolved::Component {
                info,
                id,
                default,
                scratch,
            } = r
            else {
                continue;
            };
            kept.push(*id);
            report.components += 1;
            // A matched entity's own value: read in place, so fields the save skips keep
            // the level's (a renderer's mesh, a body's handle).
            if matched[i]
                && let Some(ptr) = world.get_raw_mut(e, *id)
            {
                // SAFETY: the entity's live value of the component `info` describes.
                unsafe { save.decode(value, info, ptr.as_ptr(), &entity, &label, &mut notes)? };
                continue;
            }
            // SAFETY: the scratch is room for one `info` value; the default makes it
            // valid, decoding keeps it valid, and `insert_raw` moves it into the world (the
            // entity is alive: just matched or made), leaving the scratch free again.
            unsafe {
                default.write(scratch.ptr);
                let decoded = save.decode(value, info, scratch.ptr, &entity, &label, &mut notes);
                world.insert_raw(e, *id, scratch.ptr);
                decoded?;
            }
        }
        if matched[i] {
            // The level's version of this entity may hold saved components the save says
            // it no longer had.
            gone.clear();
            gone.extend(
                world
                    .component_ids_of(e)
                    .unwrap_or_default()
                    .iter()
                    .copied()
                    .filter(|&c| Some(c) != parent_id && Some(c) != children_id)
                    .filter(|c| !kept.contains(c))
                    .filter(|&c| {
                        world
                            .components()
                            .type_info(c)
                            .is_some_and(|t| !t.attrs.skip)
                    }),
            );
            for &c in &gone {
                world.remove_raw(e, c);
            }
        }
    }
    restore_hierarchy(world, save, &entity_of);
    for id in save.destroyed.iter().filter(|_| options.destroy) {
        if let Some(&e) = stable.get(id)
            && world.despawn(e)
        {
            report.destroyed += 1;
        }
    }
    for value in save.resources.iter().filter(|_| options.resources) {
        let Some(slot) = resolved.get_mut(value.ty as usize) else {
            continue;
        };
        let r = slot
            .get_or_insert_with(|| resolve(world, &save.types[value.ty as usize].name, &mut notes));
        let Resolved::Resource {
            info,
            get_mut,
            insert_default,
        } = r
        else {
            continue;
        };
        // From its default, as components are: fields the save lacks do not keep play's.
        if let Some(insert) = insert_default {
            insert(world);
        }
        let Some(ptr) = get_mut(world) else {
            continue;
        };
        // SAFETY: the world's live value of the resource, described by `info`.
        unsafe { save.decode(value, info, ptr, &entity, &label, &mut notes)? };
        report.resources += 1;
    }
    if options.clock {
        world.set_sim_tick(save.sim_tick);
    }
    report.notes = notes;
    Ok(report)
}

/// Sets each entity's parent, the children of a parent in their saved order (each list
/// walked from its first, by the previous-sibling links).
fn restore_hierarchy(world: &mut World, save: &LoadedSave, entity_of: &[Entity]) {
    let n = save.entities.len();
    let mut next = vec![usize::MAX; n];
    let mut heads = Vec::new();
    for (i, loaded) in save.entities.iter().enumerate() {
        match (loaded.parent, loaded.prev.map(|q| q as usize)) {
            (Some(_), Some(q)) if q < n && save.entities[q].parent == loaded.parent => {
                if next[q] == usize::MAX {
                    next[q] = i;
                } else {
                    heads.push(i);
                }
            }
            (Some(_), _) => heads.push(i),
            (None, _) => {
                let e = entity_of[i];
                if e != Entity::NULL && world.parent(e).is_some() {
                    let _ = world.set_parent(e, None);
                }
            }
        }
    }
    let mut done = vec![false; n];
    let mut place = |world: &mut World, i: usize| {
        if std::mem::replace(&mut done[i], true) {
            return false;
        }
        let e = entity_of[i];
        let parent = save.entities[i]
            .parent
            .and_then(|p| entity_of.get(p as usize).copied())
            .filter(|&p| p != Entity::NULL);
        if e != Entity::NULL {
            // Re-parenting appends: detach first so the order is the saved one.
            if world.parent(e).is_some() {
                let _ = world.set_parent(e, None);
            }
            let _ = world.set_parent(e, parent);
        }
        true
    };
    for h in heads {
        let mut i = h;
        while i != usize::MAX && place(world, i) {
            i = next[i];
        }
    }
    // A damaged chain (a cycle) leaves some unplaced.
    for i in 0..n {
        if save.entities[i].parent.is_some() {
            place(world, i);
        }
    }
}
