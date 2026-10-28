//! The `.mgraph` file (ADR 0123): a graph in the editor's grammar (ADR 0108), its id in its
//! header, nodes keyed by short random ids, links keyed by the input they feed, written
//! sorted so a diff shows what changed.
//!
//! ```text
//! [mgraph version=3 id=7d2e5c0b9a4f41e3b8c6d5a4f3e2d1c0]
//!
//! model = "lit"
//! nodes["a3f9"].kind = "texture"
//! nodes["a3f9"].at = [80, 40]
//! nodes["a3f9"].param = "albedo"
//! nodes["a3f9"].texture = asset("rocks/rock_albedo.png", 1c2d3e4f5a6b4c7d8e9f0a1b2c3d4e5f)
//! nodes["b71c"].kind = "number"
//! nodes["b71c"].at = [80, 260]
//! nodes["b71c"].value = [0.6]
//! nodes["b71c"].param = "roughness"
//! nodes["out"].kind = "output"
//! nodes["out"].at = [640, 120]
//! links["out.base"] = "a3f9.rgba"
//! links["out.roughness"] = "b71c.value"
//! ```
//!
//! - **A node:** its `kind`, its place on the canvas (`at`), a constant's components
//!   (`value`, as many as it is wide), a parameter's name (`param`), a texture node's own
//!   image (`texture`: what the material shows until a material file gives one), its
//!   inputs' own values (`inputs["strength"] = [0.7]`), a `comment` and an Expression
//!   node's `code` (E10-3g-1). Fields at their defaults are not written.
//! - **A link** by the input it feeds, `<node>.<port>`, to the output it comes from.
//! - **A frame** (version 2, E10-3e-2b) by id: its place (`at`), its `size` and its
//!   `title` (`frames["frame1"].title = "Moss"`). Version 1 files, which have none, read.
//! - **A call** (version 3, E10-3g-3): a node of kind `call` names its `function`, the
//!   Slang `module` that declares it (an asset) and its `signature` as Slang spells it
//!   (`nodes["sw"].signature = "float3(MaterialContext ctx, float3 color)"`), so the module
//!   is generated without reading the function. Older files read as they are.

use crate::call::node_kind;
use crate::graph::{Frame, Graph, Model, Node, PortRef};
use crate::library::{self, Body};
use engine_reflect::Reflect;
use engine_scene::{AssetId, AssetPaths, AssetRef, LoadError, ValueReport};
use std::collections::BTreeMap;

/// The file's lines as the grammar reads and writes them.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct GraphFile {
    /// The output's shading model, by name (`lit`).
    pub model: String,
    /// The nodes, by id.
    pub nodes: BTreeMap<String, NodeFile>,
    /// The links, by the input they feed (`out.base`): the output they come from.
    pub links: BTreeMap<String, String>,
    /// The frames, by id.
    pub frames: BTreeMap<String, FrameFile>,
}

/// A frame's lines.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct FrameFile {
    /// Its top left on the canvas.
    pub at: [f32; 2],
    /// Its width and height.
    pub size: [f32; 2],
    /// Its title.
    pub title: String,
}

/// A node's lines.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct NodeFile {
    /// Its kind's name.
    pub kind: String,
    /// Its place on the canvas.
    pub at: [f32; 2],
    /// A constant's components.
    pub value: Vec<f32>,
    /// A parameter's name.
    pub param: String,
    /// A texture node's own image.
    pub texture: AssetRef,
    /// Its inputs' own values.
    pub inputs: BTreeMap<String, Vec<f32>>,
    /// A note on the node.
    pub comment: String,
    /// An Expression node's Slang.
    pub code: String,
    /// A call's function.
    pub function: String,
    /// The Slang module that declares it.
    pub module: AssetRef,
    /// Its signature, as Slang spells it.
    pub signature: String,
}

impl GraphFile {
    /// The kind in the file's header.
    pub const KIND: &'static str = "mgraph";
    /// The format's version: 3 adds calls (E10-3g-3), 2 frames (E10-3e-2b); 1 and 2 are
    /// read.
    pub const VERSION: u32 = 3;

    /// The file of `graph`.
    pub fn of(graph: &Graph) -> Self {
        let nodes = graph
            .nodes
            .iter()
            .map(|(id, n)| {
                // As many components as a constant is wide; none for the rest.
                let kind = node_kind(n).ok();
                let width = match kind.map(|k| k.body) {
                    Some(Body::Constant(w)) => usize::from(w),
                    _ => 0,
                };
                let node = NodeFile {
                    kind: n.kind.clone(),
                    at: n.at,
                    value: n.value[..width].to_vec(),
                    param: n.param.clone(),
                    texture: n.texture,
                    inputs: n
                        .inputs
                        .iter()
                        .map(|(name, v)| {
                            // As wide as the input; a dynamic one's value once when its
                            // components are the same (it fills them on reading).
                            let same = v.iter().all(|x| x.to_bits() == v[0].to_bits());
                            let width =
                                kind.and_then(|k| k.input(name)).map_or(4, |i| match i.ty {
                                    library::PortType::Float(w) => usize::from(w),
                                    library::PortType::Dynamic if same => 1,
                                    library::PortType::Dynamic => 4,
                                });
                            (name.clone(), v[..width].to_vec())
                        })
                        .collect(),
                    comment: n.comment.clone(),
                    code: n.code.clone(),
                    function: n.function.clone(),
                    module: n.module,
                    signature: n.signature.clone(),
                };
                (id.clone(), node)
            })
            .collect();
        let links = graph
            .links
            .iter()
            .map(|(to, from)| (to.to_string(), from.to_string()))
            .collect();
        let frames = graph
            .frames
            .iter()
            .map(|(id, f)| {
                let frame = FrameFile {
                    at: f.at,
                    size: f.size,
                    title: f.title.clone(),
                };
                (id.clone(), frame)
            })
            .collect();
        Self {
            model: graph.model.name().to_owned(),
            nodes,
            links,
            frames,
        }
    }

    /// The graph it holds.
    ///
    /// # Errors
    ///
    /// A model or a link that cannot be read (what the generator would refuse, a node of
    /// no kind or a link into nothing, is the generator's to say, at its node).
    pub fn graph(&self) -> Result<Graph, String> {
        let model = Model::from_name(&self.model)
            .ok_or_else(|| format!("no shading model `{}`", self.model))?;
        let mut graph = Graph::new(model);
        graph.nodes.clear();
        for (id, n) in &self.nodes {
            let mut node = Node::new(&n.kind, n.at);
            for (k, v) in n.value.iter().take(4).enumerate() {
                node.value[k] = *v;
            }
            node.param.clone_from(&n.param);
            node.texture = n.texture;
            node.comment.clone_from(&n.comment);
            node.code.clone_from(&n.code);
            node.function.clone_from(&n.function);
            node.module = n.module;
            node.signature.clone_from(&n.signature);
            for (name, v) in &n.inputs {
                let mut value = [0.0; 4];
                for (k, x) in v.iter().take(4).enumerate() {
                    value[k] = *x;
                }
                // A scalar written for a wider input fills it.
                if v.len() == 1 {
                    value = [v[0]; 4];
                }
                node.inputs.insert(name.clone(), value);
            }
            graph.nodes.insert(id.clone(), node);
        }
        for (to, from) in &self.links {
            let to_ref =
                PortRef::parse(to).ok_or_else(|| format!("`{to}` is not `<node>.<port>`"))?;
            let from_ref =
                PortRef::parse(from).ok_or_else(|| format!("`{from}` is not `<node>.<port>`"))?;
            graph.links.insert(to_ref, from_ref);
        }
        for (id, f) in &self.frames {
            let frame = Frame {
                at: f.at,
                size: f.size,
                title: f.title.clone(),
            };
            graph.frames.insert(id.clone(), frame);
        }
        Ok(graph)
    }

    /// The file's text with its id, its references written with their paths from `assets`.
    pub fn to_text(&self, id: AssetId, assets: Option<&AssetPaths>) -> String {
        let mut all = String::new();
        engine_scene::write_value(Self::KIND, Self::VERSION, Some(id), self, assets, &mut all);
        // Fields at their defaults are left out: a reader takes them so.
        let mut out = String::with_capacity(all.len());
        for line in all.lines() {
            let default = [" = []", " = \"\"", " = none", " = {}"]
                .iter()
                .any(|end| line.ends_with(end) && line.starts_with("nodes["));
            if !default {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// A graph file from its text, and what reading it reported (its id, the paths its
    /// references were written with, lines it skipped).
    ///
    /// # Errors
    ///
    /// Not a graph file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = engine_scene::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::graph::OUTPUT;

    fn moss() -> (Graph, AssetPaths, AssetId) {
        let image = AssetId::new();
        let mut paths = AssetPaths::new();
        paths.insert(AssetRef::file(image), "rocks/rock_albedo.png");
        let mut g = Graph::new(Model::Lit);
        g.nodes.get_mut(OUTPUT).unwrap().at = [640.0, 120.0];
        g.add(
            "a3f9",
            Node::new("texture", [80.0, 40.0])
                .named("albedo")
                .with_texture(AssetRef::file(image)),
        )
        .add(
            "b71c",
            Node::new("number", [80.0, 260.0])
                .with_value([0.6, 0.0, 0.0, 0.0])
                .named("roughness"),
        )
        .add(
            "c2d8",
            Node::new("normal_map", [300.0, 40.0]).with_input("strength", [0.7; 4]),
        )
        .add(
            "d5e0",
            Node::new("dot", [300.0, 200.0]).with_input("b", [0.0, 1.0, 0.0, 0.0]),
        )
        .link(PortRef::new("a3f9", "rgba"), PortRef::new(OUTPUT, "base"))
        .link(
            PortRef::new("b71c", "value"),
            PortRef::new(OUTPUT, "roughness"),
        );
        g.nodes.get_mut("d5e0").unwrap().comment = String::from("up-facing");
        (g, paths, AssetId::new())
    }

    #[test]
    fn a_graph_reads_back_what_it_writes() {
        let (g, paths, id) = moss();
        let text = GraphFile::of(&g).to_text(id, Some(&paths));
        let (file, report) = GraphFile::parse(&text).unwrap();
        assert_eq!(file.graph().unwrap(), g, "{text}");
        assert_eq!(report.id, Some(id));
        // The texture's path as written.
        let (image, _) = paths.iter().next().unwrap();
        assert_eq!(report.assets.get(image), Some("rocks/rock_albedo.png"));
    }

    #[test]
    fn the_file_holds_only_what_differs_from_the_defaults_sorted() {
        let (g, paths, id) = moss();
        let text = GraphFile::of(&g).to_text(id, Some(&paths));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("[mgraph version=3 id={id}]"));
        for line in [
            "model = \"lit\"",
            "nodes[\"a3f9\"].kind = \"texture\"",
            "nodes[\"a3f9\"].at = [80, 40]",
            "nodes[\"a3f9\"].param = \"albedo\"",
            "nodes[\"b71c\"].value = [0.6]",
            "nodes[\"c2d8\"].inputs[\"strength\"] = [0.7]",
            "nodes[\"d5e0\"].inputs[\"b\"] = [0, 1, 0, 0]",
            "nodes[\"d5e0\"].comment = \"up-facing\"",
            "links[\"out.base\"] = \"a3f9.rgba\"",
            "links[\"out.roughness\"] = \"b71c.value\"",
        ] {
            assert!(lines.contains(&line), "{line}\n{text}");
        }
        assert!(
            lines.iter().any(|l| l.starts_with("nodes[\"a3f9\"].texture = asset(\"rocks/rock_albedo.png\"")),
            "{text}"
        );
        // Defaults are not written: no value for a texture, no parameter, texture, inputs or
        // comment where a node has none.
        for gone in [
            "nodes[\"a3f9\"].value",
            "nodes[\"c2d8\"].param",
            "nodes[\"c2d8\"].texture",
            "nodes[\"b71c\"].inputs",
            "nodes[\"b71c\"].comment",
        ] {
            assert!(!text.contains(gone), "{gone}\n{text}");
        }
        // Sorted: the nodes by id, then the links.
        let at = |needle: &str| text.find(needle).unwrap();
        assert!(at("nodes[\"a3f9\"]") < at("nodes[\"b71c\"]"));
        assert!(at("nodes[\"d5e0\"]") < at("nodes[\"out\"]"));
        assert!(at("nodes[\"out\"]") < at("links["));
    }

    /// E10-3e-2b: frames are the file's version 2, written by id after the links and read
    /// back; a version 1 file, which has none, still reads; they are nothing of the code.
    #[test]
    fn frames_read_back_and_a_version_1_file_still_reads() {
        let (mut g, paths, id) = moss();
        let plain = crate::generate(&g, "Moss", crate::Build::Saved)
            .unwrap()
            .text;
        g.frames.insert(
            String::from("frame1"),
            Frame {
                at: [60.0, 0.0],
                size: [300.0, 200.0],
                title: String::from("Moss on top"),
            },
        );
        let text = GraphFile::of(&g).to_text(id, Some(&paths));
        for line in [
            "frames[\"frame1\"].at = [60, 0]",
            "frames[\"frame1\"].size = [300, 200]",
            "frames[\"frame1\"].title = \"Moss on top\"",
        ] {
            assert!(text.lines().any(|l| l == line), "{line}\n{text}");
        }
        let (file, _) = GraphFile::parse(&text).unwrap();
        assert_eq!(file.graph().unwrap(), g);
        assert_eq!(
            crate::generate(&g, "Moss", crate::Build::Saved)
                .unwrap()
                .text,
            plain
        );
        // The same graph written as version 1 was (no frames): read, framed by nothing.
        g.frames.clear();
        let old = GraphFile::of(&g)
            .to_text(id, Some(&paths))
            .replace("[mgraph version=3", "[mgraph version=1");
        let (file, _) = GraphFile::parse(&old).unwrap();
        assert_eq!(file.graph().unwrap(), g);
    }

    /// E10-3g-3: a call's function, module and signature are written (its module an asset
    /// with its path) and read back; a call no signature makes is the generator's to say.
    #[test]
    fn a_call_is_written_with_its_function_module_and_signature() {
        let (mut g, mut paths, id) = moss();
        let module = AssetRef::file(AssetId::new());
        paths.insert(module, "shaders/mapping.slang");
        let mut call = Node::new(crate::call::CALL_KIND, [300.0, 300.0])
            .with_call("swirl", "float3(MaterialContext ctx, float3 color)");
        call.module = module;
        g.add("sw", call);
        let text = GraphFile::of(&g).to_text(id, Some(&paths));
        for line in [
            "nodes[\"sw\"].kind = \"call\"",
            "nodes[\"sw\"].function = \"swirl\"",
            "nodes[\"sw\"].signature = \"float3(MaterialContext ctx, float3 color)\"",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert!(
            text.contains("nodes[\"sw\"].module = asset(\"shaders/mapping.slang\", "),
            "{text}"
        );
        let (file, report) = GraphFile::parse(&text).unwrap();
        assert_eq!(file.graph().unwrap(), g);
        assert_eq!(report.assets.get(module), Some("shaders/mapping.slang"));
    }

    #[test]
    fn what_cannot_be_read_is_said() {
        let (g, paths, id) = moss();
        let text = GraphFile::of(&g).to_text(id, Some(&paths));
        // A model, a link.
        let plastic = text.replace("model = \"lit\"", "model = \"plastic\"");
        let (file, _) = GraphFile::parse(&plastic).unwrap();
        assert!(file.graph().unwrap_err().contains("plastic"));
        let mut file = GraphFile::of(&g);
        file.links
            .insert(String::from("outbase"), String::from("a3f9.rgba"));
        assert!(file.graph().unwrap_err().contains("outbase"));
        // A newer file, another kind of file.
        let newer = text.replace("[mgraph version=3", "[mgraph version=4");
        assert!(GraphFile::parse(&newer).is_err());
        let other = text.replace("[mgraph version=3", "[material version=3");
        assert!(GraphFile::parse(&other).is_err());
        // The database knows the file, its id inside.
        let kind = engine_scene::AssetKind::of(std::path::Path::new("rocks/moss.mgraph"));
        assert_eq!(kind, Some(engine_scene::AssetKind::Graph));
        assert!(engine_scene::AssetKind::Graph.own_id());
    }
}
