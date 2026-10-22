//! Prefabs in the editor (ADR 0107's E5, ADR 0108's E5-2): scenes placed as instances,
//! read through the project's asset database.
//!
//! - **A scene dropped** from the FileSystem dock into the view or the tree is placed as
//!   an instance of it, in one undo step.
//! - **Make Prefab** writes the selected entity and its children to a scene file of their
//!   own and turns them into an instance of it: the entity keeps its id (the instance's),
//!   its children take ids derived from it, one undo step. Undo leaves the file.
//! - **Unpack Prefab** turns an instance's entities into plain ones; instances inside it
//!   stay instances.
//! - **Scenes load and save through their prefabs**, found by id, so a prefab moved or
//!   renamed in the editor is still found.
//! - **A prefab written anew is followed** (E5-4, [`follow`]): the open scene's instances
//!   take its new values where they did not change them, its new entities and components,
//!   and lose what it no longer has where they kept it as it was. Entities are changed in
//!   place, so what refers to them still does.

use crate::History;
use engine_ecs::{ComponentId, Entity, GlobalTransform, QueryState, Transform, World};
use engine_reflect::TypeInfo;
use engine_scene::text::ReadRefs;
use engine_scene::{
    AssetDatabase, AssetId, AssetPaths, PrefabBase, PrefabFiles, PrefabInstance, PrefabPart,
    Prefabs, SceneId, SceneRegistry,
};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

/// The project's prefabs: scene files under its folder, found by id through the asset
/// database (a file moved since the scene was written is found where it is now), else by
/// the path the scene wrote.
pub(crate) struct ProjectPrefabs<'a> {
    pub(crate) root: &'a Path,
    pub(crate) db: &'a AssetDatabase,
}

impl Prefabs for ProjectPrefabs<'_> {
    fn text(&self, prefab: AssetId, path: &str) -> Result<String, String> {
        let path = self.db.get(prefab).map_or(path, |e| e.path.as_str());
        PrefabFiles(self.root).text(prefab, path)
    }
}

/// A scene of one instance, `instance`, of scene `prefab` at `path`: what loading places
/// into the world for a drop.
pub(crate) fn instance_text(instance: SceneId, prefab: AssetId, path: &str) -> String {
    let mut text = format!(
        "[scene version={}]\n\n[instance {instance}]\nprefab = asset(",
        engine_scene::VERSION
    );
    engine_scene::text::quote_into(&mut text, path);
    let _ = writeln!(text, ", {prefab})");
    text
}

/// A file name for a prefab made of an entity named `name` in folder `dir` (under the
/// project's `root`): its name, made safe for a file, and not one there is.
pub(crate) fn prefab_path(root: &Path, dir: &str, name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    let stem = stem.trim();
    let stem = if stem.is_empty() { "Prefab" } else { stem };
    let join = |file: &str| {
        if dir.is_empty() {
            file.to_owned()
        } else {
            format!("{dir}/{file}")
        }
    };
    let mut path = join(&format!("{stem}.scene"));
    let mut n = 2;
    while root.join(&path).exists() {
        path = join(&format!("{stem} {n}.scene"));
        n += 1;
    }
    path
}

/// What following a prefab changed in an instance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Followed {
    /// Values that took the prefab's new ones.
    pub(crate) values: usize,
    /// Components added or taken away.
    pub(crate) components: usize,
    /// Entities added or taken away.
    pub(crate) entities: usize,
}

impl Followed {
    /// Whether nothing changed.
    pub(crate) fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// A component's lines (`path = value`, entity references by the scene ids `ids` gives).
fn lines_of(
    info: &'static TypeInfo,
    ptr: *const u8,
    ids: &dyn Fn(Entity) -> Option<SceneId>,
    assets: &AssetPaths,
) -> String {
    let mut out = String::new();
    // SAFETY: the callers pass a live value of the type `info` describes.
    unsafe { engine_scene::write_lines_with(info, ptr, ids, Some(assets), &mut out) };
    out
}

/// A component's lines as (path, value) pairs.
fn pairs(text: &str) -> Vec<(&str, &str)> {
    text.lines()
        .map(|l| l.split_once(" = ").unwrap_or((l, "")))
        .collect()
}

/// Entity references read back by scene id (or path) through `index`.
struct IdRefs<'a> {
    index: &'a HashMap<SceneId, Entity>,
}

impl ReadRefs for IdRefs<'_> {
    fn entity(&mut self, label: &str) -> Result<Entity, String> {
        if label == "none" {
            return Ok(Entity::NULL);
        }
        let path = SceneId::parse_path(label).ok_or_else(|| format!("`@{label}`"))?;
        Ok(self
            .index
            .get(&SceneId::of_path(&path))
            .copied()
            .unwrap_or(Entity::NULL))
    }
}

/// Gives entity `e` the component `info` describes, from its `text` lines (its other
/// fields their type's default). False when the type cannot be made (no `Default`).
fn insert_from_lines(
    world: &mut World,
    e: Entity,
    info: &'static TypeInfo,
    text: &str,
    index: &HashMap<SceneId, Entity>,
) -> bool {
    let Some(reflected) = engine_ecs::find_reflected(info.name) else {
        return false;
    };
    let engine_ecs::ReflectedKind::Component { id, default } = reflected.kind else {
        return false;
    };
    let Some(write) = default() else {
        return false;
    };
    let Ok(layout) = std::alloc::Layout::from_size_align(info.size.max(1), info.align) else {
        return false;
    };
    if !world.contains(e) {
        return false;
    }
    let component = id(world);
    let mut refs = IdRefs { index };
    // SAFETY: a non-zero size. The room is made a valid value by its default writer, read
    // into (staying valid), then moved into the world by `insert_raw` (the entity is
    // alive, so it is consumed), and freed without dropping what moved out.
    unsafe {
        let ptr = std::alloc::alloc(layout);
        if ptr.is_null() {
            return false;
        }
        write(ptr);
        let _ = engine_scene::read_lines(info, ptr, text, &mut refs);
        world.insert_raw(e, component, ptr);
        std::alloc::dealloc(ptr, layout);
    }
    true
}

/// Makes the instance rooted at `root` follow its prefab from `old` (as it was placed
/// when the instance last followed it) to `new` (E5-4), in the open history step:
/// - entities the prefab no longer has go, with what is under them;
/// - an entity kept by both: each value the instance left as the old prefab had it takes
///   the new one (a component whose fields changed shape is taken whole when the
///   instance had not changed it); components the prefab added are added, and those it
///   took away go where the instance had them as they were;
/// - entities the prefab added are placed, parents first.
///
/// What the instance changed (its overrides) stays: it removed or changed it.
pub(crate) fn follow(
    world: &mut World,
    history: &mut History,
    root: Entity,
    old: &PrefabBase,
    new: &PrefabBase,
    registry: &SceneRegistry,
    assets: &AssetPaths,
) -> Followed {
    let mut done = Followed::default();
    // The scene's entities by id (references resolve through it), and the instance's own:
    // its root and what instances placed under it.
    let mut ids_q = QueryState::<(Entity, &SceneId)>::new(world);
    let mut index: HashMap<SceneId, Entity> =
        ids_q.iter_readonly(world).map(|(e, &id)| (id, e)).collect();
    let mut members: HashMap<SceneId, Entity> = HashMap::new();
    let mut stack = vec![root];
    while let Some(e) = stack.pop() {
        if e == root || world.get::<PrefabPart>(e).is_some() {
            if let Some(&id) = world.get::<SceneId>(e) {
                members.insert(id, e);
            }
            stack.extend(world.children(e));
        }
    }
    // Entities the prefab no longer has.
    let gone: Vec<SceneId> = old.ids().filter(|&id| new.entity(id).is_none()).collect();
    for id in gone {
        if let Some(e) = members.remove(&id)
            && world.contains(e)
        {
            history.delete_subtree(world, e);
            done.entities += 1;
        }
    }
    members.retain(|_, e| world.contains(*e));
    // Entities both have: what the instance did not change follows.
    let kept: Vec<(SceneId, Entity)> = members
        .iter()
        .filter(|&(&id, _)| old.entity(id).is_some() && new.entity(id).is_some())
        .map(|(&id, &e)| (id, e))
        .collect();
    for (id, e) in kept {
        follow_entity(
            world, history, e, id, old, new, registry, assets, &index, &mut done,
        );
    }
    // Entities the prefab added, parents first.
    let mut added: Vec<SceneId> = new
        .ids()
        .filter(|&id| old.entity(id).is_none() && !members.contains_key(&id))
        .collect();
    added.sort_by_key(|&id| (new.depth(id), id));
    for id in added {
        let Some(from) = new.entity(id) else {
            continue;
        };
        let e = world.spawn((id,));
        history.touch_new(world, e);
        index.insert(id, e);
        if let Some(&part) = new.world().get::<PrefabPart>(from) {
            world.insert(e, part);
        }
        if let Some(&instance) = new.world().get::<PrefabInstance>(from) {
            world.insert(e, instance);
        }
        for (_, info, ptr) in new.components(id) {
            if registry.contains(info.name) {
                let text = lines_of(info, ptr.as_ptr(), &|x| new.scene_id(x), assets);
                insert_from_lines(world, e, info, &text, &index);
            }
        }
        if world.get::<Transform>(e).is_some() && world.get::<GlobalTransform>(e).is_none() {
            world.insert(e, GlobalTransform::default());
        }
        if let Some(parent) = new.parent(id).and_then(|p| index.get(&p).copied()) {
            let _ = world.set_parent(e, Some(parent));
        }
        done.entities += 1;
    }
    done
}

/// [`follow`] for one entity both prefabs place: its components.
#[allow(clippy::too_many_arguments)]
fn follow_entity(
    world: &mut World,
    history: &mut History,
    e: Entity,
    id: SceneId,
    old: &PrefabBase,
    new: &PrefabBase,
    registry: &SceneRegistry,
    assets: &AssetPaths,
    index: &HashMap<SceneId, Entity>,
    done: &mut Followed,
) {
    // The instance's scene components, by their type's name.
    let mine: Vec<(String, ComponentId, &'static TypeInfo)> = world
        .component_ids_of(e)
        .map_or_else(Vec::new, <[ComponentId]>::to_vec)
        .into_iter()
        .filter_map(|c| {
            let info = world.components().type_info(c)?;
            registry
                .contains(info.name)
                .then(|| Some((world.components().name(c)?.to_owned(), c, info)))?
        })
        .collect();
    let olds = old.components(id);
    let news = new.components(id);
    let mut names: Vec<String> = mine.iter().map(|(n, _, _)| n.clone()).collect();
    for (n, info, _) in olds.iter().chain(&news) {
        if registry.contains(info.name) && !names.iter().any(|m| m == n) {
            names.push((*n).to_owned());
        }
    }
    for name in names {
        let ids_here = |x: Entity| world.get::<SceneId>(x).copied();
        let here = mine
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|(n, c, info)| (n.as_str(), *c, *info));
        let mine_text = here.and_then(|(_, c, info)| {
            let ptr = world.get_raw(e, c)?;
            Some(lines_of(info, ptr.as_ptr(), &ids_here, assets))
        });
        let was = olds.iter().find(|(n, _, _)| *n == name);
        let is = news.iter().find(|(n, _, _)| *n == name);
        let was_text =
            was.map(|&(_, info, ptr)| lines_of(info, ptr.as_ptr(), &|x| old.scene_id(x), assets));
        let is_text =
            is.map(|&(_, info, ptr)| lines_of(info, ptr.as_ptr(), &|x| new.scene_id(x), assets));
        match (here, mine_text, was_text, is_text) {
            (Some((_, c, info)), Some(mine), Some(was), Some(is)) if was != is => {
                let (m, w, n) = (pairs(&mine), pairs(&was), pairs(&is));
                let same_fields = m.iter().map(|f| f.0).eq(w.iter().map(|f| f.0))
                    && w.iter().map(|f| f.0).eq(n.iter().map(|f| f.0));
                if same_fields {
                    // Each value the instance left as the old prefab had it.
                    let mut changes = String::new();
                    let mut count = 0;
                    for ((mine, was), is) in m.iter().zip(&w).zip(&n) {
                        if mine.1 == was.1 && was.1 != is.1 {
                            let _ = writeln!(changes, "{} = {}", is.0, is.1);
                            count += 1;
                        }
                    }
                    if count > 0
                        && let Some(ptr) = {
                            history.touch(world, e);
                            world.get_raw_mut(e, c)
                        }
                    {
                        let mut refs = IdRefs { index };
                        // SAFETY: the entity's live value of the component `info` describes.
                        let _ = unsafe {
                            engine_scene::read_lines(info, ptr.as_ptr(), &changes, &mut refs)
                        };
                        done.values += count;
                    }
                } else if mine == was {
                    // Its fields changed shape (a list grown or shrunk): taken whole, as the
                    // instance had not changed it.
                    history.touch(world, e);
                    world.remove_raw(e, c);
                    insert_from_lines(world, e, info, &is, index);
                    done.values += 1;
                }
            }
            // The prefab took it away, and the instance had it as it was.
            (Some((_, c, _)), Some(mine), Some(was), None) if mine == was => {
                history.touch(world, e);
                world.remove_raw(e, c);
                done.components += 1;
            }
            // The prefab added it.
            (None, None, None, Some(is)) => {
                if let Some(&(_, info, _)) = news.iter().find(|(n, _, _)| *n == name) {
                    history.touch(world, e);
                    if insert_from_lines(world, e, info, &is, index) {
                        done.components += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_text_names_its_prefab() {
        let (i, p) = (SceneId::from_u128(7), AssetId::from_u128(9));
        let text = instance_text(i, p, "props/a \"b\".scene");
        assert!(text.starts_with("[scene version="));
        assert!(text.contains(&format!("[instance {i}]\n")));
        assert!(text.contains(&format!(
            "prefab = asset(\"props/a \\\"b\\\".scene\", {p})\n"
        )));
    }

    #[test]
    fn a_prefab_takes_a_free_name() {
        let dir = std::env::temp_dir().join(format!("ofelia-prefab-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("props")).unwrap();
        assert_eq!(prefab_path(&dir, "props", "Lamp"), "props/Lamp.scene");
        std::fs::write(dir.join("props/Lamp.scene"), "").unwrap();
        assert_eq!(prefab_path(&dir, "props", "Lamp"), "props/Lamp 2.scene");
        assert_eq!(prefab_path(&dir, "", "a/b"), "a_b.scene");
        assert_eq!(prefab_path(&dir, "", "  "), "Prefab.scene");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
