//! A build's material types compiled ahead (ADR 0126's G2): the types the build's material
//! files name, compiled into `data/materials.pack` so that the built game's player makes
//! their pipelines without Slang (`engine_assets::material_pack`).
//!
//! - **The types** ([`material_types`]): each typed material file in the plan read with its
//!   bases, as the player reads it; a hand-written module's type, or a graph's module (made
//!   in the editor's cache, `.ofelia/cache/graphs/`, and added to the plan: the player
//!   reads it to know a graph is unchanged). Each type once.
//! - **The key** ([`material_key`]): a hash of the engine's shader folder (`forward.slang`
//!   and what it includes), the types, and every Slang module in the plan. A pack of the same
//!   key is kept; another is compiled again, every type's fragment entry points and its
//!   parameters' layout.
//! - **The PSO record** (`game.pso.ron`, G5a): a type it names is compiled for the entry
//!   points it lists only, those the editor's Play drew; a type it does not name whole. The
//!   record is part of the key.

use std::collections::BTreeSet;
use std::path::Path;

use engine_assets::material_file::{MaterialFile, MaterialTypeBuilder as _, SlangMaterialTypes};
use engine_assets::material_pack::{MaterialPack, PackedType};

use crate::{BuildError, Plan, extension, walk_all};

/// A material type a build's material files name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MaterialType {
    /// Its module's path under the game's folder, `/` between folders.
    pub module: String,
    /// Its name in the module.
    pub type_name: String,
}

/// The material types the plan's material files name, each once, sorted; a graph's module
/// added to the plan.
///
/// # Errors
///
/// A material file that cannot be read or resolved (a graph that cannot be made a module),
/// or whose shader is outside the game's folder.
pub fn material_types(root: &Path, plan: &mut Plan) -> Result<Vec<MaterialType>, BuildError> {
    let mut types = BTreeSet::new();
    let files: Vec<String> = plan
        .files
        .iter()
        .filter(|f| extension(f) == engine_assets::material_file::MATERIAL_FILE_EXTENSION)
        .cloned()
        .collect();
    for path in files {
        let failed = |message: String| BuildError::new(format!("{path}: {message}"));
        let resolved = MaterialFile::load_with_bases(&root.join(&path), root)
            .map_err(|e| failed(e.message))?;
        let Some(typed) = resolved
            .file
            .typed(&resolved.report.assets, root)
            .map_err(|e| failed(e.message))?
        else {
            continue;
        };
        let module = under(root, &typed.shader).ok_or_else(|| {
            failed(format!(
                "its shader {} is outside the game's folder",
                typed.shader.display()
            ))
        })?;
        if module.starts_with('.') {
            plan.files.insert(module.clone());
        }
        types.insert(MaterialType {
            module,
            type_name: typed.type_name,
        });
    }
    Ok(types.into_iter().collect())
}

/// `full` as a path under `root`, `/` between folders, `..` resolved; `None` outside it.
fn under(root: &Path, full: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for part in full.strip_prefix(root).ok()?.components() {
        match part {
            std::path::Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// The hash of what `types` compile from: the engine's shader folder `shaders`, the types,
/// and the plan's Slang modules (under `root`).
pub fn material_key(root: &Path, shaders: &Path, plan: &Plan, types: &[MaterialType]) -> u128 {
    let mut bytes = Vec::from(&b"ofeliaengine material pack 1\0"[..]);
    let mut add = |name: &str, file: &Path| {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&std::fs::read(file).unwrap_or_default());
        bytes.push(0);
    };
    let mut engine = Vec::new();
    walk_all(shaders, shaders, &mut engine);
    engine.sort();
    for file in &engine {
        add(file, &shaders.join(file));
    }
    for file in plan.files.iter().filter(|f| extension(f) == "slang") {
        add(file, &root.join(file));
    }
    add(crate::PSO_RECORD, &root.join(crate::PSO_RECORD));
    for t in types {
        bytes.extend_from_slice(t.module.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(t.type_name.as_bytes());
        bytes.push(0);
    }
    engine_core::hash::content_hash128(&bytes)
}

/// `types` (of the game at `root`) compiled with the engine's `forward.slang` in `shaders`
/// into the pack at `pack`, its key `key`, unless the pack there has that key. Whether it
/// compiled.
///
/// # Errors
///
/// Slang not found, a type that does not compile (its module, its name, Slang's message),
/// or the pack not written.
pub fn material_pack(
    root: &Path,
    shaders: &Path,
    types: &[MaterialType],
    key: u128,
    pack: &Path,
) -> Result<bool, BuildError> {
    if MaterialPack::read_key(pack) == Some(key) {
        return Ok(false);
    }
    let mut builder = SlangMaterialTypes::new(shaders)
        .map_err(|e| BuildError::new(format!("material types: no Slang compiler ({e})")))?;
    let record = engine_assets::pso_record::PsoRecord::load(&root.join(crate::PSO_RECORD))
        .map_err(|e| BuildError::new(format!("{}: {e}", crate::PSO_RECORD)))?;
    if !record.is_empty() {
        builder = builder.with_pso_record(record);
    }
    let mut out = MaterialPack {
        key,
        types: Vec::with_capacity(types.len()),
    };
    for t in types {
        let module = root.join(&t.module);
        let failed = |e: String| BuildError::new(format!("{}: {}: {e}", t.module, t.type_name));
        let layout = builder.layout(&module, &t.type_name).map_err(failed)?;
        let entries = builder.entries(&module, &t.type_name).map_err(failed)?;
        out.types.push(PackedType {
            module: t.module.clone(),
            type_name: t.type_name.clone(),
            layout,
            entries,
        });
    }
    let bytes = out.to_bytes().map_err(BuildError::new)?;
    if let Some(parent) = pack.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(pack, bytes).map_err(|e| BuildError::new(format!("{}: {e}", pack.display())))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{project, write};

    const SHADERS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../engine_render/shaders");

    /// A hand-written type named by a material and by its variant (one type), a standard
    /// material (none), and a graph's material (its module made and planned).
    fn materials(root: &Path) -> Plan {
        write(
            root,
            "materials/stripes.slang",
            include_str!("../../../tests/golden/materials/stripes.slang"),
        );
        write(
            root,
            "materials/red.material",
            "[material version=2 id=0000000000000000000000000000000a]\n\nshader = asset(\"materials/stripes.slang\", 0000000000000000000000000000000b)\n\
             type = \"StripesMaterial\"\nvectors[\"color_a\"] = [0.9, 0.1, 0.1, 1]\n",
        );
        write(
            root,
            "materials/dark_red.material",
            "[material version=2 id=0000000000000000000000000000000c]\n\nbase = asset(\"materials/red.material\", 0000000000000000000000000000000a)\n\
             numbers[\"roughness\"] = 0.9\n",
        );
        write(
            root,
            "materials/plain.material",
            "[material version=2 id=0000000000000000000000000000000d]\n\nroughness = 0.5\n",
        );
        write(
            root,
            "materials/glow.mgraph",
            "[mgraph version=1 id=0000000000000000000000000000000e]\n\nmodel = \"unlit\"\n\
             nodes[\"c\"].kind = \"color\"\nnodes[\"c\"].value = [0.9, 0.5, 0.1, 1]\n\
             nodes[\"out\"].kind = \"output\"\nlinks[\"out.base\"] = \"c.rgba\"\n",
        );
        write(
            root,
            "materials/glow.material",
            "[material version=2 id=0000000000000000000000000000000f]\n\nshader = asset(\"materials/glow.mgraph\", \
             0000000000000000000000000000000e)\ntype = \"GraphMaterial\"\n",
        );
        let mut plan = Plan::default();
        for f in [
            "materials/stripes.slang",
            "materials/red.material",
            "materials/dark_red.material",
            "materials/plain.material",
            "materials/glow.mgraph",
            "materials/glow.material",
        ] {
            plan.files.insert(String::from(f));
        }
        plan
    }

    #[test]
    fn material_files_name_their_types_once() {
        let root = project("material-types");
        let mut plan = materials(&root);
        let types = material_types(&root, &mut plan).unwrap();
        let graph = ".ofelia/cache/graphs/0000000000000000000000000000000e.slang";
        assert_eq!(
            types,
            [
                MaterialType {
                    module: String::from(graph),
                    type_name: String::from("GraphMaterial"),
                },
                MaterialType {
                    module: String::from("materials/stripes.slang"),
                    type_name: String::from("StripesMaterial"),
                },
            ]
        );
        assert!(plan.files.contains(graph), "the graph's module goes in");
        assert!(root.join(graph).is_file());
        // The key follows the modules' text.
        let key = material_key(&root, Path::new(SHADERS), &plan, &types);
        assert_eq!(key, material_key(&root, Path::new(SHADERS), &plan, &types));
        write(&root, "materials/stripes.slang", "// changed");
        assert_ne!(key, material_key(&root, Path::new(SHADERS), &plan, &types));
        // A shader outside the game's folder is an error naming the file.
        write(
            &root,
            "materials/far.material",
            "[material version=2 id=00000000000000000000000000000010]\n\nshader = asset(\"../far.slang\", 00000000000000000000000000000011)\n\
             type = \"Far\"\n",
        );
        plan.files.insert(String::from("materials/far.material"));
        let e = material_types(&root, &mut plan).unwrap_err();
        assert!(e.what.starts_with("materials/far.material: "), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The types compiled into a pack (where Slang is at hand): each with its layout and its
    /// entry points; a pack of the same key kept.
    #[test]
    fn material_types_compile_into_a_pack_once() {
        if engine_shader::SlangLibrary::load().is_err() {
            println!("skipping: no Slang (set ENGINE_SLANG_LIBRARY)");
            return;
        }
        let root = project("material-pack");
        let mut plan = materials(&root);
        let types = material_types(&root, &mut plan).unwrap();
        let shaders = Path::new(SHADERS);
        let key = material_key(&root, shaders, &plan, &types);
        let pack = root.join("out/materials.pack");
        assert!(material_pack(&root, shaders, &types, key, &pack).unwrap());
        let read = MaterialPack::load(&pack).unwrap().unwrap();
        assert_eq!(read.key, key);
        let stripes = read
            .find("materials/stripes.slang", "StripesMaterial")
            .unwrap();
        assert!(
            stripes.layout.field("color_a").is_some(),
            "{:?}",
            stripes.layout
        );
        assert!(stripes.entries.iter().any(|(n, _)| n == "fragmentMain"));
        assert!(stripes.entries.iter().all(|(_, w)| w[0] == 0x0723_0203));
        assert!(read.types.iter().any(|t| t.type_name == "GraphMaterial"));
        println!(
            "{} types, {} entry points, {} KB",
            read.types.len(),
            read.types.iter().map(|t| t.entries.len()).sum::<usize>(),
            std::fs::metadata(&pack).unwrap().len() / 1024
        );
        assert!(
            !material_pack(&root, shaders, &types, key, &pack).unwrap(),
            "kept"
        );
        // A PSO record naming the stripes' one entry point: only that compiled for it, the
        // graph's type (not named) whole; the record moves the key.
        let mut record = engine_assets::pso_record::PsoRecord::new();
        record.add(
            &root.join("materials/stripes.slang"),
            "StripesMaterial",
            "fragmentMain",
        );
        record.save_merged(&root.join(crate::PSO_RECORD)).unwrap();
        let recorded = material_key(&root, shaders, &plan, &types);
        assert_ne!(recorded, key);
        assert!(material_pack(&root, shaders, &types, recorded, &pack).unwrap());
        let read = MaterialPack::load(&pack).unwrap().unwrap();
        let stripes = read
            .find("materials/stripes.slang", "StripesMaterial")
            .unwrap();
        assert_eq!(
            stripes
                .entries
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["fragmentMain"]
        );
        let graph = read
            .types
            .iter()
            .find(|t| t.type_name == "GraphMaterial")
            .unwrap();
        assert!(
            graph.entries.len() > 1,
            "a type the record does not name: whole"
        );
        println!(
            "with the record: {} KB",
            std::fs::metadata(&pack).unwrap().len() / 1024
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
