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

use engine_ecs::{Children, ComponentId, Entity, Parent, QueryState, World};
use engine_reflect::{TypeInfo, TypeKind};
use engine_scene::text::ReadRefs;
use engine_scene::{AssetId, SceneId};
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

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
#[derive(Clone)]
struct LinesCodec {
    info: &'static TypeInfo,
    default: std::sync::Arc<engine_ecs::DefaultValue>,
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
        self.lines
            .entry(id)
            .or_insert_with(|| {
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
                // A program type's `Default`, or a script's bytes and texts.
                let default = engine_ecs::DefaultValue::of(info.name, Some(layout))?;
                Some(LinesCodec {
                    info,
                    default: std::sync::Arc::new(default),
                    entities: holds_entities(info),
                })
            })
            .clone()
    }

    /// Whether undo records the component: plain data, or saved by scenes.
    fn records(&mut self, world: &World, id: ComponentId) -> bool {
        world.components().is_pod(id) || self.lines(world, id).is_some()
    }

    /// Whether scenes save the component: reflected, not runtime state (`hidden`, `skip`),
    /// recorded by undo.
    fn saved(&mut self, world: &World, id: ComponentId) -> bool {
        let Some(info) = world.components().type_info(id) else {
            return false;
        };
        !info.attrs.hidden && !info.attrs.skip && self.records(world, id)
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

/// A file's text before and after a step (a material file edited in the inspector, ADR
/// 0120's E10-1c): undo and redo hand it back to the editor ([`History::take_files`]).
#[derive(Debug, Clone, PartialEq)]
struct FileChange {
    file: AssetId,
    before: Arc<str>,
    after: Arc<str>,
}

/// One undoable step.
#[derive(Debug, Clone)]
pub struct Transaction {
    /// New whenever the step's content changes (merges too): see [`History::head`].
    id: u64,
    name: String,
    merge: Option<u64>,
    changes: Vec<Change>,
    /// The files it changed ([`History::change_file`]).
    files: Vec<FileChange>,
    bytes: usize,
    /// The document it was made in ([`History::set_document`]).
    doc: u32,
}

impl Transaction {
    /// The name shown in the history panel.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The document it was made in ([`History::set_document`]).
    pub fn doc(&self) -> u32 {
        self.doc
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
    files: Vec<FileChange>,
}

/// Entity `e`'s children of the scene: not those the engine made for it
/// (`engine_runtime::Derived`, ADR 0115: a model's further materials), which undo neither
/// records nor copies; the engine makes them again for what undo puts back.
fn scene_children(world: &World, e: Entity) -> impl Iterator<Item = Entity> + '_ {
    world
        .children(e)
        .filter(move |&c| world.get::<engine_runtime::Derived>(c).is_none())
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
    /// While playing: the last step made before Play, below which undo does not go.
    floor: Option<u64>,
    /// The document steps are made in now: the editor's scene in the world, of those open
    /// in tabs (ADR 0107 §4: one history across tabs).
    doc: u32,
    /// Each document's newest step dropped for the budget: its head while none of its
    /// steps is left ([`Self::head_of`]).
    dropped: Vec<(u32, u64)>,
    /// Files' texts the last undo, redo, revert or cancel put back ([`Self::take_files`]).
    put_back: Vec<(AssetId, Arc<str>)>,
}

/// The world as it was, kept to put it back (ADR 0107 E3: Play runs on the scene's world,
/// and Stop puts it back): the scene's entities' recorded states, as an undo step keeps
/// them, and every entity alive with the types of its components.
pub struct WorldSnapshot {
    states: Vec<Change>,
    alive: HashMap<Entity, Box<[ComponentId]>>,
    /// The named scene entities' names, for what a list says of those despawned since.
    names: HashMap<SceneId, String>,
}

impl WorldSnapshot {
    /// The scene's entities it holds (those with a scene id).
    pub fn scene_entities(&self) -> usize {
        self.states.len()
    }

    /// [`History::relayout`] for what the snapshot holds.
    pub fn relayout(&mut self, r: &engine_ecs::Relayout) {
        relayout_changes(&mut self.states, r);
        for types in self.alive.values_mut() {
            for id in types.iter_mut() {
                if *id == r.old {
                    *id = r.new;
                }
            }
        }
    }

    /// Bytes of recorded state.
    pub fn bytes(&self) -> usize {
        transaction_bytes(&self.states, &[])
    }

    /// The name scene entity `id` had then.
    pub fn name_of(&self, id: SceneId) -> Option<&str> {
        self.names.get(&id).map(String::as_str)
    }
}

/// What changed since a [`WorldSnapshot`] in what scenes save (ADR 0107 E3: the changes
/// made while playing, to keep).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayChange {
    /// A scene entity whose saved components or place differ: their names (`Parent` for
    /// its place).
    Changed {
        /// The entity.
        entity: Entity,
        /// What differs.
        components: Vec<&'static str>,
    },
    /// An entity made since (by the game, or by a scene it loaded).
    Made {
        /// The entity.
        entity: Entity,
    },
    /// A scene entity despawned since.
    Gone {
        /// Its scene id.
        id: SceneId,
    },
}

/// The states of entities as they were taken, to give the world again with
/// [`History::keep`]: what changes kept from Play bring into the scene.
#[derive(Default)]
pub struct Kept {
    /// By scene id: the saved components and place, or `None` to despawn.
    states: Vec<(SceneId, Option<EntityState>)>,
}

impl Kept {
    /// How many entities it changes.
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Whether it changes none.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }
}

/// The history as Play found it ([`History::begin_play`]): its last step and the steps to
/// redo, given back at Stop.
pub struct PlayMark {
    head: u64,
    redo: Vec<Transaction>,
}

impl PlayMark {
    /// [`History::relayout`] for the steps to redo it gives back.
    pub fn relayout(&mut self, r: &engine_ecs::Relayout) {
        for t in &mut self.redo {
            relayout_changes(&mut t.changes, r);
        }
    }
}

/// The records of component `r.old` in `changes` rewritten as `r.new`'s.
fn relayout_changes(changes: &mut [Change], r: &engine_ecs::Relayout) {
    for change in changes {
        for state in [&mut change.before, &mut change.after]
            .into_iter()
            .flatten()
        {
            relayout_state(state, r);
        }
    }
}

fn relayout_state(state: &mut EntityState, r: &engine_ecs::Relayout) {
    for record in &mut state.components {
        if record.id != r.old {
            continue;
        }
        record.id = r.new;
        let Value::Bytes { bytes, refs } = &mut record.value else {
            continue;
        };
        let mut moved = r.apply(bytes);
        // Entity slots hold zeros here and their scene ids apart: a moved one keeps its
        // reference, a new one refers to no entity.
        let mut kept = Vec::with_capacity(r.entities.len());
        for &at in &r.entities {
            if let Some(slot) = moved.get_mut(at..at + 8) {
                slot.fill(0);
            }
            let target = r
                .moves
                .iter()
                .find(|m| m.entity && m.to == at)
                .and_then(|m| refs.iter().find(|&&(off, _)| off as usize == m.from))
                .and_then(|&(_, target)| target);
            kept.push((at as u32, target));
        }
        *bytes = moved.into_boxed_slice();
        *refs = kept;
    }
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
            floor: None,
            doc: 0,
            dropped: Vec::new(),
            put_back: Vec::new(),
        }
    }

    /// Steps from now on are document `doc`'s: the scene the editor shows (its tabs share
    /// one history; undoing another document's step is the editor's to switch to it first,
    /// [`Self::undo_document`]).
    pub fn set_document(&mut self, doc: u32) {
        self.doc = doc;
    }

    /// The document whose step undo takes next, if any.
    pub fn undo_document(&self) -> Option<u32> {
        self.undo
            .back()
            .filter(|t| self.floor.is_none_or(|floor| t.id > floor))
            .map(|t| t.doc)
    }

    /// The document whose step redo takes next, if any.
    pub fn redo_document(&self) -> Option<u32> {
        self.redo.last().map(|t| t.doc)
    }

    /// [`Self::head`] for document `doc` alone: the id of its last step (0 when it has
    /// none), so steps in other documents leave its saved state as it is.
    pub fn head_of(&self, doc: u32) -> u64 {
        self.undo
            .iter()
            .rev()
            .find(|t| t.doc == doc)
            .map(|t| t.id)
            .or_else(|| {
                self.dropped
                    .iter()
                    .find(|&&(d, _)| d == doc)
                    .map(|&(_, id)| id)
            })
            .unwrap_or(0)
    }

    /// Document `doc` is closed: its steps go, to undo and to redo.
    pub fn forget(&mut self, doc: u32) {
        let mut freed = 0;
        self.undo.retain(|t| {
            let keep = t.doc != doc;
            if !keep {
                freed += t.bytes;
            }
            keep
        });
        self.bytes -= freed;
        self.redo.retain(|t| t.doc != doc);
        self.dropped.retain(|&(d, _)| d != doc);
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
            files: Vec::new(),
        });
    }

    /// Records that the open action changed file `file`'s text from `before` to `after` (a
    /// material file edited in the inspector, ADR 0120's E10-1c), the file being the
    /// editor's to hold: undo hands `before` back and redo `after` ([`Self::take_files`]).
    /// Nothing without an open action.
    pub fn change_file(&mut self, file: AssetId, before: &str, after: &str) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        match open.files.iter_mut().find(|f| f.file == file) {
            Some(f) => f.after = Arc::from(after),
            None => open.files.push(FileChange {
                file,
                before: Arc::from(before),
                after: Arc::from(after),
            }),
        }
    }

    /// The files' texts the last undo, redo, revert or cancel put back, in order, for the
    /// editor to give to what holds them; taken once.
    pub fn take_files(&mut self) -> Vec<(AssetId, Arc<str>)> {
        core::mem::take(&mut self.put_back)
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
    /// moving a subtree), those the engine made for them left out.
    pub fn touch_subtree(&mut self, world: &mut World, entity: Entity) {
        let mut stack = vec![entity];
        while let Some(e) = stack.pop() {
            self.touch(world, e);
            stack.extend(scene_children(world, e));
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
            stack.extend(scene_children(world, e));
        }
        for r in referrers(world, &mut self.codecs, &doomed) {
            // A part the engine made for a doomed entity refers to it as its parent: it
            // goes with it, unrecorded.
            if world.get::<engine_runtime::Derived>(r).is_none() {
                self.touch(world, r);
            }
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
            let kids: Vec<Entity> = scene_children(world, e).collect();
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

    /// Gives entities new scene ids as part of the open action (a subtree made a prefab's
    /// instance, E5): to the history each old one is gone and each new one made, and
    /// everything that refers to them is recorded too, so undo puts the old ids back with
    /// the references to them.
    pub fn change_ids(&mut self, world: &mut World, changes: &[(Entity, SceneId)]) {
        if self.open.is_none() {
            return;
        }
        let targets: HashSet<Entity> = changes.iter().map(|&(e, _)| e).collect();
        for r in referrers(world, &mut self.codecs, &targets) {
            self.touch(world, r);
        }
        for &(e, _) in changes {
            self.touch(world, e);
        }
        for &(e, id) in changes {
            world.insert(e, id);
        }
        for &(e, _) in changes {
            self.touch_new(world, e);
        }
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
        if open.touched.is_empty() && open.files.is_empty() {
            return false;
        }
        let mut changes = Vec::with_capacity(open.touched.len());
        if !open.touched.is_empty() {
            let index = id_index(world);
            for (id, before) in open.touched {
                let after = index.get(&id).map(|&e| self.snapshot(world, e));
                if before != after {
                    changes.push(Change { id, before, after });
                }
            }
        }
        let mut files = open.files;
        files.retain(|f| f.before != f.after);
        if changes.is_empty() && files.is_empty() {
            return false;
        }
        self.redo.clear();
        self.last_id += 1;
        if let Some(key) = open.merge
            && let Some(top) = self.undo.back_mut()
            && top.merge == Some(key)
            && top.doc == self.doc
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
            for f in files {
                match top.files.iter_mut().find(|t| t.file == f.file) {
                    Some(existing) => existing.after = f.after,
                    None => top.files.push(f),
                }
            }
            top.files.retain(|f| f.before != f.after);
            let bytes = transaction_bytes(&top.changes, &top.files);
            self.bytes = self.bytes - top.bytes + bytes;
            top.bytes = bytes;
            if top.changes.is_empty() && top.files.is_empty() {
                self.bytes -= top.bytes;
                self.undo.pop_back();
            }
        } else {
            let bytes = transaction_bytes(&changes, &files);
            self.bytes += bytes;
            self.undo.push_back(Transaction {
                id: self.last_id,
                name: open.name,
                merge: open.merge,
                changes,
                files,
                bytes,
                doc: self.doc,
            });
        }
        while self.bytes > self.budget && self.undo.len() > 1 {
            if let Some(t) = self.undo.pop_front() {
                self.bytes -= t.bytes;
                match self.dropped.iter_mut().find(|(d, _)| *d == t.doc) {
                    Some(entry) => entry.1 = t.id,
                    None => self.dropped.push((t.doc, t.id)),
                }
            }
        }
        true
    }

    /// Abandons the open action and puts the touched entities back as they were.
    pub fn cancel(&mut self, world: &mut World) {
        let Some(open) = self.open.take() else {
            return;
        };
        put_back(&mut self.put_back, &open.files, true);
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
        put_back(&mut self.put_back, &t.files, true);
        true
    }

    /// Undoes the last step; returns its name. While playing, not the steps made before
    /// Play ([`Self::begin_play`]).
    pub fn undo(&mut self, world: &mut World) -> Option<String> {
        engine_profiler::zone!("editor::undo");
        self.open = None;
        if let Some(floor) = self.floor
            && self.undo.back().is_some_and(|t| t.id <= floor)
        {
            return None;
        }
        let t = self.undo.pop_back()?;
        self.bytes -= t.bytes;
        apply(world, &mut self.codecs, &t.changes, true);
        put_back(&mut self.put_back, &t.files, true);
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
        put_back(&mut self.put_back, &t.files, false);
        let name = t.name.clone();
        self.bytes += t.bytes;
        self.undo.push_back(t);
        Some(name)
    }

    /// Whether there is a step to undo.
    pub fn can_undo(&self) -> bool {
        self.undo
            .back()
            .is_some_and(|t| self.floor.is_none_or(|floor| t.id > floor))
    }

    /// Every record of component `r.old` rewritten as `r.new`'s, its values moved by name
    /// ([`engine_ecs::Relayout`]: a script's props changed, ADR 0114 E7-4b): the steps done
    /// and undone, and the one open. Undo then puts back values of the layout the world
    /// holds now.
    pub fn relayout(&mut self, r: &engine_ecs::Relayout) {
        for t in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            relayout_changes(&mut t.changes, r);
        }
        if let Some(open) = &mut self.open {
            for state in open.touched.iter_mut().filter_map(|(_, s)| s.as_mut()) {
                relayout_state(state, r);
            }
        }
    }

    /// Play begins (ADR 0107 E3): the steps made while playing are the play world's, so
    /// undo stops at the last step before it and the steps to redo wait for Stop.
    pub fn begin_play(&mut self) -> PlayMark {
        self.open = None;
        let head = self.head();
        self.floor = Some(head);
        PlayMark {
            head,
            redo: core::mem::take(&mut self.redo),
        }
    }

    /// Play ends: the steps made while playing go (the world they were made in is gone),
    /// and the history is as Play found it.
    pub fn end_play(&mut self, mark: PlayMark) {
        self.open = None;
        self.floor = None;
        while self.undo.back().is_some_and(|t| t.id > mark.head) {
            if let Some(t) = self.undo.pop_back() {
                self.bytes -= t.bytes;
            }
        }
        self.redo = mark.redo;
    }

    /// The world as it is now, to put back with [`Self::restore_world`]: every scene
    /// entity's state as undo records it, and every entity alive with its components'
    /// types (entities without a scene id, the editor's own, are kept by handle).
    pub fn snapshot_world(&mut self, world: &mut World) -> WorldSnapshot {
        engine_profiler::zone!("editor::snapshot_world");
        let mut q = QueryState::<Entity>::new(world);
        let all: Vec<Entity> = q.iter_readonly(world).collect();
        let mut states = Vec::new();
        let mut alive = HashMap::with_capacity(all.len());
        let mut names = HashMap::new();
        for e in all {
            let types: Box<[ComponentId]> = world
                .component_ids_of(e)
                .map_or_else(Box::default, Box::from);
            alive.insert(e, types);
            if let Some(&id) = world.get::<SceneId>(e) {
                if let Some(name) = world.get::<engine_scene::Name>(e) {
                    names.insert(id, name.as_str().to_owned());
                }
                let before = Some(self.snapshot(world, e));
                states.push(Change {
                    id,
                    before,
                    after: None,
                });
            }
        }
        WorldSnapshot {
            states,
            alive,
            names,
        }
    }

    /// What changed since `snapshot` in what scenes save: the scene's entities whose
    /// saved components or place differ, the entities made since, and the scene's
    /// entities despawned since.
    pub fn changes_since(
        &mut self,
        world: &mut World,
        snapshot: &WorldSnapshot,
    ) -> Vec<PlayChange> {
        let index = id_index(world);
        let mut out = Vec::new();
        for c in &snapshot.states {
            let Some(before) = &c.before else {
                continue;
            };
            let Some(&e) = index.get(&c.id) else {
                out.push(PlayChange::Gone { id: c.id });
                continue;
            };
            let now = self.snapshot(world, e);
            let mut components: Vec<&'static str> = Vec::new();
            let mut differ = |cid: ComponentId| {
                if let Some(info) = world.components().type_info(cid)
                    && !components.contains(&info.name)
                {
                    components.push(info.name);
                }
            };
            for r in &now.components {
                let then = before.components.iter().find(|b| b.id == r.id);
                if then != Some(r) && self.codecs.saved(world, r.id) {
                    differ(r.id);
                }
            }
            for b in &before.components {
                if !now.components.iter().any(|r| r.id == b.id) && self.codecs.saved(world, b.id) {
                    differ(b.id);
                }
            }
            if now.parent != before.parent {
                components.push("Parent");
            }
            if !components.is_empty() {
                out.push(PlayChange::Changed {
                    entity: e,
                    components,
                });
            }
        }
        let mut q = QueryState::<Entity>::new(world);
        out.extend(
            q.iter_readonly(world)
                .filter(|e| !snapshot.alive.contains_key(e))
                .map(|entity| PlayChange::Made { entity }),
        );
        out
    }

    /// The saved components and places of `entities` as they are now, and the scene
    /// entities `gone` (despawned), to give the world again with [`Self::keep`]. Entities
    /// without a scene id (made while playing) get a new one.
    pub fn capture_kept(
        &mut self,
        world: &mut World,
        entities: &[Entity],
        gone: &[SceneId],
    ) -> Kept {
        let mut states = Vec::with_capacity(entities.len() + gone.len());
        for &e in entities {
            if !world.contains(e) {
                continue;
            }
            let id = match world.get::<SceneId>(e) {
                Some(&id) => id,
                None => {
                    let id = SceneId::new();
                    world.insert(e, id);
                    id
                }
            };
            let mut state = self.snapshot(world, e);
            state.components.retain(|r| self.codecs.saved(world, r.id));
            states.push((id, Some(state)));
        }
        states.extend(gone.iter().map(|&id| (id, None)));
        Kept { states }
    }

    /// Gives the world the states `kept` holds, in the open action (one undo step): each
    /// entity its kept saved components and place (its other components stay), those not
    /// in the world spawned with their scene ids, those kept gone despawned.
    pub fn keep(&mut self, world: &mut World, kept: &Kept) {
        if self.open.is_none() {
            return;
        }
        let mut index = id_index(world);
        let mut made = Vec::new();
        for (id, state) in &kept.states {
            match (state, index.get(id).copied()) {
                (Some(_), Some(e)) => self.touch(world, e),
                (Some(_), None) => {
                    let e = world.spawn((*id,));
                    index.insert(*id, e);
                    made.push(e);
                }
                (None, Some(e)) => self.delete_subtree(world, e),
                (None, None) => {}
            }
        }
        let index = id_index(world);
        let skip = Skip::of(world);
        for (id, state) in &kept.states {
            let (Some(state), Some(&e)) = (state, index.get(id)) else {
                continue;
            };
            // Only what scenes save: the entity's runtime components stay.
            let current: Vec<ComponentId> = world
                .component_ids_of(e)
                .map(<[ComponentId]>::to_vec)
                .unwrap_or_default();
            for cid in current {
                if !skip.skips(cid)
                    && self.codecs.saved(world, cid)
                    && !state.components.iter().any(|r| r.id == cid)
                {
                    world.remove_raw(e, cid);
                }
            }
            let partial = EntityState {
                components: state.components.clone(),
                parent: None,
            };
            restore_kept(world, &mut self.codecs, &index, e, &partial);
            let parent = state
                .parent
                .and_then(|(pid, i)| index.get(&pid).map(|&p| (p, i)));
            match parent {
                Some((p, i)) if world.parent(e) != Some(p) => {
                    let _ = world.set_parent_at(e, Some(p), i as usize);
                }
                None if world.parent(e).is_some() => {
                    let _ = world.set_parent(e, None);
                }
                _ => {}
            }
        }
        for e in made {
            self.touch_new(world, e);
        }
    }

    /// Puts the world back as `snapshot` found it: entities made since go; components
    /// of types an entity did not have are taken off; the scene's entities get their
    /// recorded state and place again, those despawned since coming back as new entities
    /// with their scene ids. The open action, if any, is dropped.
    pub fn restore_world(&mut self, world: &mut World, snapshot: &WorldSnapshot) {
        engine_profiler::zone!("editor::restore_world");
        self.open = None;
        let mut q = QueryState::<Entity>::new(world);
        let now: Vec<Entity> = q.iter_readonly(world).collect();
        // Entities made since: gone, detached first so the hierarchy's lists stay right.
        for &e in &now {
            if snapshot.alive.contains_key(&e) {
                continue;
            }
            let kids: Vec<Entity> = world.children(e).collect();
            for k in kids {
                let _ = world.set_parent(k, None);
            }
            let _ = world.set_parent(e, None);
            world.despawn(e);
        }
        // Components of types added since (the game's own, runtime state) go; the links
        // of the hierarchy are put back below.
        let skip = Skip::of(world);
        for (&e, types) in &snapshot.alive {
            let Some(current) = world.component_ids_of(e).map(<[ComponentId]>::to_vec) else {
                continue;
            };
            for cid in current {
                if !types.contains(&cid) && Some(cid) != skip.parent && Some(cid) != skip.children {
                    world.remove_raw(e, cid);
                }
            }
        }
        apply(world, &mut self.codecs, &snapshot.states, true);
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

fn transaction_bytes(changes: &[Change], files: &[FileChange]) -> usize {
    changes
        .iter()
        .map(|c| {
            c.before.as_ref().map_or(0, EntityState::bytes)
                + c.after.as_ref().map_or(0, EntityState::bytes)
        })
        .sum::<usize>()
        + files
            .iter()
            .map(|f| f.before.len() + f.after.len() + 32)
            .sum::<usize>()
}

/// Files' texts a step puts back into `out`: as before it (`undo`) or after it.
fn put_back(out: &mut Vec<(AssetId, Arc<str>)>, files: &[FileChange], undo: bool) {
    for f in files {
        let text = if undo { &f.before } else { &f.after };
        out.push((f.file, Arc::clone(text)));
    }
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

/// Sets the components of `state` on `e` (and no others), entity references resolved
/// through `index`: kept changes, over what the entity has.
fn restore_kept(
    world: &mut World,
    codecs: &mut Codecs,
    index: &HashMap<SceneId, Entity>,
    e: Entity,
    state: &EntityState,
) {
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
        codec.default.write(value.ptr_mut());
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
