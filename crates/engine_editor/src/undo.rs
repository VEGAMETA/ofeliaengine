//! Undo and redo (CLAUDE.md §12 "everything is undoable"; ADR 0031).
//!
//! A [`Transaction`] records, for every entity an action touched, the entity's state
//! before and after: its components, and its place in the hierarchy (parent and index
//! among the siblings). Plain-data components are kept as bytes; the others scenes save
//! (`Name`, `Model`, a game's components with strings and lists) as the lines a scene
//! writes for them. Entities are identified by their [`SceneId`], so a despawned and
//! restored entity is the same entity to later transactions. Entity references inside
//! components are stored as scene ids and resolved again on restore.
//!
//! Editor code does not write inverse operations. It marks what it is about to change
//! and then changes the world freely:
//!
//! ```
//! use engine_ecs::{Transform, World};
//! use engine_editor::History;
//! use engine_scene::SceneId;
//!
//! let mut world = World::new();
//! let e = world.spawn((SceneId::new(), Transform::from_xyz(0.0, 0.0, 0.0)));
//! let mut history = History::new();
//!
//! history.begin("Move", None);
//! history.touch(&mut world, e);
//! world.get_mut::<Transform>(e).unwrap().translation.x = 5.0;
//! history.commit(&mut world);
//!
//! history.undo(&mut world);
//! assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 0.0);
//! history.redo(&mut world);
//! assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 5.0);
//! ```

use engine_ecs::reflected::WriteDefault;
use engine_ecs::{Children, ComponentId, Entity, Parent, QueryState, World};
use engine_reflect::{TypeInfo, TypeKind};
use engine_scene::SceneId;
use engine_scene::text::ReadRefs;
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};

/// A component's recorded value.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    /// Plain data: the value's bytes, with entity references zeroed (they live in `refs`:
    /// byte offset and the referenced scene id).
    Bytes {
        bytes: Box<[u8]>,
        refs: Vec<(u32, Option<SceneId>)>,
    },
    /// Not plain data: the lines a scene writes for it, entity references by scene id.
    Lines(Box<str>),
}

#[derive(Debug, Clone, PartialEq)]
struct ComponentRecord {
    id: ComponentId,
    value: Value,
}

impl ComponentRecord {
    fn bytes(&self) -> usize {
        16 + match &self.value {
            Value::Bytes { bytes, refs } => bytes.len() + refs.len() * 24,
            Value::Lines(text) => text.len(),
        }
    }
}

/// How a component that is not plain data is kept: as the scene lines it saves with.
#[derive(Clone, Copy)]
struct LinesCodec {
    info: &'static TypeInfo,
    default: WriteDefault,
    /// Whether the type holds entity references (for finding referrers).
    entities: bool,
}

/// Each component's way of being kept, found once per component.
#[derive(Default)]
struct Codecs {
    lines: HashMap<ComponentId, Option<LinesCodec>>,
}

impl Codecs {
    /// The scene-lines codec of a component that is not plain data. None for plain data,
    /// and for components scenes do not save: runtime state (`hidden`, `skip`), types
    /// without reflection or without `Default`.
    fn lines(&mut self, world: &World, id: ComponentId) -> Option<LinesCodec> {
        *self.lines.entry(id).or_insert_with(|| {
            let reg = world.components();
            if reg.is_pod(id) {
                return None;
            }
            let info = reg.type_info(id)?;
            let layout = reg.layout(id)?;
            if info.attrs.hidden
                || info.attrs.skip
                || layout.size() != info.size
                || layout.align() != info.align
            {
                return None;
            }
            let engine_ecs::ReflectedKind::Component { default, .. } =
                &engine_ecs::find_reflected(info.name)?.kind
            else {
                return None;
            };
            Some(LinesCodec {
                info,
                default: default()?,
                entities: holds_entities(info),
            })
        })
    }

    /// Whether undo records the component: plain data, or saved by scenes.
    fn records(&mut self, world: &World, id: ComponentId) -> bool {
        world.components().is_pod(id) || self.lines(world, id).is_some()
    }
}

/// Whether a type holds entity references anywhere (inside lists, options and maps too).
fn holds_entities(ty: &TypeInfo) -> bool {
    if engine_scene::text::is_entity(ty) {
        return true;
    }
    match ty.kind {
        TypeKind::Struct(fields) => fields.iter().any(|f| holds_entities(f.ty)),
        TypeKind::Array { element, .. } => holds_entities(element),
        TypeKind::List(list) => holds_entities(list.element),
        TypeKind::Option(option) => holds_entities(option.inner),
        TypeKind::Map(map) => holds_entities(map.key) || holds_entities(map.value),
        _ => false,
    }
}

/// Entity references read back from scene lines: by scene id, through the world's index.
struct IndexRefs<'a> {
    index: &'a HashMap<SceneId, Entity>,
}

impl ReadRefs for IndexRefs<'_> {
    fn entity(&mut self, label: &str) -> Result<Entity, String> {
        Ok(SceneId::parse(label)
            .and_then(|id| self.index.get(&id).copied())
            .unwrap_or(Entity::NULL))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct EntityState {
    components: Vec<ComponentRecord>,
    /// Parent and index among its children.
    parent: Option<(SceneId, u32)>,
}

impl EntityState {
    fn bytes(&self) -> usize {
        self.components
            .iter()
            .map(ComponentRecord::bytes)
            .sum::<usize>()
            + 32
    }
}

#[derive(Debug, Clone)]
struct Change {
    id: SceneId,
    before: Option<EntityState>,
    after: Option<EntityState>,
}

/// One undoable step.
#[derive(Debug, Clone)]
pub struct Transaction {
    /// New whenever the step's content changes (merges too): see [`History::head`].
    id: u64,
    name: String,
    merge: Option<u64>,
    changes: Vec<Change>,
    bytes: usize,
}

impl Transaction {
    /// The name shown in the history panel.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of entities it changed.
    pub fn entity_count(&self) -> usize {
        self.changes.len()
    }
}

struct Open {
    name: String,
    merge: Option<u64>,
    touched: Vec<(SceneId, Option<EntityState>)>,
    seen: HashSet<SceneId>,
}

/// Components never captured: the hierarchy links are restored from `parent`.
struct Skip {
    parent: Option<ComponentId>,
    children: Option<ComponentId>,
    scene_id: Option<ComponentId>,
}

impl Skip {
    fn of(world: &World) -> Self {
        Self {
            parent: world.component_id::<Parent>(),
            children: world.component_id::<Children>(),
            scene_id: world.component_id::<SceneId>(),
        }
    }

    fn skips(&self, id: ComponentId) -> bool {
        Some(id) == self.parent || Some(id) == self.children || Some(id) == self.scene_id
    }
}

/// Undo and redo stacks for one world.
pub struct History {
    undo: VecDeque<Transaction>,
    redo: Vec<Transaction>,
    open: Option<Open>,
    budget: usize,
    bytes: usize,
    /// The last transaction id given out.
    last_id: u64,
    codecs: Codecs,
    /// Components skipped because they are neither plain data nor saved by scenes.
    skipped_non_pod: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// Empty history with a 256 MiB budget.
    pub fn new() -> Self {
        Self::with_budget(256 << 20)
    }

    /// Empty history that drops its oldest steps beyond `bytes` of recorded state.
    pub fn with_budget(bytes: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            open: None,
            budget: bytes,
            bytes: 0,
            last_id: 0,
            codecs: Codecs::default(),
            skipped_non_pod: 0,
        }
    }

    /// Starts recording an action called `name`. Consecutive commits with the same
    /// `merge` key (for example one per drag gesture) become one step.
    ///
    /// An action that is already open is committed first.
    pub fn begin(&mut self, name: &str, merge: Option<u64>) {
        self.open = Some(Open {
            name: name.to_owned(),
            merge,
            touched: Vec::new(),
            seen: HashSet::new(),
        });
    }

    /// Records `entity`'s current state before the action changes it (once per action;
    /// later calls are free). Entities without a [`SceneId`] get one.
    pub fn touch(&mut self, world: &mut World, entity: Entity) {
        self.touch_with(world, entity, false);
    }

    /// Records that `entity` did not exist before the action (call right after spawning
    /// it).
    pub fn touch_new(&mut self, world: &mut World, entity: Entity) {
        self.touch_with(world, entity, true);
    }

    /// [`History::touch`] for `entity` and all its descendants (before despawning or
    /// moving a subtree).
    pub fn touch_subtree(&mut self, world: &mut World, entity: Entity) {
        let mut stack = vec![entity];
        while let Some(e) = stack.pop() {
            self.touch(world, e);
            stack.extend(world.children(e));
        }
    }

    /// Deletes `entity` and its descendants as part of the open action. Everything
    /// that refers to them is recorded too, so undo restores those references (they
    /// would otherwise point at the old, dead entities).
    pub fn delete_subtree(&mut self, world: &mut World, entity: Entity) {
        let mut doomed = HashSet::new();
        let mut stack = vec![entity];
        while let Some(e) = stack.pop() {
            doomed.insert(e);
            stack.extend(world.children(e));
        }
        for r in referrers(world, &mut self.codecs, &doomed) {
            self.touch(world, r);
        }
        self.touch_subtree(world, entity);
        world.despawn_recursive(entity);
    }

    /// Copies `entity` and its descendants as part of the open action. The copies get new
    /// scene ids; their references into the subtree point at the copies (references out
    /// of it stay); the copy of `entity` goes right after it among its siblings. Returns
    /// that copy, or `None` without an open action.
    pub fn duplicate_subtree(&mut self, world: &mut World, entity: Entity) -> Option<Entity> {
        if self.open.is_none() || !world.contains(entity) {
            return None;
        }
        // The subtree in pre-order, and a new scene id for each.
        let mut order = Vec::new();
        let mut stack = vec![entity];
        while let Some(e) = stack.pop() {
            order.push(e);
            let kids: Vec<Entity> = world.children(e).collect();
            stack.extend(kids.into_iter().rev());
        }
        let mut renamed: HashMap<SceneId, SceneId> = HashMap::with_capacity(order.len());
        for &e in &order {
            let id = match world.get::<SceneId>(e) {
                Some(&id) => id,
                None => {
                    let id = SceneId::new();
                    world.insert(e, id);
                    id
                }
            };
            renamed.insert(id, SceneId::new());
        }
        let new_id = |id: SceneId| renamed.get(&id).copied();
        let mut copies: Vec<(SceneId, EntityState)> = Vec::with_capacity(order.len());
        for (i, &e) in order.iter().enumerate() {
            let mut state = self.snapshot(world, e);
            for record in &mut state.components {
                match &mut record.value {
                    Value::Bytes { refs, .. } => {
                        for (_, target) in refs {
                            if let Some(copy) = target.and_then(new_id) {
                                *target = Some(copy);
                            }
                        }
                    }
                    Value::Lines(text) => {
                        let mut changed = text.to_string();
                        for (old, new) in &renamed {
                            let old = format!("@{old}");
                            if changed.contains(&old) {
                                changed = changed.replace(&old, &format!("@{new}"));
                            }
                        }
                        *text = changed.into_boxed_str();
                    }
                }
            }
            state.parent = match (i, state.parent) {
                // The copy of `entity`: right after it, under the same parent.
                (0, parent) => parent.map(|(p, index)| (p, index + 1)),
                (_, parent) => parent.map(|(p, index)| (new_id(p).unwrap_or(p), index)),
            };
            let id = world.get::<SceneId>(e).copied().and_then(new_id)?;
            copies.push((id, state));
        }
        let spawned: Vec<Entity> = copies.iter().map(|(id, _)| world.spawn((*id,))).collect();
        let index = id_index(world);
        let skip = Skip::of(world);
        for (&e, (_, state)) in spawned.iter().zip(&copies) {
            restore_components(world, &mut self.codecs, &skip, &index, e, state);
        }
        // Parents before children (pre-order), each at its recorded place.
        for (&e, (_, state)) in spawned.iter().zip(&copies) {
            if let Some((p, i)) = state.parent
                && let Some(&parent) = index.get(&p)
            {
                let _ = world.set_parent_at(e, Some(parent), i as usize);
            }
        }
        for &e in &spawned {
            self.touch_new(world, e);
        }
        spawned.first().copied()
    }

    fn touch_with(&mut self, world: &mut World, entity: Entity, new: bool) {
        if self.open.is_none() || !world.contains(entity) {
            return;
        }
        let id = match world.get::<SceneId>(entity) {
            Some(&id) => id,
            None => {
                let id = SceneId::new();
                world.insert(entity, id);
                id
            }
        };
        let seen = self.open.as_ref().is_some_and(|o| o.seen.contains(&id));
        if seen {
            return;
        }
        let before = if new {
            None
        } else {
            Some(self.snapshot(world, entity))
        };
        if let Some(open) = self.open.as_mut() {
            open.seen.insert(id);
            open.touched.push((id, before));
        }
    }

    fn snapshot(&mut self, world: &World, entity: Entity) -> EntityState {
        let skip = Skip::of(world);
        let mut components = Vec::new();
        let ids: Vec<ComponentId> = world
            .component_ids_of(entity)
            .map(<[ComponentId]>::to_vec)
            .unwrap_or_default();
        for cid in ids {
            if skip.skips(cid) {
                continue;
            }
            let Some(ptr) = world.get_raw(entity, cid) else {
                continue;
            };
            let reg = world.components();
            if !reg.is_pod(cid) {
                let Some(codec) = self.codecs.lines(world, cid) else {
                    self.skipped_non_pod += 1;
                    continue;
                };
                let mut text = String::new();
                let ids = |e: Entity| world.get::<SceneId>(e).copied();
                // SAFETY: `ptr` is the entity's live value of the component `codec.info`
                // describes.
                unsafe {
                    engine_scene::write_lines_with(
                        codec.info,
                        ptr.as_ptr().cast_const(),
                        &ids,
                        None,
                        &mut text,
                    );
                }
                components.push(ComponentRecord {
                    id: cid,
                    value: Value::Lines(text.into_boxed_str()),
                });
                continue;
            }
            let size = reg.layout(cid).map_or(0, |l| l.size());
            // SAFETY: `ptr` points to a live plain-data value of `size` bytes.
            let mut bytes: Box<[u8]> =
                unsafe { core::slice::from_raw_parts(ptr.as_ptr(), size) }.into();
            let mut refs = Vec::new();
            if let Some(info) = reg.type_info(cid) {
                let mut offsets = Vec::new();
                entity_offsets(info, 0, &mut offsets);
                for off in offsets {
                    let raw = &mut bytes[off..off + core::mem::size_of::<Entity>()];
                    // SAFETY: an `Entity` value at this offset of the component.
                    let target = unsafe { raw.as_ptr().cast::<Entity>().read_unaligned() };
                    raw.fill(0);
                    refs.push((off as u32, world.get::<SceneId>(target).copied()));
                }
            }
            components.push(ComponentRecord {
                id: cid,
                value: Value::Bytes { bytes, refs },
            });
        }
        let parent = world.parent(entity).and_then(|p| {
            let pid = *world.get::<SceneId>(p)?;
            Some((pid, world.child_index(entity).unwrap_or(0) as u32))
        });
        EntityState { components, parent }
    }

    /// Ends the action: records every touched entity's new state. Returns false (and
    /// records nothing) if nothing changed.
    pub fn commit(&mut self, world: &mut World) -> bool {
        let Some(open) = self.open.take() else {
            return false;
        };
        if open.touched.is_empty() {
            return false;
        }
        let index = id_index(world);
        let mut changes = Vec::with_capacity(open.touched.len());
        for (id, before) in open.touched {
            let after = index.get(&id).map(|&e| self.snapshot(world, e));
            if before != after {
                changes.push(Change { id, before, after });
            }
        }
        if changes.is_empty() {
            return false;
        }
        self.redo.clear();
        self.last_id += 1;
        if let Some(key) = open.merge
            && let Some(top) = self.undo.back_mut()
            && top.merge == Some(key)
        {
            top.id = self.last_id;
            for c in changes {
                if let Some(existing) = top.changes.iter_mut().find(|t| t.id == c.id) {
                    existing.after = c.after;
                } else {
                    top.changes.push(c);
                }
            }
            top.changes.retain(|c| c.before != c.after);
            let bytes = transaction_bytes(&top.changes);
            self.bytes = self.bytes - top.bytes + bytes;
            top.bytes = bytes;
            if top.changes.is_empty() {
                self.bytes -= top.bytes;
                self.undo.pop_back();
            }
        } else {
            let bytes = transaction_bytes(&changes);
            self.bytes += bytes;
            self.undo.push_back(Transaction {
                id: self.last_id,
                name: open.name,
                merge: open.merge,
                changes,
                bytes,
            });
        }
        while self.bytes > self.budget && self.undo.len() > 1 {
            if let Some(t) = self.undo.pop_front() {
                self.bytes -= t.bytes;
            }
        }
        true
    }

    /// Abandons the open action and puts the touched entities back as they were.
    pub fn cancel(&mut self, world: &mut World) {
        let Some(open) = self.open.take() else {
            return;
        };
        let changes: Vec<Change> = open
            .touched
            .into_iter()
            .map(|(id, before)| Change {
                id,
                before,
                after: None,
            })
            .collect();
        apply(world, &mut self.codecs, &changes, true);
    }

    /// Takes back the last step if it has merge key `merge` (a drag cancelled with
    /// Escape): its entities go back to their state before it, exactly, and the step is
    /// forgotten (nothing to redo). False if the last step is another one.
    pub fn revert(&mut self, world: &mut World, merge: u64) -> bool {
        self.open = None;
        if self.undo.back().is_none_or(|t| t.merge != Some(merge)) {
            return false;
        }
        let Some(t) = self.undo.pop_back() else {
            return false;
        };
        self.bytes -= t.bytes;
        apply(world, &mut self.codecs, &t.changes, true);
        true
    }

    /// Undoes the last step; returns its name.
    pub fn undo(&mut self, world: &mut World) -> Option<String> {
        engine_profiler::zone!("editor::undo");
        self.open = None;
        let t = self.undo.pop_back()?;
        self.bytes -= t.bytes;
        apply(world, &mut self.codecs, &t.changes, true);
        let name = t.name.clone();
        self.redo.push(t);
        Some(name)
    }

    /// Redoes the last undone step; returns its name.
    pub fn redo(&mut self, world: &mut World) -> Option<String> {
        engine_profiler::zone!("editor::redo");
        self.open = None;
        let t = self.redo.pop()?;
        apply(world, &mut self.codecs, &t.changes, false);
        let name = t.name.clone();
        self.bytes += t.bytes;
        self.undo.push_back(t);
        Some(name)
    }

    /// Whether there is a step to undo.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether there is a step to redo.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Undoable steps, oldest first (for the history panel).
    pub fn steps(&self) -> impl Iterator<Item = &Transaction> {
        self.undo.iter()
    }

    /// Redoable steps, next first.
    pub fn redo_steps(&self) -> impl Iterator<Item = &Transaction> {
        self.redo.iter().rev()
    }

    /// Which state the world is in, as far as the history knows: the id of the last
    /// step (0 when there is none). It changes with every commit, undo and redo, and
    /// comes back with the state it names, so an editor compares it with the head at its
    /// last save to know whether the scene has unsaved changes.
    pub fn head(&self) -> u64 {
        self.undo.back().map_or(0, |t| t.id)
    }

    /// Bytes of recorded state in the undo stack.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Components that could not be recorded: neither plain data nor saved by scenes
    /// (runtime state, which the systems that own it make again).
    pub fn skipped_non_pod(&self) -> usize {
        self.skipped_non_pod
    }
}

fn transaction_bytes(changes: &[Change]) -> usize {
    changes
        .iter()
        .map(|c| {
            c.before.as_ref().map_or(0, EntityState::bytes)
                + c.after.as_ref().map_or(0, EntityState::bytes)
        })
        .sum()
}

/// Byte offsets of `Entity` values inside a type.
fn entity_offsets(ty: &TypeInfo, base: usize, out: &mut Vec<usize>) {
    let e = <Entity as engine_reflect::Reflect>::TYPE_INFO;
    if ty.name == e.name && ty.size == e.size && ty.align == e.align {
        out.push(base);
        return;
    }
    match ty.kind {
        TypeKind::Struct(fields) => {
            for f in fields {
                entity_offsets(f.ty, base + f.offset, out);
            }
        }
        TypeKind::Array { element, len } => {
            for i in 0..len {
                entity_offsets(element, base + i * element.size, out);
            }
        }
        _ => {}
    }
}

/// Entities with a component that holds a reference to one of `targets`.
fn referrers(world: &mut World, codecs: &mut Codecs, targets: &HashSet<Entity>) -> Vec<Entity> {
    let mut offsets: HashMap<ComponentId, Vec<usize>> = HashMap::new();
    let mut q = QueryState::<Entity>::new(world);
    let all: Vec<Entity> = q.iter_readonly(world).collect();
    let mut out = Vec::new();
    for e in all {
        if targets.contains(&e) {
            continue;
        }
        let ids: Vec<ComponentId> = world
            .component_ids_of(e)
            .map(<[ComponentId]>::to_vec)
            .unwrap_or_default();
        let refers = ids.iter().any(|&cid| {
            let offs = offsets.entry(cid).or_insert_with(|| {
                let mut v = Vec::new();
                if world.components().is_pod(cid)
                    && let Some(info) = world.components().type_info(cid)
                {
                    entity_offsets(info, 0, &mut v);
                }
                v
            });
            let Some(ptr) = world.get_raw(e, cid) else {
                return false;
            };
            if let Some(codec) = codecs.lines(world, cid) {
                // Not plain data: its references are found by writing its lines.
                if !codec.entities {
                    return false;
                }
                let hit = Cell::new(false);
                let probe = |t: Entity| {
                    if targets.contains(&t) {
                        hit.set(true);
                    }
                    None
                };
                let mut scratch = String::new();
                // SAFETY: the entity's live value of the component `codec.info` describes.
                unsafe {
                    engine_scene::write_lines_with(
                        codec.info,
                        ptr.as_ptr().cast_const(),
                        &probe,
                        None,
                        &mut scratch,
                    );
                }
                return hit.get();
            }
            offs.iter().any(|&o| {
                // SAFETY: an `Entity` value at this offset of a live component.
                let target = unsafe { ptr.as_ptr().add(o).cast::<Entity>().read_unaligned() };
                targets.contains(&target)
            })
        });
        if refers {
            out.push(e);
        }
    }
    out
}

fn id_index(world: &mut World) -> HashMap<SceneId, Entity> {
    let mut q = QueryState::<(Entity, &SceneId)>::new(world);
    q.iter_readonly(world).map(|(e, &id)| (id, e)).collect()
}

/// Puts every changed entity into its `before` (`undo`) or `after` state.
fn apply(world: &mut World, codecs: &mut Codecs, changes: &[Change], undo: bool) {
    let mut index = id_index(world);
    fn pick(c: &Change, undo: bool) -> &Option<EntityState> {
        if undo { &c.before } else { &c.after }
    }
    let target = |c| pick(c, undo);
    // 1. Existence: spawn what must exist, despawn what must not (detached first so
    //    hierarchy lists stay consistent).
    for c in changes {
        match (target(c), index.get(&c.id).copied()) {
            (Some(_), None) => {
                let e = world.spawn((c.id,));
                index.insert(c.id, e);
            }
            (None, Some(e)) => {
                let kids: Vec<Entity> = world.children(e).collect();
                for k in kids {
                    let _ = world.set_parent(k, None);
                }
                let _ = world.set_parent(e, None);
                world.despawn(e);
                index.remove(&c.id);
            }
            _ => {}
        }
    }
    // 2. Components, with entity references resolved through the index.
    let skip = Skip::of(world);
    for c in changes {
        let (Some(state), Some(&e)) = (target(c), index.get(&c.id)) else {
            continue;
        };
        restore_components(world, codecs, &skip, &index, e, state);
    }
    // 3. Hierarchy: detach everything that moves, then re-attach by ascending index so
    //    each insertion lands where it was recorded.
    let mut links: Vec<(Entity, Option<(Entity, u32)>)> = Vec::new();
    for c in changes {
        let (Some(state), Some(&e)) = (target(c), index.get(&c.id)) else {
            continue;
        };
        let parent = state
            .parent
            .and_then(|(pid, i)| index.get(&pid).map(|&p| (p, i)));
        links.push((e, parent));
    }
    for &(e, _) in &links {
        let _ = world.set_parent(e, None);
    }
    links.sort_by_key(|&(_, p)| p.map_or(0, |(_, i)| i));
    for (e, p) in links {
        if let Some((parent, i)) = p {
            let _ = world.set_parent_at(e, Some(parent), i as usize);
        }
    }
}

/// Gives `e` exactly the recorded components of `state` (of those undo records), entity
/// references resolved through `index`.
fn restore_components(
    world: &mut World,
    codecs: &mut Codecs,
    skip: &Skip,
    index: &HashMap<SceneId, Entity>,
    e: Entity,
    state: &EntityState,
) {
    let current: Vec<ComponentId> = world
        .component_ids_of(e)
        .map(<[ComponentId]>::to_vec)
        .unwrap_or_default();
    for cid in current {
        if !skip.skips(cid)
            && codecs.records(world, cid)
            && !state.components.iter().any(|r| r.id == cid)
        {
            world.remove_raw(e, cid);
        }
    }
    for r in &state.components {
        let (bytes, refs) = match &r.value {
            Value::Bytes { bytes, refs } => (bytes, refs),
            Value::Lines(text) => {
                restore_lines(world, codecs, index, e, r.id, text);
                continue;
            }
        };
        let mut bytes = bytes.to_vec();
        for &(off, target) in refs {
            let entity = target
                .and_then(|id| index.get(&id).copied())
                .unwrap_or(Entity::NULL);
            let off = off as usize;
            // SAFETY: the offset holds an `Entity` inside the value.
            unsafe {
                bytes[off..]
                    .as_mut_ptr()
                    .cast::<Entity>()
                    .write_unaligned(entity);
            }
        }
        // Keep the copy aligned for the component: copy into an aligned buffer.
        let layout = world
            .components()
            .layout(r.id)
            .unwrap_or(core::alloc::Layout::new::<u8>());
        let mut aligned = AlignedBuf::new(layout);
        aligned.bytes_mut()[..bytes.len()].copy_from_slice(&bytes);
        // SAFETY: plain-data bytes of this component type, aligned.
        unsafe { world.insert_raw(e, r.id, aligned.ptr()) };
    }
}

/// Puts back a component kept as scene lines: its default, the lines read into it, the
/// value moved into the world.
fn restore_lines(
    world: &mut World,
    codecs: &mut Codecs,
    index: &HashMap<SceneId, Entity>,
    entity: Entity,
    id: ComponentId,
    text: &str,
) {
    let (Some(codec), Some(layout)) = (codecs.lines(world, id), world.components().layout(id))
    else {
        return;
    };
    let mut value = AlignedBuf::new(layout);
    let mut refs = IndexRefs { index };
    // SAFETY: the buffer is room for one value of the component (its size and alignment);
    // the default writer makes it a valid value, reading lines keeps it valid (also when a
    // line fails), and `insert_raw` moves it into the world, leaving the buffer's memory
    // to be freed without dropping the value.
    unsafe {
        (codec.default)(value.ptr_mut());
        if let Err(e) = engine_scene::read_lines(codec.info, value.ptr_mut(), text, &mut refs) {
            log::warn!(target: "engine_editor::undo", "`{}`: {e}", codec.info.name);
        }
        let moved = world.insert_raw(entity, id, value.ptr());
        debug_assert!(moved, "restored entities are alive");
    }
}

/// A heap buffer with a component's alignment.
struct AlignedBuf {
    ptr: *mut u8,
    layout: core::alloc::Layout,
}

impl AlignedBuf {
    fn new(layout: core::alloc::Layout) -> Self {
        let layout = core::alloc::Layout::from_size_align(layout.size().max(1), layout.align())
            .unwrap_or(core::alloc::Layout::new::<u8>());
        // SAFETY: non-zero size.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `layout.size()` initialized (zeroed) bytes owned by this buffer.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }

    fn ptr(&self) -> *const u8 {
        self.ptr
    }

    fn ptr_mut(&mut self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}
