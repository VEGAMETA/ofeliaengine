//! A material graph in memory (ADR 0123): its shading model, its nodes by id, its links by
//! the input they feed.

use std::collections::BTreeMap;
use std::fmt;

/// The output node's id in every graph.
pub const OUTPUT: &str = "out";

/// The output's inputs every shading model reads in its surface.
pub const SURFACE_TERMS: [&str; 4] = ["base", "normal", "metallic", "roughness"];

/// A shading model (ADR 0092): the output's choice, and which of its terms it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
    /// glTF's metallic-roughness.
    #[default]
    Lit,
    /// The base color and emission, no light.
    Unlit,
    /// Lit under a clear coat.
    Clearcoat,
    /// Cloth's sheen.
    Cloth,
    /// Two shifted highlights along the strands.
    Hair,
    /// Banded diffuse, a hard highlight, a rim.
    Toon,
    /// Lit under glTF's sheen layer.
    Sheen,
    /// The sheen layer under a clear coat.
    SheenClearcoat,
}

impl Model {
    /// Every model.
    pub const ALL: [Self; 8] = [
        Self::Lit,
        Self::Unlit,
        Self::Clearcoat,
        Self::Cloth,
        Self::Hair,
        Self::Toon,
        Self::Sheen,
        Self::SheenClearcoat,
    ];

    /// Its name in files (`lit`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Lit => "lit",
            Self::Unlit => "unlit",
            Self::Clearcoat => "clearcoat",
            Self::Cloth => "cloth",
            Self::Hair => "hair",
            Self::Toon => "toon",
            Self::Sheen => "sheen",
            Self::SheenClearcoat => "sheen_clearcoat",
        }
    }

    /// The model named `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }

    /// Its constant in `forward.slang` (`MODEL_LIT`).
    pub fn constant(self) -> &'static str {
        match self {
            Self::Lit => "MODEL_LIT",
            Self::Unlit => "MODEL_UNLIT",
            Self::Clearcoat => "MODEL_CLEARCOAT",
            Self::Cloth => "MODEL_CLOTH",
            Self::Hair => "MODEL_HAIR",
            Self::Toon => "MODEL_TOON",
            Self::Sheen => "MODEL_SHEEN",
            Self::SheenClearcoat => "MODEL_SHEEN_CLEARCOAT",
        }
    }

    /// The output's inputs it reads beyond [`SURFACE_TERMS`]: `Surface`'s fields of the
    /// same names.
    pub fn terms(self) -> &'static [&'static str] {
        match self {
            Self::Lit | Self::Unlit => &[],
            Self::Clearcoat => &["clearcoat", "clearcoat_roughness"],
            Self::Cloth | Self::Sheen => &["sheen", "sheen_roughness"],
            Self::Hair => &["hair_tangent", "hair_shift"],
            Self::Toon => &[
                "toon_steps",
                "toon_softness",
                "toon_highlight",
                "rim",
                "rim_width",
            ],
            Self::SheenClearcoat => &[
                "sheen",
                "sheen_roughness",
                "clearcoat",
                "clearcoat_roughness",
            ],
        }
    }
}

/// A node.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Its kind's name ([`crate::library::kind`]).
    pub kind: String,
    /// Its place on the canvas.
    pub at: [f32; 2],
    /// A constant's components (as many as it is wide).
    pub value: [f32; 4],
    /// A parameter's name: a row of the material, its value kept by name in material files
    /// (empty: none; a texture without one is named after its node).
    pub param: String,
    /// Its inputs' own values where nothing is linked to them, in place of the kind's.
    pub inputs: BTreeMap<String, [f32; 4]>,
}

impl Node {
    /// A node of kind `kind` at `at`, its values the kind's.
    pub fn new(kind: &str, at: [f32; 2]) -> Self {
        Self {
            kind: kind.to_owned(),
            at,
            value: [0.0; 4],
            param: String::new(),
            inputs: BTreeMap::new(),
        }
    }

    /// With a constant's components.
    pub fn with_value(mut self, value: [f32; 4]) -> Self {
        self.value = value;
        self
    }

    /// As the material's parameter `name`.
    pub fn named(mut self, name: &str) -> Self {
        name.clone_into(&mut self.param);
        self
    }

    /// With input `name`'s own value.
    pub fn with_input(mut self, name: &str, value: [f32; 4]) -> Self {
        self.inputs.insert(name.to_owned(), value);
        self
    }
}

/// A node's port: `<node>.<port>` in files.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortRef {
    /// The node's id.
    pub node: String,
    /// The port's name.
    pub port: String,
}

impl PortRef {
    /// Port `port` of node `node`.
    pub fn new(node: &str, port: &str) -> Self {
        Self {
            node: node.to_owned(),
            port: port.to_owned(),
        }
    }

    /// `<node>.<port>` read back.
    pub fn parse(text: &str) -> Option<Self> {
        let (node, port) = text.split_once('.')?;
        (!node.is_empty() && !port.is_empty()).then(|| Self::new(node, port))
    }
}

impl fmt::Display for PortRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.node, self.port)
    }
}

/// A material graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Graph {
    /// The output's shading model.
    pub model: Model,
    /// Its nodes by id, the output's [`OUTPUT`].
    pub nodes: BTreeMap<String, Node>,
    /// Its links by the input they feed: `links[input]` is the output it comes from.
    pub links: BTreeMap<PortRef, PortRef>,
}

impl Graph {
    /// A graph of `model` holding its output node alone.
    pub fn new(model: Model) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            OUTPUT.to_owned(),
            Node::new(crate::library::OUTPUT_KIND, [0.0; 2]),
        );
        Self {
            model,
            nodes,
            links: BTreeMap::new(),
        }
    }

    /// `node` added as `id` (replacing one of that id).
    pub fn add(&mut self, id: &str, node: Node) -> &mut Self {
        self.nodes.insert(id.to_owned(), node);
        self
    }

    /// Output `from` linked into input `to` (replacing `to`'s link).
    pub fn link(&mut self, from: PortRef, to: PortRef) -> &mut Self {
        self.links.insert(to, from);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_and_ports_read_back_what_they_write() {
        for m in Model::ALL {
            assert_eq!(Model::from_name(m.name()), Some(m));
        }
        assert_eq!(Model::from_name("plastic"), None);
        let p = PortRef::new("a3f9", "rgb");
        assert_eq!(p.to_string(), "a3f9.rgb");
        assert_eq!(PortRef::parse("a3f9.rgb"), Some(p));
        assert_eq!(PortRef::parse("a3f9"), None);
        assert_eq!(PortRef::parse(".rgb"), None);
        let g = Graph::new(Model::Toon);
        assert_eq!(g.nodes[OUTPUT].kind, crate::library::OUTPUT_KIND);
    }
}
