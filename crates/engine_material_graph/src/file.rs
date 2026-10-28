//! The `.mgraph` file (ADR 0123): a graph in the editor's grammar (ADR 0108), its id in its
//! header, nodes keyed by short random ids, links keyed by the input they feed, written
//! sorted so a diff shows what changed.
//!
//! ```text
//! [mgraph version=1 id=7d2e5c0b9a4f41e3b8c6d5a4f3e2d1c0]
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
//!   inputs' own values (`inputs["strength"] = [0.7]`) and a `comment`. Fields at their
//!   defaults are not written.
//! - **A link** by the input it feeds, `<node>.<port>`, to the output it comes from.

use crate::graph::{Graph, Model, Node, PortRef};
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
}

impl GraphFile {
    /// The kind in the file's header.
    pub const KIND: &'static str = "mgraph";
    /// The format's version.
    pub const VERSION: u32 = 1;

    /// The file of `graph`.
    pub fn of(graph: &Graph) -> Self {
        let nodes = graph
            .nodes
            .iter()
            .map(|(id, n)| {
                // As many components as a constant is wide; none for the rest.
                let width = match library::kind(&n.kind).map(|k| k.body) {
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
                                library::kind(&n.kind)
                                    .and_then(|k| k.input(name))
                                    .map_or(4, |i| match i.ty {
                                        library::PortType::Float(w) => usize::from(w),
                                        library::PortType::Dynamic if same => 1,
                                        library::PortType::Dynamic => 4,
                                    });
                            (name.clone(), v[..width].to_vec())
                        })
                        .collect(),
                    comment: n.comment.clone(),
                };
                (id.clone(), node)
            })
            .collect();
        let links = graph
            .links
            .iter()
            .map(|(to, from)| (to.to_string(), from.to_string()))
            .collect();
        Self {
            model: graph.model.name().to_owned(),
            nodes,
            links,
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
        assert_eq!(lines[0], format!("[mgraph version=1 id={id}]"));
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
        let newer = text.replace("[mgraph version=1", "[mgraph version=2");
        assert!(GraphFile::parse(&newer).is_err());
        let other = text.replace("[mgraph version=1", "[material version=1");
        assert!(GraphFile::parse(&other).is_err());
        // The database knows the file, its id inside.
        let kind = engine_scene::AssetKind::of(std::path::Path::new("rocks/moss.mgraph"));
        assert_eq!(kind, Some(engine_scene::AssetKind::Graph));
        assert!(engine_scene::AssetKind::Graph.own_id());
    }
}
