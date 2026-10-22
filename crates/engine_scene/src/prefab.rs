//! Prefab instances (ADR 0108's E5, "any scene is a prefab"): a scene placed in another by
//! an `[instance]` block, the values of its entities overridden by `[override]` blocks.
//!
//! ```text
//! [instance 7d2e0f1a9b3c4d5e8f6a7b1c2d3e4f50]
//! prefab = asset("props/crate.scene", 9b41d07c2e8a4f6b8d3c1a5e7f902b64)
//! parent = 6c1f0a3e9d2b47c88e51f0d4a7b3c912
//! Transform.translation = (3, 0, 1)
//! remove = @0b12e4f8c3a94d5e9f7a6b1c2d3e4f50
//!
//! [override 7d2e0f1a9b3c4d5e8f6a7b1c2d3e4f50/a13f5c2e8b7d4e1f9a6b3c0d2e5f8a71]
//! Health.current = 50
//! remove = Light
//! ```
//!
//! - **Ids inside an instance** are derived ([`SceneId::derive`]) from the instance's and
//!   the entity's own in the prefab's file, so they are the same at every load:
//!   references into an instance (`@7d2e…/a13f…`) and undo find them. A path goes through
//!   instances inside instances (`7d2e…/0b12…/a13f…`, [`SceneId::of_path`]).
//! - **A prefab of one root:** that root is the instance's entity and takes the instance's
//!   id; the instance block's lines override its values. A prefab of several roots gets an
//!   entity of its own with the instance's id, the prefab's roots its children.
//! - **Overrides apply over the prefab's values:** a component's lines come after the
//!   prefab's (`Component = {}` among them starts the component from its default again).
//!   `remove = Component` takes a component away; `remove = @<entity>` in an instance
//!   block leaves one of the prefab's entities out, with its children.
//! - **What cannot be applied is kept:** an instance whose prefab cannot be read (or that
//!   would place itself), and an override whose target the prefab no longer has, are
//!   warnings, and their blocks are written back as they were.

use crate::{
    AssetId, AssetRef, Block, Kind, LoadError, Parsed, SceneId, SceneRegistry, err, scene_header,
    text, whole_value,
};
use engine_ecs::bytemuck;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

/// The root of a prefab instance: the scene it places. Its `[instance]` block names it;
/// it is not written as a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct PrefabInstance {
    /// The prefab, a scene file.
    pub prefab: AssetRef,
    /// 1 when the prefab could not be read: nothing of it placed, the instance's blocks
    /// kept as written.
    missing: u64,
}

// SAFETY: an `AssetRef` (three `u64`s) and a `u64`, `repr(C)`, no padding; every bit
// pattern is valid.
unsafe impl bytemuck::Zeroable for PrefabInstance {}
// SAFETY: as above; `Copy` and `repr(C)`.
unsafe impl bytemuck::Pod for PrefabInstance {}
engine_ecs::pod_component!(PrefabInstance);

impl PrefabInstance {
    /// An instance of `prefab`.
    pub const fn new(prefab: AssetRef) -> Self {
        Self { prefab, missing: 0 }
    }

    /// Whether its prefab could not be read when it was loaded (nothing of it placed).
    pub const fn is_missing(&self) -> bool {
        self.missing != 0
    }
}

/// An entity a prefab instance placed: the instance's root, by its scene id, and the
/// entity's id in the prefab's own file, which paths to it are written with. Its scene
/// id is derived from both ([`SceneId::derive`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct PrefabPart {
    /// The instance's root.
    pub instance: SceneId,
    /// Its id in the prefab's file.
    pub local: SceneId,
}

// SAFETY: two `SceneId`s (four `u64`s), `repr(C)`, no padding; every bit pattern is valid.
unsafe impl bytemuck::Zeroable for PrefabPart {}
// SAFETY: as above; `Copy` and `repr(C)`.
unsafe impl bytemuck::Pod for PrefabPart {}
engine_ecs::pod_component!(PrefabPart);

/// What of an instance could not be applied, as it was written, kept until the user drops
/// it (ADR 0108): lines of its own block (`remove = @…` of entities the prefab no longer
/// has; all of them while its prefab cannot be read), and override blocks written after
/// the instance's own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeptBlocks {
    /// Lines of the instance's block.
    pub lines: String,
    /// Whole blocks.
    pub blocks: String,
}

impl engine_ecs::Component for KeptBlocks {}

/// Where the prefabs scenes place come from: their text, by asset reference.
pub trait Prefabs {
    /// Scene `prefab`'s text (`path` is what scenes write beside its id), or why it
    /// cannot be had.
    ///
    /// # Errors
    ///
    /// The file cannot be found or read, or is not that scene.
    fn text(&self, prefab: AssetId, path: &str) -> Result<String, String>;
}

/// Prefabs read from the files under a folder, by the paths scenes write, each file's
/// header checked against the id.
#[derive(Debug, Clone, Copy)]
pub struct PrefabFiles<'a>(pub &'a Path);

impl Prefabs for PrefabFiles<'_> {
    fn text(&self, prefab: AssetId, path: &str) -> Result<String, String> {
        if path.is_empty() {
            return Err(format!("no path is known for the prefab {prefab}"));
        }
        let text =
            std::fs::read_to_string(self.0.join(path)).map_err(|e| format!("{path}: {e}"))?;
        match scene_header(&text) {
            Ok((_, Some(id))) if id != prefab => {
                Err(format!("{path} is the scene {id}, not {prefab}"))
            }
            Ok(_) => Ok(text),
            Err(e) => Err(format!("{path}:{e}")),
        }
    }
}

/// A prefab's text as read.
pub(crate) struct Text {
    pub(crate) path: String,
    pub(crate) text: String,
}

/// The texts of every prefab `parsed` places, and theirs in turn, read once each through
/// `source` (none: every one missing).
pub(crate) fn read_texts(
    parsed: &Parsed<'_>,
    source: Option<&dyn Prefabs>,
) -> HashMap<AssetId, Result<Text, String>> {
    let mut texts: HashMap<AssetId, Result<Text, String>> = HashMap::new();
    let mut queue: Vec<(AssetId, String)> = prefabs_of(parsed).collect();
    while let Some((id, path)) = queue.pop() {
        if texts.contains_key(&id) {
            continue;
        }
        let read = match source {
            Some(source) => source.text(id, &path),
            None => Err(String::from("no prefabs were given to the loading")),
        };
        if let Ok(text) = &read
            && let Ok(inner) = crate::parse(text)
        {
            queue.extend(prefabs_of(&inner));
        }
        texts.insert(id, read.map(|text| Text { path, text }));
    }
    texts
}

/// The prefabs a scene's instance blocks name: their ids and paths.
fn prefabs_of<'a>(parsed: &'a Parsed<'_>) -> impl Iterator<Item = (AssetId, String)> + 'a {
    parsed.blocks.iter().filter_map(|b| {
        let (_, value) = b.prefab()?;
        let (path, id) = text::parse_asset(value).ok()??;
        Some((id, path))
    })
}

/// A prefab parsed for placing (its path and blocks), or why it cannot be placed.
pub(crate) type Prefab<'t> = Result<(&'t str, Parsed<'t>), String>;

/// Parses every text read and checks what loading it alone would refuse (an unknown
/// component, an id twice): such a prefab cannot be placed.
pub(crate) fn parse_texts<'t>(
    texts: &'t HashMap<AssetId, Result<Text, String>>,
    registry: &SceneRegistry,
) -> HashMap<AssetId, Prefab<'t>> {
    texts
        .iter()
        .map(|(&id, read)| {
            let parsed = match read {
                Ok(t) => crate::parse(&t.text)
                    .and_then(|p| check(&p, registry).map(|()| p))
                    .map(|p| (t.path.as_str(), p))
                    .map_err(|e| format!("{}:{e}", t.path)),
                Err(e) => Err(e.clone()),
            };
            (id, parsed)
        })
        .collect()
}

/// What placing a prefab would refuse: an id twice, an instance without its prefab, an
/// unknown component, a component written whole that is not. Such a prefab is not placed.
/// (The loaded scene's own are found as it is placed. Parents are checked once instances
/// are placed: one may be inside an instance.)
pub(crate) fn check(parsed: &Parsed<'_>, registry: &SceneRegistry) -> Result<(), LoadError> {
    let mut seen: HashMap<SceneId, usize> = HashMap::new();
    for b in &parsed.blocks {
        if b.kind != Kind::Override && seen.insert(b.id, b.line).is_some() {
            return Err(err(b.line, format!("scene id {} appears twice", b.id)));
        }
        if b.kind == Kind::Instance && b.prefab().is_none() {
            return Err(err(
                b.line,
                "an instance needs `prefab = asset(\"path\", <id>)`",
            ));
        }
        for &(line, component, path, value) in &b.fields {
            let Some(&entry) = registry.by_name.get(component) else {
                return Err(err(line, format!("unknown component `{component}`")));
            };
            if path.is_empty() && value != "{}" && !whole_value(registry.entries[entry].info) {
                return Err(err(
                    line,
                    format!("expected `{component}.<field> = value` or `{component} = {{}}`"),
                ));
            }
        }
    }
    Ok(())
}

/// One step of a namespace: an instance placing a prefab, by the instance's id in the
/// scene around it, and the prefab's single root, which takes the instance's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) instance: SceneId,
    pub(crate) root: Option<SceneId>,
}

impl Step {
    fn map(self, id: SceneId) -> SceneId {
        if Some(id) == self.root {
            self.instance
        } else {
            SceneId::derive(self.instance, id)
        }
    }
}

/// An id of the innermost prefab of `chain` (outermost first), in the scene loaded.
pub(crate) fn map(chain: &[Step], id: SceneId) -> SceneId {
    chain.iter().rev().fold(id, |id, step| step.map(id))
}

/// A value's line, with where it comes from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Line<'t> {
    /// The loaded scene's line it comes from: its own, or its instance block's.
    pub(crate) at: usize,
    /// Where a prefab writes it: the prefab's path in [`Plan::files`], and its line.
    pub(crate) from: Option<(u32, usize)>,
    /// The namespace its entity references are read in ([`Plan::chains`]).
    pub(crate) chain: u32,
    pub(crate) component: &'t str,
    pub(crate) path: &'t str,
    pub(crate) value: &'t str,
}

/// A block's value lines: (line, component, path after the component, value).
pub(crate) type Fields<'t> = [(usize, &'t str, &'t str, &'t str)];

/// An entity to place.
#[derive(Debug)]
pub(crate) struct Planned<'p, 't> {
    pub(crate) id: SceneId,
    /// Its own block is the loaded scene's (not a prefab's).
    pub(crate) own: bool,
    /// Left out by an instance's `remove = @…`.
    pub(crate) gone: bool,
    pub(crate) parent: Option<(usize, SceneId)>,
    pub(crate) order: Option<(usize, u32)>,
    /// Its block's lines, and where the block sits.
    pub(crate) block: Option<(&'p Fields<'t>, Place)>,
    /// What an instance's entities have besides (none for most of a scene's).
    pub(crate) more: Option<Box<More<'t>>>,
}

/// What an entity an instance places, or an instance's root, has besides its block.
#[derive(Debug, Default)]
pub(crate) struct More<'t> {
    /// Lines over its block's: its instance's, overrides'.
    pub(crate) lines: Vec<Line<'t>>,
    /// Components taken away (`remove = Component`), with their lines.
    pub(crate) removed: Vec<(usize, &'t str)>,
    pub(crate) instance: Option<PrefabInstance>,
    pub(crate) part: Option<PrefabPart>,
    /// What of its instance is kept as written.
    pub(crate) kept: KeptBlocks,
}

impl<'t> Planned<'_, 't> {
    fn new(id: SceneId, own: bool) -> Self {
        Self {
            id,
            own,
            gone: false,
            parent: None,
            order: None,
            block: None,
            more: None,
        }
    }

    fn more(&mut self) -> &mut More<'t> {
        self.more.get_or_insert_with(Box::default)
    }

    pub(crate) fn instance(&self) -> Option<PrefabInstance> {
        self.more.as_ref().and_then(|m| m.instance)
    }

    pub(crate) fn part(&self) -> Option<PrefabPart> {
        self.more.as_ref().and_then(|m| m.part)
    }
}

/// Everything a scene places, its prefabs' entities among its own.
pub(crate) struct Plan<'p, 't> {
    pub(crate) entities: Vec<Planned<'p, 't>>,
    /// The namespaces lines read their references in; the loaded scene's is the first.
    pub(crate) chains: Vec<Vec<Step>>,
    /// The prefabs' paths, for messages.
    pub(crate) files: Vec<&'t str>,
    pub(crate) warnings: Vec<(usize, String)>,
    /// The prefabs placed.
    pub(crate) prefabs: Vec<AssetId>,
    /// The paths instances name their prefabs by, for writing them back the same way.
    pub(crate) paths: Vec<(AssetId, String)>,
    pub(crate) index: HashMap<SceneId, usize>,
}

impl<'p, 't> Plan<'p, 't> {
    pub(crate) fn new() -> Self {
        Self {
            entities: Vec::new(),
            chains: vec![Vec::new()],
            files: Vec::new(),
            warnings: Vec::new(),
            prefabs: Vec::new(),
            paths: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn push(&mut self, planned: Planned<'p, 't>, line: usize) -> Result<usize, LoadError> {
        let i = self.entities.len();
        if self.index.insert(planned.id, i).is_some() {
            return Err(err(line, format!("scene id {} appears twice", planned.id)));
        }
        self.entities.push(planned);
        Ok(i)
    }
}

/// What planning needs besides the scene: the prefabs read, and those being placed
/// (outermost first: one placing itself is refused).
pub(crate) struct Planning<'p, 't> {
    pub(crate) prefabs: &'p HashMap<AssetId, Prefab<'t>>,
    pub(crate) stack: Vec<AssetId>,
}

/// Where a scene being planned sits: the namespace mapping its ids, the loaded scene's
/// line it all comes from (none: the loaded scene itself, each block its own line), and
/// its path in [`Plan::files`] (a prefab).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Place {
    pub(crate) chain: u32,
    pub(crate) at: Option<usize>,
    pub(crate) file: Option<u32>,
}

impl Place {
    /// The loaded scene's own blocks.
    pub(crate) const SCENE: Self = Self {
        chain: 0,
        at: None,
        file: None,
    };

    fn here(self, line: usize) -> usize {
        self.at.unwrap_or(line)
    }

    /// A block's value line, read here.
    pub(crate) fn line<'t>(
        self,
        (line, component, path, value): (usize, &'t str, &'t str, &'t str),
    ) -> Line<'t> {
        Line {
            at: self.here(line),
            from: self.file.map(|f| (f, line)),
            chain: self.chain,
            component,
            path,
            value,
        }
    }

    fn lines<'t>(self, fields: &Fields<'t>) -> Vec<Line<'t>> {
        fields.iter().map(|&f| self.line(f)).collect()
    }
}

/// The part an entity written `local` in a scene placed by `steps` is of that scene's
/// instance: none for the loaded scene's own, and none for the prefab's single root (it is
/// the instance's entity; the level placing it says what it is part of).
fn part_of(steps: &[Step], local: SceneId) -> Option<PrefabPart> {
    let (&step, outer) = steps.split_last()?;
    (step.root != Some(local)).then(|| PrefabPart {
        instance: map(outer, step.instance),
        local,
    })
}

/// Plans the entities of `parsed`, its instances' prefabs placed and its overrides
/// applied.
pub(crate) fn plan_scene<'p, 't>(
    plan: &mut Plan<'p, 't>,
    planning: &mut Planning<'p, 't>,
    parsed: &'p Parsed<'t>,
    place: Place,
) -> Result<(), LoadError> {
    let steps = plan.chains[place.chain as usize].clone();
    plan.entities.reserve(parsed.blocks.len());
    plan.index.reserve(parsed.blocks.len());
    for b in &parsed.blocks {
        match b.kind {
            Kind::Entity => {
                let mut planned = Planned::new(map(&steps, b.id), place.at.is_none());
                planned.parent = b.parent.map(|(l, p)| (place.here(l), map(&steps, p)));
                planned.order = b.order.map(|(l, n)| (place.here(l), n));
                planned.block = Some((b.fields.as_slice(), place));
                if let Some(part) = part_of(&steps, b.id) {
                    planned.more().part = Some(part);
                }
                plan.push(planned, place.here(b.line))?;
            }
            Kind::Instance => place_instance(plan, planning, b, place, &steps)?,
            Kind::Override => {}
        }
    }
    // The overrides, over what the instances placed.
    for b in parsed.blocks.iter().filter(|b| b.kind == Kind::Override) {
        let target = map(&steps, b.id);
        if let Some(i) = plan.index.get(&target).copied()
            && !plan.entities[i].gone
        {
            let lines = place.lines(&b.fields);
            let more = plan.entities[i].more();
            more.lines.extend(lines);
            more.removed
                .extend(b.removes().iter().map(|&(l, c)| (place.here(l), c)));
            continue;
        }
        let line = place.here(b.line);
        let target = b.target();
        let instance = target.first().map(|&first| map(&steps, first));
        match instance.and_then(|i| plan.index.get(&i).copied()) {
            // An instance not placed: its warning said so; its overrides are kept.
            Some(i) if plan.entities[i].instance().is_some_and(|x| x.is_missing()) => {
                plan.entities[i].more().kept.blocks.push_str(b.text());
            }
            Some(i) if plan.entities[i].instance().is_some() => {
                plan.warnings.push((
                    line,
                    format!(
                        "the prefab has no entity {} any more: its override is kept as \
                         written",
                        path_text(&target[1..])
                    ),
                ));
                plan.entities[i].more().kept.blocks.push_str(b.text());
            }
            _ => plan.warnings.push((
                line,
                format!(
                    "no instance {} to override: the block is dropped",
                    target.first().map_or(String::new(), ToString::to_string)
                ),
            )),
        }
    }
    Ok(())
}

/// Places instance block `b`: its prefab's entities, planned with their ids mapped
/// through a new step, then the block's own lines and removals.
fn place_instance<'p, 't>(
    plan: &mut Plan<'p, 't>,
    planning: &mut Planning<'p, 't>,
    b: &Block<'t>,
    place: Place,
    steps: &[Step],
) -> Result<(), LoadError> {
    let line = place.here(b.line);
    let id = map(steps, b.id);
    let parent = b.parent.map(|(l, p)| (place.here(l), map(steps, p)));
    let order = b.order.map(|(l, n)| (place.here(l), n));
    let part = part_of(steps, b.id);
    let own_lines = place.lines(&b.fields);
    let Some((prefab_path, prefab)) = b
        .prefab()
        .and_then(|(_, value)| text::parse_asset(value).ok().flatten())
    else {
        return Err(err(
            line,
            "an instance needs `prefab = asset(\"path\", <id>)`",
        ));
    };
    plan.paths.push((prefab, prefab_path.clone()));
    let placeable = if planning.stack.contains(&prefab) {
        Err(format!("{prefab_path} would place itself"))
    } else {
        match planning.prefabs.get(&prefab) {
            Some(Ok((path, inner))) => Ok((*path, inner)),
            Some(Err(why)) => Err(why.clone()),
            None => Err(format!("{prefab_path} was not read")),
        }
    };
    let (path, inner) = match placeable {
        Ok(found) => found,
        Err(why) => {
            // Nothing to place: the instance's entity alone, its own lines applied (they
            // place it), and its blocks kept as written.
            plan.warnings.push((
                line,
                format!("instance {}: its prefab cannot be placed: {why}", b.id),
            ));
            let mut planned = Planned::new(id, place.at.is_none());
            planned.parent = parent;
            planned.order = order;
            let more = planned.more();
            more.lines = own_lines;
            more.part = part;
            more.instance = Some(PrefabInstance {
                prefab: AssetRef::file(prefab),
                missing: 1,
            });
            // Its overrides are kept with it (the scene's overrides, below).
            more.kept.lines.push_str(b.body());
            plan.push(planned, line)?;
            return Ok(());
        }
    };
    if !plan.prefabs.contains(&prefab) {
        plan.prefabs.push(prefab);
    }
    let roots: Vec<SceneId> = inner
        .blocks
        .iter()
        .filter(|r| r.kind != Kind::Override && r.parent.is_none())
        .map(|r| r.id)
        .collect();
    let root = (roots.len() == 1).then(|| roots[0]);
    let mut chain = steps.to_vec();
    chain.push(Step {
        instance: b.id,
        root,
    });
    plan.chains.push(chain);
    let inner_place = Place {
        chain: (plan.chains.len() - 1) as u32,
        at: Some(line),
        file: Some(plan.files.len() as u32),
    };
    plan.files.push(path);
    let start = plan.entities.len();
    // Several roots: an entity of the instance's own holds them.
    if root.is_none() {
        let mut holder = Planned::new(id, place.at.is_none());
        holder.parent = parent;
        holder.order = order;
        holder.more().part = part;
        plan.push(holder, line)?;
    }
    planning.stack.push(prefab);
    let placed = plan_scene(plan, planning, inner, inner_place);
    planning.stack.pop();
    placed?;
    // The prefab's roots under the holder.
    if root.is_none() {
        for planned in &mut plan.entities[start + 1..] {
            if planned.parent.is_none() && planned.part().is_some_and(|p| p.instance == id) {
                planned.parent = Some((line, id));
            }
        }
    }
    let Some(&i) = plan.index.get(&id) else {
        return Ok(());
    };
    let entity = &mut plan.entities[i];
    if root.is_some() {
        entity.parent = parent;
        entity.order = order;
        entity.own = place.at.is_none();
    }
    let more = entity.more();
    // The outermost instance an entity roots is the one it shows (a variant's root is an
    // instance of the prefab it varies).
    more.instance = Some(PrefabInstance::new(AssetRef::file(prefab)));
    more.kept = KeptBlocks::default();
    if root.is_some() {
        more.part = part;
    }
    more.lines.extend(own_lines);
    // Entities of the prefab left out, with their children (spawning skips those under a
    // gone one).
    let inner_chain = plan.chains[inner_place.chain as usize].clone();
    for &(at, label) in b.removes() {
        let at = place.here(at);
        let Some(target) = label
            .strip_prefix('@')
            .and_then(SceneId::parse_path)
            .map(|path| map(&inner_chain, SceneId::of_path(&path)))
        else {
            plan.warnings.push((
                at,
                format!("expected `remove = @<entity>`, found `remove = {label}`"),
            ));
            continue;
        };
        match plan.index.get(&target).copied() {
            Some(t) if t != i => plan.entities[t].gone = true,
            _ => {
                plan.warnings.push((
                    at,
                    format!("the prefab has no entity {label} any more: its removal is kept"),
                ));
                let _ = writeln!(plan.entities[i].more().kept.lines, "remove = {label}");
            }
        }
    }
    Ok(())
}

/// `a/b/c` of ids.
pub(crate) fn path_text(path: &[SceneId]) -> String {
    let mut out = String::new();
    for (i, id) in path.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        let _ = write!(out, "{id}");
    }
    out
}
