//! An animation graph (`.agraph`, ADR 0124's E12-5): what plays a character's clips as its
//! parameters say. Layers of state machines whose states hold a clip, a blend of clips or
//! another machine; transitions between the states with their conditions and blends.
//!
//! The file is one value in the scenes' grammar, written as ADR 0123's graphs are: nodes,
//! states, transitions and layers keyed by short random ids (names are labels, so a rename
//! touches no reference), parameters by their names (scripts and conditions name them),
//! conditions as text, lines at their defaults left out and everything sorted, so a merge
//! sees what a change touched:
//!
//! ```text
//! [agraph version=1 id=…]
//!
//! params["speed"].kind = Float
//! params["jump"].kind = Trigger
//! layers["base"].name = "Base"
//! layers["base"].root = "m0"
//! nodes["m0"].kind = Machine
//! nodes["m0"].entry = "idle"
//! nodes["c1"].clip = "Survey"
//! nodes["b2"].kind = Blend1d
//! nodes["b2"].param = "speed"
//! nodes["b2"].points.0.clip = "Walk"
//! nodes["b2"].points.1.at = [3.5, 0]
//! nodes["b2"].points.1.clip = "Run"
//! states["idle"].machine = "m0"
//! states["idle"].name = "Idle"
//! states["idle"].pose = "c1"
//! states["move"].machine = "m0"
//! states["move"].name = "Move"
//! states["move"].pose = "b2"
//! transitions["t1"].from = "idle"
//! transitions["t1"].to = "move"
//! transitions["t1"].when = "speed > 0.1"
//! ```
//!
//! - **A parameter** has a kind (a number, a flag, a whole number or a trigger) and the value
//!   it starts with. Scripts set them; conditions read them.
//! - **A layer** plays its root node (a machine or a blend) over the layers before it, in
//!   their `order`: normal or additive, by a weight (a constant, or a parameter's), only on
//!   the joints its mask names (each with a weight for its subtree; none: all of them).
//! - **A node** is a clip (by its name: the model's, or one of the animator's own clips), a
//!   blend of clips along one parameter (`Blend1d`) or two (`Blend2d`), or a state machine
//!   (its `entry` state first).
//! - **A state** belongs to a machine and shows a node's pose.
//! - **A transition** goes to a state from another of its machine (from any of them without
//!   `from`) when its condition holds (`when`, over the parameters) and, with an exit time,
//!   once the state it leaves has played that much of its clip; it blends over `blend`
//!   seconds, by a cross-fade or by inertialization, into its destination at `offset` (or at
//!   the phase the source had, `keep_phase`). The smallest `priority` is tried first.
//! - `preview` names the model the editor shows the graph on; the game binds a graph to the
//!   character playing it, by its clips' and joints' names.

use std::collections::BTreeMap;
use std::path::Path;

use engine_reflect::Reflect;

use crate::{AssetId, AssetRef, LoadError, ValueReport};

/// What a parameter holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum ParamKind {
    /// A number (a speed, a direction).
    #[default]
    Float = 0,
    /// A flag: true or false (grounded).
    Bool = 1,
    /// A whole number (a weapon's slot).
    Int = 2,
    /// Fired by a script and used up by the transition that takes it (jump).
    Trigger = 3,
}

/// A parameter of a graph.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct ParamFile {
    /// What it holds.
    pub kind: ParamKind,
    /// The value it starts with (a flag's 0 or 1).
    pub value: f32,
}

/// How a layer goes over the layers before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum LayerBlend {
    /// In their place, by its weight.
    #[default]
    Normal = 0,
    /// Added to them, as the difference from its clips' first frames (a lean, a breath).
    Additive = 1,
}

/// A layer of a graph.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct LayerFile {
    /// Its name, as the editor shows it.
    pub name: String,
    /// Its place among the layers: the smallest plays first, under the others.
    pub order: u32,
    /// The node it plays (a machine or a blend), by id.
    pub root: String,
    /// How it goes over the layers before it.
    pub blend: LayerBlend,
    /// How much of it shows, 0 to 1.
    pub weight: f32,
    /// A parameter that sets its weight in place of `weight`, by name (empty: none).
    pub weight_param: String,
    /// The joints it plays, each with a weight for itself and its subtree (empty: all).
    pub mask: BTreeMap<String, f32>,
}

impl Default for LayerFile {
    fn default() -> Self {
        Self {
            name: String::new(),
            order: 0,
            root: String::new(),
            blend: LayerBlend::Normal,
            weight: 1.0,
            weight_param: String::new(),
            mask: BTreeMap::new(),
        }
    }
}

/// What a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum NodeKind {
    /// A clip.
    #[default]
    Clip = 0,
    /// Clips blended along a parameter.
    Blend1d = 1,
    /// Clips blended over two parameters.
    Blend2d = 2,
    /// A state machine.
    Machine = 3,
}

/// A clip of a blend, where it lies along the blend's parameters.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct BlendPoint {
    /// Where: along the parameter (`Blend1d`), or over the two (`Blend2d`).
    pub at: [f32; 2],
    /// The clip, by name.
    pub clip: String,
}

/// A node of a graph.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct NodeFile {
    /// What it is.
    pub kind: NodeKind,
    /// Where it lies on the editor's canvas.
    pub at: [f32; 2],
    /// A clip's clip, by name (the model's, or one of the animator's own clips).
    pub clip: String,
    /// How fast it plays: 1 at its own pace (a clip's, a blend's).
    pub speed: f32,
    /// The group whose clips keep in step, by name (empty: none; a blend's clips always do).
    pub sync: String,
    /// A blend's parameter, by name (along it for `Blend1d`, across for `Blend2d`).
    pub param: String,
    /// A `Blend2d`'s second parameter (up its points' second number).
    pub param_y: String,
    /// A blend's clips and where they lie.
    pub points: Vec<BlendPoint>,
    /// A machine's first state, by id.
    pub entry: String,
}

impl Default for NodeFile {
    fn default() -> Self {
        Self {
            kind: NodeKind::Clip,
            at: [0.0, 0.0],
            clip: String::new(),
            speed: 1.0,
            sync: String::new(),
            param: String::new(),
            param_y: String::new(),
            points: Vec::new(),
            entry: String::new(),
        }
    }
}

/// A state of a machine.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct StateFile {
    /// Its name: what `animator.state` reads and the editor shows.
    pub name: String,
    /// The machine it belongs to, by id.
    pub machine: String,
    /// The node whose pose it shows, by id.
    pub pose: String,
    /// Where it lies on its machine's canvas.
    pub at: [f32; 2],
}

/// A transition between two states of a machine.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct TransitionFile {
    /// The state it leaves, by id; empty: any state of its machine (the destination's).
    pub from: String,
    /// The state it goes to, by id.
    pub to: String,
    /// When it may go, over the parameters (`speed > 0.1 and grounded`); empty: always
    /// (with an exit time: at it). A trigger it names is used up when it goes.
    pub when: String,
    /// Seconds the blend into the destination takes.
    pub blend: f32,
    /// The order transitions from one state are tried in: the smallest first.
    pub priority: i32,
    /// How much of the state it leaves must have played first, as a fraction of its clip
    /// (0.9: its last tenth); 0: none.
    pub exit: f32,
    /// Where the destination starts, as a fraction of its clip.
    pub offset: f32,
    /// The destination starts at the fraction the source has played (a walk into a run in
    /// step), in place of `offset`.
    pub keep_phase: bool,
    /// Blends by carrying the source's motion into the destination as it fades
    /// (inertialization: only the destination is sampled) instead of a cross-fade.
    pub inertialize: bool,
}

impl Default for TransitionFile {
    fn default() -> Self {
        Self {
            from: String::new(),
            to: String::new(),
            when: String::new(),
            blend: 0.25,
            priority: 0,
            exit: 0.0,
            offset: 0.0,
            keep_phase: false,
            inertialize: false,
        }
    }
}

/// An animation graph: an `.agraph` file.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct AnimGraphFile {
    /// The model the editor shows it on (none: the selected character).
    pub preview: AssetRef,
    /// Its parameters, by name.
    pub params: BTreeMap<String, ParamFile>,
    /// Its layers, by id.
    pub layers: BTreeMap<String, LayerFile>,
    /// Its nodes, by id.
    pub nodes: BTreeMap<String, NodeFile>,
    /// Its machines' states, by id.
    pub states: BTreeMap<String, StateFile>,
    /// The transitions between them, by id.
    pub transitions: BTreeMap<String, TransitionFile>,
}

/// One line of each of the file's entries at its defaults (`speed = 1`), by the map it is
/// in: those lines are left out of the file.
fn default_lines() -> [(&'static str, Vec<String>); 5] {
    fn lines<T: Reflect>(value: &T) -> Vec<String> {
        let mut text = String::new();
        crate::write_value("defaults", 1, None, value, None, &mut text);
        text.lines()
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    }
    [
        ("params[", lines(&ParamFile::default())),
        ("layers[", lines(&LayerFile::default())),
        ("nodes[", lines(&NodeFile::default())),
        ("states[", lines(&StateFile::default())),
        ("transitions[", lines(&TransitionFile::default())),
    ]
}

/// Line `line`'s entry (`nodes["b2"]` of `nodes["b2"].speed = 1`) and whether its field is
/// at its default, by `defaults` ([`default_lines`]); none for a line of no entry.
fn entry_of<'l>(defaults: &[(&str, Vec<String>)], line: &'l str) -> Option<(&'l str, bool)> {
    let (map, fields) = defaults.iter().find(|(map, _)| line.starts_with(map))?;
    let (key, field) = line[map.len()..].split_once("\"].")?;
    let at_default = fields.iter().any(|d| d == field);
    Some((&line[..map.len() + key.len() + 2], at_default))
}

impl AnimGraphFile {
    /// The kind in the file's header.
    pub const KIND: &'static str = "agraph";
    /// The format's version.
    pub const VERSION: u32 = 1;
    /// The files' extension.
    pub const EXTENSION: &'static str = "agraph";

    /// The file's text with its id, its references written with their paths from `assets`;
    /// lines at their defaults left out (a reader takes them so).
    pub fn to_text(&self, id: AssetId, assets: Option<&crate::AssetPaths>) -> String {
        let mut all = String::new();
        crate::write_value(Self::KIND, Self::VERSION, Some(id), self, assets, &mut all);
        let defaults = default_lines();
        // An entry with every field at its default keeps its first line, so it is read.
        let mut kept = std::collections::HashSet::new();
        for line in all.lines() {
            if let Some((key, false)) = entry_of(&defaults, line) {
                kept.insert(key);
            }
        }
        let mut out = String::with_capacity(all.len());
        for line in all.lines() {
            let keep = match entry_of(&defaults, line) {
                Some((_, false)) => true,
                Some((key, true)) => kept.insert(key),
                None => !matches!(
                    line,
                    "preview = none"
                        | "params = {}"
                        | "layers = {}"
                        | "nodes = {}"
                        | "states = {}"
                        | "transitions = {}"
                ),
            };
            if keep {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// A graph from its text, and what reading it reported (its id, the paths its
    /// references were written with, lines it skipped).
    ///
    /// # Errors
    ///
    /// Not a graph's file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = crate::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
    }

    /// The graph in file `path`.
    ///
    /// # Errors
    ///
    /// The file cannot be read or is not a graph's (with its path).
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
            .map(|(file, _)| file)
            .map_err(|e| format!("{}:{e}", path.display()))
    }

    /// The layers in the order they play (by `order`, then id).
    pub fn layers_in_order(&self) -> Vec<(&str, &LayerFile)> {
        let mut layers: Vec<(&str, &LayerFile)> =
            self.layers.iter().map(|(id, l)| (id.as_str(), l)).collect();
        layers.sort_by_key(|(id, l)| (l.order, *id));
        layers
    }

    /// The states of machine `machine`, by id.
    pub fn states_of<'a>(
        &'a self,
        machine: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a StateFile)> {
        self.states
            .iter()
            .filter(move |(_, s)| s.machine == machine)
            .map(|(id, s)| (id.as_str(), s))
    }

    /// The machine a transition belongs to: its destination's (none when that is missing).
    pub fn machine_of(&self, transition: &TransitionFile) -> Option<&str> {
        self.states.get(&transition.to).map(|s| s.machine.as_str())
    }

    /// What does not hold together: a reference to something the file lacks (a layer's
    /// root, a machine's entry, a state's machine or pose, a transition's states, a
    /// parameter a blend or a layer names), a transition between two machines, a blend
    /// without clips; each with what it is (`transition t1`). The editor marks them; the
    /// game leaves out what they break.
    pub fn problems(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut say = |what: String, why: String| out.push((what, why));
        let param = |name: &str| name.is_empty() || self.params.contains_key(name);
        for (id, layer) in &self.layers {
            if !self.nodes.contains_key(&layer.root) {
                say(
                    format!("layer {id}"),
                    format!("no node `{}` to play", layer.root),
                );
            }
            if !param(&layer.weight_param) {
                say(
                    format!("layer {id}"),
                    format!("no parameter `{}`", layer.weight_param),
                );
            }
        }
        for (id, node) in &self.nodes {
            match node.kind {
                NodeKind::Clip if node.clip.is_empty() => {
                    say(format!("node {id}"), String::from("no clip"));
                }
                NodeKind::Blend1d | NodeKind::Blend2d => {
                    if node.points.is_empty() {
                        say(format!("node {id}"), String::from("no clips to blend"));
                    }
                    for name in [&node.param, &node.param_y] {
                        if !param(name) {
                            say(format!("node {id}"), format!("no parameter `{name}`"));
                        }
                    }
                    if node.param.is_empty()
                        || (node.kind == NodeKind::Blend2d && node.param_y.is_empty())
                    {
                        say(
                            format!("node {id}"),
                            String::from("no parameter to blend by"),
                        );
                    }
                }
                NodeKind::Machine => match self.states.get(&node.entry) {
                    Some(s) if s.machine == *id => {}
                    Some(_) => say(
                        format!("node {id}"),
                        format!("its entry `{}` is another machine's", node.entry),
                    ),
                    None => say(format!("node {id}"), String::from("no entry state")),
                },
                NodeKind::Clip => {}
            }
        }
        for (id, state) in &self.states {
            if self
                .nodes
                .get(&state.machine)
                .is_none_or(|n| n.kind != NodeKind::Machine)
            {
                say(
                    format!("state {id}"),
                    format!("no machine `{}`", state.machine),
                );
            }
            if !self.nodes.contains_key(&state.pose) {
                say(format!("state {id}"), format!("no node `{}`", state.pose));
            }
        }
        for (id, t) in &self.transitions {
            let to = self.states.get(&t.to);
            if to.is_none() {
                say(format!("transition {id}"), format!("no state `{}`", t.to));
            }
            if !t.from.is_empty() {
                match self.states.get(&t.from) {
                    None => say(format!("transition {id}"), format!("no state `{}`", t.from)),
                    Some(from) if to.is_some_and(|to| to.machine != from.machine) => {
                        say(
                            format!("transition {id}"),
                            String::from("between two machines"),
                        );
                    }
                    Some(_) => {}
                }
            }
            if t.when.trim().is_empty() && t.exit <= 0.0 {
                say(
                    format!("transition {id}"),
                    String::from("no condition and no exit time: it would go at once"),
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    /// The module's example: idle and a walk-to-run blend, a trigger, an upper-body layer.
    fn locomotion() -> AnimGraphFile {
        let mut g = AnimGraphFile::default();
        g.params.insert(String::from("speed"), ParamFile::default());
        g.params.insert(
            String::from("jump"),
            ParamFile {
                kind: ParamKind::Trigger,
                value: 0.0,
            },
        );
        g.layers.insert(
            String::from("base"),
            LayerFile {
                name: String::from("Base"),
                root: String::from("m0"),
                ..LayerFile::default()
            },
        );
        g.nodes.insert(
            String::from("m0"),
            NodeFile {
                kind: NodeKind::Machine,
                entry: String::from("idle"),
                ..NodeFile::default()
            },
        );
        g.nodes.insert(
            String::from("c1"),
            NodeFile {
                clip: String::from("Survey"),
                ..NodeFile::default()
            },
        );
        g.nodes.insert(
            String::from("b2"),
            NodeFile {
                kind: NodeKind::Blend1d,
                param: String::from("speed"),
                points: vec![
                    BlendPoint {
                        at: [0.0, 0.0],
                        clip: String::from("Walk"),
                    },
                    BlendPoint {
                        at: [3.5, 0.0],
                        clip: String::from("Run"),
                    },
                ],
                ..NodeFile::default()
            },
        );
        for (id, name, pose) in [("idle", "Idle", "c1"), ("move", "Move", "b2")] {
            g.states.insert(
                String::from(id),
                StateFile {
                    name: String::from(name),
                    machine: String::from("m0"),
                    pose: String::from(pose),
                    at: [0.0, 0.0],
                },
            );
        }
        g.transitions.insert(
            String::from("t1"),
            TransitionFile {
                from: String::from("idle"),
                to: String::from("move"),
                when: String::from("speed > 0.1"),
                ..TransitionFile::default()
            },
        );
        g.transitions.insert(
            String::from("t2"),
            TransitionFile {
                from: String::from("move"),
                to: String::from("idle"),
                when: String::from("speed <= 0.1"),
                blend: 0.4,
                inertialize: true,
                ..TransitionFile::default()
            },
        );
        g
    }

    /// The file holds what differs from the defaults, sorted, and reads back the same.
    #[test]
    fn a_graph_is_written_without_its_defaults_and_read_back() {
        let g = locomotion();
        let id = AssetId::new();
        let text = g.to_text(id, None);
        assert!(
            text.starts_with(&format!("[agraph version=1 id={id}]")),
            "{text}"
        );
        for line in [
            "params[\"jump\"].kind = Trigger",
            "layers[\"base\"].root = \"m0\"",
            "nodes[\"m0\"].kind = Machine",
            "nodes[\"b2\"].points.1.clip = \"Run\"",
            "states[\"move\"].pose = \"b2\"",
            "transitions[\"t1\"].when = \"speed > 0.1\"",
            "transitions[\"t2\"].blend = 0.4",
            "transitions[\"t2\"].inertialize = true",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
        // A parameter at its defaults keeps a line, so it is read.
        assert!(text.contains("params[\"speed\"].kind = Float"), "{text}");
        for default in [
            "params[\"speed\"].value",
            "layers[\"base\"].weight",
            "nodes[\"c1\"].kind",
            "nodes[\"c1\"].speed",
            "states[\"idle\"].at",
            "transitions[\"t1\"].blend",
            "transitions[\"t1\"].priority",
            "preview",
        ] {
            assert!(!text.contains(default), "{default} left out of {text}");
        }
        let (back, report) = AnimGraphFile::parse(&text).unwrap();
        assert_eq!(report.id, Some(id));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(back, g);
        assert_eq!(back.to_text(id, None), text, "written alike again");
    }

    /// Its parts found in order: the layers by their order, a machine's states, a
    /// transition's machine (its destination's).
    #[test]
    fn its_layers_states_and_machines_are_found() {
        let mut g = locomotion();
        g.layers.insert(
            String::from("arms"),
            LayerFile {
                name: String::from("Arms"),
                order: 1,
                root: String::from("c1"),
                ..LayerFile::default()
            },
        );
        let order: Vec<&str> = g.layers_in_order().iter().map(|(id, _)| *id).collect();
        assert_eq!(order, ["base", "arms"]);
        let states: Vec<&str> = g.states_of("m0").map(|(id, _)| id).collect();
        assert_eq!(states, ["idle", "move"]);
        assert_eq!(g.machine_of(&g.transitions["t1"]), Some("m0"));
    }

    /// A graph that holds together has no problems; each broken reference is one.
    #[test]
    fn what_does_not_hold_together_is_named() {
        let g = locomotion();
        assert!(g.problems().is_empty(), "{:?}", g.problems());
        let mut broken = g.clone();
        broken.nodes.get_mut("m0").unwrap().entry = String::from("nowhere");
        broken.transitions.get_mut("t1").unwrap().to = String::from("gone");
        broken.nodes.get_mut("b2").unwrap().param = String::from("speeed");
        broken.transitions.insert(
            String::from("t3"),
            TransitionFile {
                from: String::from("move"),
                to: String::from("idle"),
                ..TransitionFile::default()
            },
        );
        let problems = broken.problems();
        let named =
            |what: &str, why: &str| problems.iter().any(|(w, y)| w == what && y.contains(why));
        assert!(named("node m0", "no entry state"), "{problems:?}");
        assert!(named("transition t1", "no state `gone`"), "{problems:?}");
        assert!(named("node b2", "no parameter `speeed`"), "{problems:?}");
        assert!(named("transition t3", "at once"), "{problems:?}");
        assert_eq!(problems.len(), 4, "{problems:?}");
    }
}
