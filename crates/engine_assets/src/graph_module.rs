//! A material graph made a Slang module (ADR 0123's E10-3c): where a material file names a
//! `.mgraph` as its shader, the graph's module is generated into the project's cache and
//! compiled as a module is, by the player, the cook and the editor alike. The graph's own
//! values (its texture nodes' images, its named numbers) are its material's defaults, under
//! the material file's.

use crate::material_file::{MaterialFileError, ParamValue, TextureEncoding};
use engine_material_graph::{Build, Graph, GraphFile, ParamType, generate};
use engine_scene::AssetRef;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The type a graph's module declares: a material file of a graph names it.
pub const GRAPH_TYPE: &str = "GraphMaterial";

/// Where graphs' modules are generated, under the project's folder (the editor's session
/// folder, which version control leaves out).
pub const GRAPH_CACHE: &str = ".ofelia/cache/graphs";

/// A graph's module and the graph's own values.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphModule {
    /// The generated module.
    pub module: PathBuf,
    /// The graph's own values by parameter: its named numbers', and its texture nodes'
    /// images (each with how it is encoded).
    pub defaults: BTreeMap<String, ParamValue>,
    /// Its texture nodes' images by parameter, as assets.
    pub textures: BTreeMap<String, AssetRef>,
}

/// Whether `path` is a material graph's file.
pub fn is_graph(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mgraph"))
}

/// The module of the graph at `path` (as the material file resolves it), generated into
/// `root`'s cache as `<graph id>.slang`, written only when its text changed (an unchanged
/// graph compiles nothing again), with the graph's own values.
///
/// # Errors
///
/// The graph cannot be read, cannot be generated (said at its node), or its module cannot
/// be written.
pub fn graph_module(path: &Path, root: &Path) -> Result<GraphModule, MaterialFileError> {
    let error = |message: String| MaterialFileError {
        path: Some(path.to_owned()),
        message,
    };
    let text = std::fs::read_to_string(path).map_err(|e| error(e.to_string()))?;
    let (file, report) = GraphFile::parse(&text).map_err(|e| error(e.to_string()))?;
    let graph = file.graph().map_err(error)?;
    let generated = generate(&graph, GRAPH_TYPE, Build::Saved).map_err(|e| error(e.to_string()))?;
    let id = report
        .id
        .map_or_else(|| format!("{:016x}", path_hash(path)), |id| id.to_string());
    let module = root.join(GRAPH_CACHE).join(format!("{id}.slang"));
    if std::fs::read_to_string(&module).ok().as_deref() != Some(generated.text.as_str()) {
        module
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&module, &generated.text))
            .map_err(|e| error(format!("{}: {e}", module.display())))?;
    }
    let mut defaults = BTreeMap::new();
    let mut textures = BTreeMap::new();
    for p in &generated.params {
        let Some(node) = graph.nodes.get(&p.node) else {
            continue;
        };
        match p.ty {
            ParamType::Texture => {
                let Some(at) = report.assets.get(node.texture).filter(|a| !a.is_empty()) else {
                    continue;
                };
                let image = if Path::new(at).is_absolute() {
                    PathBuf::from(at)
                } else {
                    root.join(at)
                };
                defaults.insert(
                    p.name.clone(),
                    ParamValue::Texture(image, encoding(&graph, &p.node)),
                );
                textures.insert(p.name.clone(), node.texture);
            }
            ParamType::Float(1) => {
                defaults.insert(p.name.clone(), ParamValue::Number(f64::from(p.value[0])));
            }
            ParamType::Float(w) => {
                let list = p.value[..usize::from(w)]
                    .iter()
                    .map(|&v| ParamValue::Number(f64::from(v)))
                    .collect();
                defaults.insert(p.name.clone(), ParamValue::List(list));
            }
        }
    }
    Ok(GraphModule {
        module,
        defaults,
        textures,
    })
}

/// How a texture node's image is encoded: as a normal map where it feeds one, else as a
/// colour (a material file's `textures["name"].encoding` says otherwise).
fn encoding(graph: &Graph, node: &str) -> TextureEncoding {
    let feeds_normal_map = graph.links.iter().any(|(to, from)| {
        from.node == node
            && graph
                .nodes
                .get(&to.node)
                .is_some_and(|n| n.kind == "normal_map")
    });
    if feeds_normal_map {
        TextureEncoding::Normal
    } else {
        TextureEncoding::Color
    }
}

/// A file without an id named by its path.
fn path_hash(path: &Path) -> u64 {
    // FNV-1a over the path's text: stable across runs.
    path.to_string_lossy()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::material_file::MaterialFile;
    use engine_scene::AssetId;

    fn moss(graph: AssetId, image: AssetId, extra: &str) -> String {
        format!(
            "[mgraph version=1 id={graph}]\n\nmodel = \"lit\"\n\
             nodes[\"t\"].kind = \"texture\"\nnodes[\"t\"].param = \"bumps\"\n\
             nodes[\"t\"].texture = asset(\"textures/bumps.png\", {image})\n\
             nodes[\"nm\"].kind = \"normal_map\"\n\
             nodes[\"r\"].kind = \"number\"\nnodes[\"r\"].value = [0.6]\n\
             nodes[\"r\"].param = \"roughness\"\n\
             nodes[\"c\"].kind = \"color\"\nnodes[\"c\"].value = [0.2, 0.4, 0.1, 1]\n\
             nodes[\"c\"].param = \"tint\"\n\
             nodes[\"out\"].kind = \"output\"\n\
             links[\"nm.color\"] = \"t.rgba\"\nlinks[\"out.normal\"] = \"nm.normal\"\n\
             links[\"out.roughness\"] = \"r.value\"\nlinks[\"out.base\"] = \"c.rgba\"\n{extra}"
        )
    }

    #[test]
    fn a_material_naming_a_graph_is_its_module_with_the_graphs_values_under_its_own() {
        let dir = std::env::temp_dir().join(format!("graph_module_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("materials")).unwrap();
        let (graph, image) = (AssetId::new(), AssetId::new());
        std::fs::write(dir.join("materials/moss.mgraph"), moss(graph, image, "")).unwrap();
        let text = format!(
            "[material version=2 id={}]\n\nshader = asset(\"materials/moss.mgraph\", {graph})\n\
             type = \"GraphMaterial\"\nnumbers[\"roughness\"] = 0.3\nlook = Own\n",
            AssetId::new()
        );
        let (file, report) = MaterialFile::parse(&text).unwrap();
        let typed = file.typed(&report.assets, &dir).unwrap().unwrap();
        // Its module, generated into the cache under the graph's id.
        let module = dir.join(GRAPH_CACHE).join(format!("{graph}.slang"));
        assert_eq!(typed.shader, module);
        assert_eq!(typed.type_name, GRAPH_TYPE);
        let source = std::fs::read_to_string(&module).unwrap();
        assert!(
            source.contains("struct GraphMaterial : IMaterial"),
            "{source}"
        );
        // The file's roughness over the graph's; the graph's tint and its texture's image,
        // a normal map's, where the file gives none.
        assert_eq!(
            typed.params["roughness"],
            ParamValue::Number(f64::from(0.3f32))
        );
        let tint = [0.2f32, 0.4, 0.1, 1.0].map(|v| ParamValue::Number(f64::from(v)));
        assert_eq!(typed.params["tint"], ParamValue::List(tint.to_vec()));
        assert_eq!(
            typed.params["bumps"],
            ParamValue::Texture(dir.join("textures/bumps.png"), TextureEncoding::Normal)
        );
        assert_eq!(typed.textures["bumps"], AssetRef::file(image));
        // Unchanged, it is not written again (nothing compiles again).
        let written = std::fs::metadata(&module).unwrap().modified().unwrap();
        std::thread::yield_now();
        let again = file.typed(&report.assets, &dir).unwrap().unwrap();
        assert_eq!(again, typed);
        assert_eq!(
            std::fs::metadata(&module).unwrap().modified().unwrap(),
            written
        );
        // A graph that cannot be generated: said with its node.
        std::fs::write(
            dir.join("materials/moss.mgraph"),
            moss(graph, image, "links[\"out.metallic\"] = \"gone.value\"\n"),
        )
        .unwrap();
        let e = file.typed(&report.assets, &dir).unwrap_err();
        assert!(e.message.contains("gone"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
