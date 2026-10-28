//! A material graph made a Slang module (ADR 0123's E10-3c): where a material file names a
//! `.mgraph` as its shader, the graph's module is generated into the project's cache and
//! compiled as a module is, by the player, the cook and the editor alike. The graph's own
//! values (its texture nodes' images, its named numbers) are its material's defaults, under
//! the material file's.

use crate::material_file::{MaterialFileError, ParamValue, TextureEncoding};
use engine_material_graph::{
    Build, FUNCTION_KIND, Graph, GraphFile, ParamType, Sources, generate_with,
};
use engine_scene::{AssetPaths, AssetRef};
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
    /// The function files its calls read (E10-3g-2): a change to one makes it again.
    pub functions: Vec<PathBuf>,
}

/// The functions a graph's calls name, and those theirs name (ADR 0123's E10-3g-2), read
/// from their files: their graphs by file, every path their files and the graph's wrote (so
/// a function's own `[Node]` calls' modules and images are found), and the files read.
#[derive(Debug, Default)]
pub struct Functions {
    /// The functions' graphs, by file.
    pub graphs: BTreeMap<AssetRef, Graph>,
    /// The paths the graph's file and the functions' files wrote.
    pub paths: AssetPaths,
    /// The functions' files read, as found.
    pub files: Vec<PathBuf>,
}

/// The [`Functions`] of `graph`, whose file wrote `paths`, under `root`.
///
/// # Errors
///
/// A function's file that cannot be read.
pub fn read_functions(
    graph: &Graph,
    paths: &AssetPaths,
    root: &Path,
) -> Result<Functions, MaterialFileError> {
    let mut out = Functions::default();
    out.paths.extend(paths);
    let calls = |g: &Graph, written: &AssetPaths| -> Vec<(AssetRef, String)> {
        g.nodes
            .values()
            .filter(|n| n.kind == FUNCTION_KIND)
            .filter_map(|n| Some((n.module, written.get(n.module)?.to_owned())))
            .collect()
    };
    let mut todo = calls(graph, paths);
    while let Some((asset, at)) = todo.pop() {
        if out.graphs.contains_key(&asset) || at.is_empty() {
            continue;
        }
        let full = if Path::new(&at).is_absolute() {
            PathBuf::from(&at)
        } else {
            root.join(&at)
        };
        let error = |message: String| MaterialFileError {
            path: Some(full.clone()),
            message,
        };
        let text = std::fs::read_to_string(&full).map_err(|e| error(e.to_string()))?;
        let (file, report) = GraphFile::parse_function(&text).map_err(|e| error(e.to_string()))?;
        let function = file.function().map_err(error)?;
        out.paths.extend(&report.assets);
        todo.extend(calls(&function, &report.assets));
        out.files.push(full);
        out.graphs.insert(asset, function);
    }
    Ok(out)
}

/// How a graph's module includes a Slang module its calls name (ADR 0123's E10-3g-3): from
/// the cache's folder up to the project's, then the module's path as the graph's file
/// writes it (an absolute path as it is), so the text is the same on every machine.
pub fn include_path(module: &str) -> String {
    let module = module.replace('\\', "/");
    if Path::new(&module).is_absolute() {
        module
    } else {
        format!("{}{module}", "../".repeat(GRAPH_CACHE.split('/').count()))
    }
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
    let functions = read_functions(&graph, &report.assets, root)?;
    let modules = |m| {
        functions
            .paths
            .get(m)
            .filter(|p| !p.is_empty())
            .map(include_path)
    };
    let found = |f| functions.graphs.get(&f).cloned();
    let sources = Sources {
        modules: &modules,
        functions: &found,
    };
    let generated = generate_with(&graph, GRAPH_TYPE, Build::Saved, &sources)
        .map_err(|e| error(e.to_string()))?;
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
    // The graph written: a function's nodes among its own (E10-3g-2), their images in the
    // functions' files.
    let written = &generated.expanded;
    for p in &generated.params {
        let Some(node) = written.nodes.get(&p.node) else {
            continue;
        };
        match p.ty {
            ParamType::Texture => {
                let Some(at) = functions.paths.get(node.texture).filter(|a| !a.is_empty()) else {
                    continue;
                };
                let image = if Path::new(at).is_absolute() {
                    PathBuf::from(at)
                } else {
                    root.join(at)
                };
                defaults.insert(
                    p.name.clone(),
                    ParamValue::Texture(image, encoding(written, &p.node)),
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
        functions: functions.files,
    })
}

/// How a texture node's image is encoded: as a normal map where it feeds one, else as a
/// colour (a material file's `textures["name"].encoding` says otherwise).
pub fn encoding(graph: &Graph, node: &str) -> TextureEncoding {
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

    /// E10-3g-2: a graph calling a function: its module made with the function's nodes,
    /// the function's file among those read (a change to it makes the module again); a
    /// function's file that cannot be read fails the module with its path.
    #[test]
    fn a_graph_calling_a_function_is_made_with_its_nodes() {
        use engine_material_graph::function::{FUNCTION_KIND, input_kind, output_kind};
        use engine_material_graph::{Model, Node, PortRef, signature_of};
        let dir =
            std::env::temp_dir().join(format!("graph_module_function_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        let mut f = Graph::new(Model::Lit);
        f.nodes.clear();
        f.add("i", Node::new(input_kind(3), [0.0, 0.0]).named("color"))
            .add("n", Node::new("one_minus", [150.0, 0.0]))
            .add(
                "o",
                Node::new(output_kind(3), [300.0, 0.0]).named("inverted"),
            )
            .link(PortRef::new("i", "value"), PortRef::new("n", "in"))
            .link(PortRef::new("n", "out"), PortRef::new("o", "value"));
        let fid = AssetId::new();
        std::fs::write(
            dir.join("lib/invert.mfunc"),
            GraphFile::of(&f).to_function_text(fid, None),
        )
        .unwrap();
        let mut g = Graph::new(Model::Lit);
        let mut call = Node::new(FUNCTION_KIND, [200.0, 0.0])
            .with_call("invert", &signature_of(&f).unwrap().to_string());
        call.module = AssetRef::file(fid);
        g.add(
            "c",
            Node::new("color", [0.0; 2]).with_value([0.2, 0.4, 0.6, 1.0]),
        )
        .add("inv", call)
        .link(PortRef::new("c", "rgba"), PortRef::new("inv", "color"))
        .link(PortRef::new("inv", "inverted"), PortRef::new("out", "base"));
        let mut paths = AssetPaths::new();
        paths.insert(AssetRef::file(fid), "lib/invert.mfunc");
        let graph_path = dir.join("tinted.mgraph");
        std::fs::write(
            &graph_path,
            GraphFile::of(&g).to_text(AssetId::new(), Some(&paths)),
        )
        .unwrap();
        let module = graph_module(&graph_path, &dir).unwrap();
        assert_eq!(module.functions, [dir.join("lib/invert.mfunc")]);
        let text = std::fs::read_to_string(&module.module).unwrap();
        assert!(
            text.contains("1.0 - "),
            "the function's node written: {text}"
        );
        std::fs::write(dir.join("lib/invert.mfunc"), "not a function").unwrap();
        let e = graph_module(&graph_path, &dir).unwrap_err();
        assert_eq!(
            e.path.as_deref(),
            Some(dir.join("lib/invert.mfunc").as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// E10-3g-3: a call's module is included from the cache's folder, the same on every
    /// machine for a path in the project; an absolute path as it is.
    #[test]
    fn a_calls_module_is_included_from_the_cache() {
        assert_eq!(
            include_path("shaders/mapping.slang"),
            "../../../shaders/mapping.slang"
        );
        assert_eq!(
            include_path(r"shaders\mapping.slang"),
            "../../../shaders/mapping.slang"
        );
        assert_eq!(include_path("C:/lib/noise.slang"), "C:/lib/noise.slang");
    }

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
