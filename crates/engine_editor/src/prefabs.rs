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

use engine_scene::{AssetDatabase, AssetId, PrefabFiles, Prefabs, SceneId};
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
