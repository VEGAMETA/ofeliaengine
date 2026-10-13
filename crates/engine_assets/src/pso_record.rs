//! PSO records (ADR 0092): the entry points of material types a game's frames drew, as
//! playtests recorded them, so that a cook compiles only those
//! ([`crate::material_file::SlangMaterialTypes::with_pso_record`]) instead of every
//! fragment entry point of `forward.slang` for every type.
//!
//! ```ron
//! // game.pso.ron
//! (
//!     version: 1,
//!     types: {
//!         "fabric.slang:FabricMaterial": ["fragmentMainAoBent", "fragmentReflectAoBent"],
//!         "gltf.slang:GltfClearcoat": ["fragmentMain"],
//!     },
//! )
//! ```
//!
//! - A type is named by its module's file name and its own name: records do not depend
//!   on where a checkout is, and a team shares them in version control.
//! - Runs merge into the file ([`PsoRecord::merge`]): a record only grows, so every
//!   feature a playtest turned on stays covered. Sorted, so merges diff cleanly.
//! - A type the record does not name is cooked whole, as without a record. An entry point
//!   a pack lacks draws its type's glTF fallback, with a warning to record it.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

/// The record's format version.
const VERSION: u32 = 1;

/// The entry points of material types frames drew (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PsoRecord {
    /// The format version.
    #[serde(default = "version")]
    pub version: u32,
    /// By [`PsoRecord::key`]: the entry points.
    pub types: BTreeMap<String, BTreeSet<String>>,
}

fn version() -> u32 {
    VERSION
}

/// Why a record could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsoRecordError(pub String);

impl fmt::Display for PsoRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PsoRecordError {}

impl PsoRecord {
    /// An empty record.
    pub fn new() -> Self {
        Self {
            version: VERSION,
            types: BTreeMap::new(),
        }
    }

    /// The name of a type: its module's file name, `:`, its name.
    pub fn key(module: &Path, type_name: &str) -> String {
        let file = module
            .file_name()
            .map_or_else(|| module.to_string_lossy(), |f| f.to_string_lossy());
        format!("{file}:{type_name}")
    }

    /// Records that frames drew `type_name` (of `module`) with `entry`.
    pub fn add(&mut self, module: &Path, type_name: &str, entry: &str) {
        self.types
            .entry(Self::key(module, type_name))
            .or_default()
            .insert(entry.to_owned());
    }

    /// The entry points recorded for a type, if the record names it.
    pub fn entries(&self, module: &Path, type_name: &str) -> Option<&BTreeSet<String>> {
        self.types.get(&Self::key(module, type_name))
    }

    /// Adds everything `other` recorded.
    pub fn merge(&mut self, other: &Self) {
        for (key, entries) in &other.types {
            self.types
                .entry(key.clone())
                .or_default()
                .extend(entries.iter().cloned());
        }
    }

    /// Entry points over all types.
    pub fn len(&self) -> usize {
        self.types.values().map(BTreeSet::len).sum()
    }

    /// Whether it records nothing.
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Parses a record's text.
    pub fn parse(text: &str) -> Result<Self, PsoRecordError> {
        let record: Self = ron::from_str(text).map_err(|e| {
            PsoRecordError(format!(
                "{}:{}: {}",
                e.span.start.line, e.span.start.col, e.code
            ))
        })?;
        if record.version != VERSION {
            return Err(PsoRecordError(format!(
                "version {} (this engine reads {VERSION})",
                record.version
            )));
        }
        Ok(record)
    }

    /// The record as text (sorted, one type per line).
    pub fn to_text(&self) -> String {
        let mut text = String::from(
            "// PSO record (ADR 0092): the material types' entry points frames drew.\n// Merged by every recording run; a cook compiles only these.\n(\n",
        );
        text.push_str(&format!("    version: {VERSION},\n    types: {{\n"));
        for (key, entries) in &self.types {
            let list: Vec<String> = entries.iter().map(|e| format!("{e:?}")).collect();
            text.push_str(&format!("        {key:?}: [{}],\n", list.join(", ")));
        }
        text.push_str("    },\n)\n");
        text
    }

    /// Reads `path`; an empty record when it does not exist.
    pub fn load(path: &Path) -> Result<Self, PsoRecordError> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                Self::parse(&text).map_err(|e| PsoRecordError(format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(PsoRecordError(format!("{}: {e}", path.display()))),
        }
    }

    /// Merges this record into the one at `path` (made if missing) and writes it back;
    /// returns the merged record. The file is locked while it is read and written, so
    /// runs that save at once (parallel tests, several playtests) keep each other's entries.
    pub fn save_merged(&self, path: &Path) -> Result<Self, PsoRecordError> {
        use std::io::{Read as _, Seek as _, Write as _};
        let error = |e: std::io::Error| PsoRecordError(format!("{}: {e}", path.display()));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(error)?;
        // Released when the file closes.
        file.lock().map_err(error)?;
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(error)?;
        let mut merged = if text.trim().is_empty() {
            Self::new()
        } else {
            Self::parse(&text).map_err(|e| PsoRecordError(format!("{}: {e}", path.display())))?
        };
        merged.merge(self);
        file.set_len(0).map_err(error)?;
        file.rewind().map_err(error)?;
        file.write_all(merged.to_text().as_bytes()).map_err(error)?;
        Ok(merged)
    }

    /// The record next to a scene: `scene.gltf` → `scene.pso.ron`.
    pub fn path_for(scene: &Path) -> std::path::PathBuf {
        scene.with_extension("pso.ron")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_are_named_by_file_and_merged() {
        let mut a = PsoRecord::new();
        a.add(
            Path::new("H:/game/materials/fabric.slang"),
            "FabricMaterial",
            "fragmentMainAo",
        );
        a.add(
            Path::new("materials/gltf.slang"),
            "GltfClearcoat",
            "fragmentMain",
        );
        let mut b = PsoRecord::new();
        b.add(
            Path::new("D:/other/checkout/materials/fabric.slang"),
            "FabricMaterial",
            "fragmentMain",
        );
        b.add(
            Path::new("D:/other/checkout/materials/fabric.slang"),
            "FabricMaterial",
            "fragmentMainAo",
        );
        a.merge(&b);
        let fabric = a
            .entries(Path::new("fabric.slang"), "FabricMaterial")
            .unwrap();
        assert_eq!(
            fabric.iter().map(String::as_str).collect::<Vec<_>>(),
            ["fragmentMain", "fragmentMainAo"],
            "the same type from another checkout; merged, sorted, no duplicates"
        );
        assert_eq!(a.len(), 3);
        assert!(
            a.entries(Path::new("fabric.slang"), "VelvetMaterial")
                .is_none()
        );
    }

    #[test]
    fn text_round_trips_and_is_sorted() {
        let mut r = PsoRecord::new();
        r.add(Path::new("b.slang"), "B", "fragmentMain");
        r.add(Path::new("a.slang"), "A", "fragmentReflectAo");
        r.add(Path::new("a.slang"), "A", "fragmentMain");
        let text = r.to_text();
        assert_eq!(PsoRecord::parse(&text).unwrap(), r);
        let a = text.find("\"a.slang:A\"").unwrap();
        let b = text.find("\"b.slang:B\"").unwrap();
        assert!(a < b, "{text}");
        assert!(
            text.contains(r#""a.slang:A": ["fragmentMain", "fragmentReflectAo"]"#),
            "{text}"
        );
        assert!(PsoRecord::parse("(version: 2, types: {})").is_err());
        assert!(PsoRecord::parse("(types: {}, extra: 1)").is_err());
    }

    #[test]
    fn saving_merges_with_the_file() {
        let path =
            std::env::temp_dir().join(format!("engine_assets_pso_{}.ron", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut first = PsoRecord::new();
        first.add(Path::new("a.slang"), "A", "fragmentMain");
        first.save_merged(&path).unwrap();
        let mut second = PsoRecord::new();
        second.add(Path::new("a.slang"), "A", "fragmentMainAo");
        let merged = second.save_merged(&path).unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(PsoRecord::load(&path).unwrap(), merged);
        std::fs::remove_file(&path).unwrap();
        assert!(PsoRecord::load(&path).unwrap().is_empty(), "missing: empty");
    }

    #[test]
    fn saves_at_once_keep_every_entry() {
        let path =
            std::env::temp_dir().join(format!("engine_assets_pso_race_{}.ron", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::thread::scope(|scope| {
            for t in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    for k in 0..8 {
                        let mut r = PsoRecord::new();
                        r.add(Path::new("a.slang"), "A", &format!("entry{t}_{k}"));
                        r.save_merged(path).unwrap();
                    }
                });
            }
        });
        assert_eq!(
            PsoRecord::load(&path).unwrap().len(),
            64,
            "no save lost another's entries"
        );
        std::fs::remove_file(&path).unwrap();
    }
}
