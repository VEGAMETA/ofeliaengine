//! Animation graphs at run time (ADR 0124's E12-5b): a graph cooked once per character's
//! rig into flat arrays ([`AnimGraph`]), and each character's own state ([`GraphState`]),
//! moved on by the ticks ([`AnimGraph::tick`]) and posed each frame between two ticks
//! ([`AnimGraph::pose`]).
//!
//! - **Parameters:** up to [`MAX_PARAMS`] numbers by slot (flags as 0 or 1, triggers as the
//!   number of times fired, used up by the transition that takes them).
//! - **Layers** play their root node over the layers before them, in order: the first in
//!   place of the rest pose, the next ones in place of what is below on the joints their
//!   mask weighs (by their weight), additive ones added as the difference of their clips
//!   from their first frames.
//! - **Nodes:** a clip; a blend of clips along one parameter (the two around its value) or
//!   over two (the three corners of the Delaunay triangle holding the point, or the two ends
//!   of the nearest edge outside them), its clips in step at one phase; a state machine.
//! - **A tick** takes each machine's transitions (from any state first, then its state's,
//!   the smallest priority first; up to four chained), weighs every clip it reaches
//!   (machines' fades, blends' weights, layers'), and moves the weighed clips and blends'
//!   phases on. A transition goes when its condition holds and its exit time has passed;
//!   its destination starts at its offset or at the fraction the source had played.
//! - **A frame** samples the clips each layer weighs (their phases and weights between the
//!   last two ticks), blends them with one [`Blender::blend`], and stacks the layers.
//!   Nothing is allocated, looked up by name or dispatched dynamically on the way.
//! - A transition taken while its machine still fades drops the state fading out (an
//!   inertialized hand-over comes later).

use crate::condition::{Condition, ParamSlot};
use crate::pose::same_hemisphere;
use crate::{Blender, CompressedClip, Layer, LayerMode, Pose, Skeleton};
use engine_core::math::{Quat, Vec3A, Vec4};

/// The most parameters a graph has.
pub const MAX_PARAMS: usize = 16;
/// The most clips one layer blends at once (the lightest ones past it are left out).
pub const MAX_LEAVES: usize = 16;
/// Transitions taken one after another in a tick at most.
const CHAIN: usize = 4;
/// Machines inside machines at most.
const DEPTH: u32 = 8;
const NONE: u16 = u16::MAX;
/// Weights below it play nothing.
const EPSILON: f32 = 1e-4;

/// What a parameter holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphParamKind {
    /// A number.
    #[default]
    Float,
    /// A flag: 0 or 1.
    Bool,
    /// A whole number.
    Int,
    /// Fired, and used up by the transition that takes it.
    Trigger,
}

/// A graph as written ([`AnimGraph::cook`] makes it playable): its parts by index.
#[derive(Debug, Clone, Default)]
pub struct GraphDesc {
    /// Its parameters.
    pub params: Vec<ParamDesc>,
    /// Its layers, in the order they play.
    pub layers: Vec<LayerDesc>,
    /// Its nodes.
    pub nodes: Vec<NodeDesc>,
    /// Its machines' states.
    pub states: Vec<StateDesc>,
    /// The transitions between them.
    pub transitions: Vec<TransitionDesc>,
}

/// A parameter.
#[derive(Debug, Clone, Default)]
pub struct ParamDesc {
    /// Its name.
    pub name: String,
    /// What it holds.
    pub kind: GraphParamKind,
    /// The value it starts with.
    pub value: f32,
}

/// A layer.
#[derive(Debug, Clone, Default)]
pub struct LayerDesc {
    /// Its name.
    pub name: String,
    /// The node it plays, by index.
    pub root: usize,
    /// Added to the layers below as its clips' difference from their first frames.
    pub additive: bool,
    /// How much of it shows, 0 to 1.
    pub weight: f32,
    /// A parameter setting its weight in place of `weight`.
    pub weight_param: Option<String>,
    /// The joints it plays by name, each with a weight for its subtree (deeper ones over
    /// their ancestors'); empty: all of them.
    pub mask: Vec<(String, f32)>,
}

/// A node.
#[derive(Debug, Clone)]
pub enum NodeDesc {
    /// A clip, by name, played at `speed`.
    Clip {
        /// The clip.
        clip: String,
        /// How fast: 1 at its own pace.
        speed: f32,
    },
    /// Clips along parameter `param`, each at its place.
    Blend1d {
        /// The parameter.
        param: String,
        /// Each clip's place along it, and the clip.
        points: Vec<(f32, String)>,
        /// How fast: 1 at the clips' own pace.
        speed: f32,
    },
    /// Clips over two parameters, each at its point.
    Blend2d {
        /// The parameters: across and up.
        params: [String; 2],
        /// Each clip's point, and the clip.
        points: Vec<([f32; 2], String)>,
        /// How fast: 1 at the clips' own pace.
        speed: f32,
    },
    /// A state machine starting in state `entry` (an index into the states).
    Machine {
        /// Its first state.
        entry: usize,
    },
}

/// A state.
#[derive(Debug, Clone, Default)]
pub struct StateDesc {
    /// Its name.
    pub name: String,
    /// The machine node it belongs to, by index.
    pub machine: usize,
    /// The node whose pose it shows, by index.
    pub pose: usize,
}

/// A transition.
#[derive(Debug, Clone, Default)]
pub struct TransitionDesc {
    /// The state it leaves (none: any state of its destination's machine).
    pub from: Option<usize>,
    /// The state it goes to.
    pub to: usize,
    /// Its condition's text (empty: always).
    pub when: String,
    /// Seconds its cross-fade takes (0: at once).
    pub blend: f32,
    /// Tried in this order among its source's: the smallest first.
    pub priority: i32,
    /// How much of the source must have played first, as a fraction (0: none).
    pub exit: f32,
    /// Where the destination starts, as a fraction.
    pub offset: f32,
    /// The destination starts at the fraction the source played, in place of `offset`.
    pub keep_phase: bool,
}

/// A clip a graph names, as the character playing it has it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipInfo {
    /// Its index among the character's clips (what [`AnimGraph::pose`] asks for).
    pub index: u16,
    /// Seconds long.
    pub length: f32,
    /// It plays once and holds its end (else it loops).
    pub once: bool,
}

/// What cooking a graph left out, and why (a transition whose condition does not compile, a
/// clip the character lacks, a mask's joint it lacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphProblem {
    /// What: `transition 3`, `node 2`, `layer 0`.
    pub what: String,
    /// Why.
    pub why: String,
}

#[derive(Debug, Clone)]
struct LayerDef {
    root: u16,
    additive: bool,
    weight: f32,
    weight_param: u16,
    /// Per joint (none: all at 1).
    mask: Option<Box<[f32]>>,
}

#[derive(Debug, Clone, Copy)]
enum NodeDef {
    /// Left out by the cook: plays nothing.
    Missing,
    Clip {
        leaf: u16,
        timed: u16,
        speed: f32,
    },
    Blend1d {
        param: u16,
        /// Its leaves, sorted along the parameter.
        leaves: (u16, u16),
        timed: u16,
        speed: f32,
    },
    Blend2d {
        params: [u16; 2],
        leaves: (u16, u16),
        /// Its Delaunay triangles, by leaf.
        triangles: (u16, u16),
        timed: u16,
        speed: f32,
    },
    Machine {
        machine: u16,
    },
}

#[derive(Debug, Clone, Copy)]
struct MachineDef {
    entry: u16,
    /// Its transitions from any state.
    any: (u16, u16),
}

#[derive(Debug, Clone)]
struct StateDef {
    name: Box<str>,
    machine: u16,
    pose: u16,
    transitions: (u16, u16),
}

#[derive(Debug, Clone)]
struct TransitionDef {
    to: u16,
    condition: Condition,
    blend: f32,
    exit: f32,
    offset: f32,
    keep_phase: bool,
}

#[derive(Debug, Clone, Copy)]
struct LeafDef {
    clip: u16,
    length: f32,
    once: bool,
    /// Its place in its blend.
    at: [f32; 2],
    /// The clip or blend whose phase it plays at.
    timed: u16,
}

/// A graph cooked for one rig: shared by every character of it that plays the graph.
#[derive(Debug, Clone, Default)]
pub struct AnimGraph {
    params: Box<[(Box<str>, GraphParamKind, f32)]>,
    layers: Box<[LayerDef]>,
    nodes: Box<[NodeDef]>,
    machines: Box<[MachineDef]>,
    states: Box<[StateDef]>,
    transitions: Box<[TransitionDef]>,
    leaves: Box<[LeafDef]>,
    triangles: Box<[[u16; 3]]>,
    /// Whether each timed node (a clip or a blend) plays once (all its clips do).
    timed_once: Box<[bool]>,
    /// Each leaf's first frame where an additive layer plays it ([`AnimGraph::prepare`]):
    /// what its difference is taken from, sampled once instead of every frame.
    references: Box<[Option<Pose>]>,
    joints: usize,
}

/// A machine's state at run time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MachineState {
    state: u16,
    /// The state fading out (`NONE`: none), and how far into its fade.
    from: u16,
    faded: f32,
    fade: f32,
}

/// One character's state of a graph: its parameters, its machines' states, its clips' phases
/// and weights now and at the tick before. Made once ([`GraphState::new`]); its ticks and
/// frames allocate nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphState {
    params: [f32; MAX_PARAMS],
    machines: Box<[MachineState]>,
    /// Each clip's and blend's phase, a fraction of its length (past 1 on later loops).
    phase: Box<[f32]>,
    phase_before: Box<[f32]>,
    /// Each timed node's weight this tick (over all layers).
    node_weight: Box<[f32]>,
    /// Each layer's leaves' weights, layer by layer.
    leaf_weight: Box<[f32]>,
    leaf_weight_before: Box<[f32]>,
    /// Ticks since it was made: the first one has no tick before to blend from.
    ticked: bool,
}

/// What [`AnimGraph::pose`] works in: poses for a layer's clips, sized for a rig. Made once
/// per rig size; posing allocates nothing.
#[derive(Debug, Clone)]
pub struct GraphWork {
    poses: Vec<Pose>,
    layer: Pose,
    reference: Pose,
    delta: Pose,
    blender: Blender,
}

impl GraphWork {
    /// Room for poses of `joints` joints.
    pub fn new(joints: usize) -> Self {
        Self {
            poses: (0..MAX_LEAVES).map(|_| Pose::identity(joints)).collect(),
            layer: Pose::identity(joints),
            reference: Pose::identity(joints),
            delta: Pose::identity(joints),
            blender: Blender::new(joints),
        }
    }

    /// The joints it has room for.
    pub fn joints(&self) -> usize {
        self.layer.len()
    }
}

impl AnimGraph {
    /// Graph `desc` cooked for a character: `clip` finds a clip it names among the
    /// character's, `skeleton` its masks' joints. What cannot be cooked is left out and
    /// named (a transition whose condition does not compile, a clip or a joint the character
    /// lacks, more than [`MAX_PARAMS`] parameters, a reference out of range).
    pub fn cook(
        desc: &GraphDesc,
        clip: impl Fn(&str) -> Option<ClipInfo>,
        skeleton: &Skeleton,
    ) -> (Self, Vec<GraphProblem>) {
        let mut problems = Vec::new();
        let mut say = |what: String, why: String| problems.push(GraphProblem { what, why });
        // Parameters, by slot.
        let mut params: Vec<(Box<str>, GraphParamKind, f32)> = Vec::new();
        for p in &desc.params {
            if params.len() == MAX_PARAMS {
                say(
                    format!("parameter {}", p.name),
                    format!("a graph has at most {MAX_PARAMS} parameters"),
                );
                continue;
            }
            params.push((p.name.as_str().into(), p.kind, p.value));
        }
        let slot_of = |name: &str| -> Option<ParamSlot> {
            let k = params.iter().position(|(n, ..)| &**n == name)?;
            Some((k as u16, params[k].1 == GraphParamKind::Trigger))
        };
        // Machines first: states and transitions name them by node.
        let mut machine_of_node = vec![NONE; desc.nodes.len()];
        let mut machines = Vec::new();
        for (n, node) in desc.nodes.iter().enumerate() {
            if let NodeDesc::Machine { .. } = node {
                machine_of_node[n] = machines.len() as u16;
                machines.push(MachineDef {
                    entry: NONE,
                    any: (0, 0),
                });
            }
        }
        // Nodes: their clips as leaves, the clips and blends timed.
        let mut nodes = Vec::with_capacity(desc.nodes.len());
        let mut leaves: Vec<LeafDef> = Vec::new();
        let mut triangles: Vec<[u16; 3]> = Vec::new();
        let mut timed_once = Vec::new();
        for (n, node) in desc.nodes.iter().enumerate() {
            let def = match node {
                NodeDesc::Clip { clip: name, speed } => match clip(name) {
                    Some(info) => {
                        let timed = timed_once.len() as u16;
                        timed_once.push(info.once);
                        leaves.push(LeafDef {
                            clip: info.index,
                            length: info.length.max(1e-6),
                            once: info.once,
                            at: [0.0, 0.0],
                            timed,
                        });
                        NodeDef::Clip {
                            leaf: (leaves.len() - 1) as u16,
                            timed,
                            speed: *speed,
                        }
                    }
                    None => {
                        say(format!("node {n}"), format!("no clip `{name}`"));
                        NodeDef::Missing
                    }
                },
                NodeDesc::Blend1d {
                    param,
                    points,
                    speed,
                } => {
                    let Some((slot, _)) = slot_of(param) else {
                        say(format!("node {n}"), format!("no parameter `{param}`"));
                        nodes.push(NodeDef::Missing);
                        continue;
                    };
                    let mut found: Vec<(f32, ClipInfo)> = Vec::new();
                    for (x, name) in points {
                        match clip(name) {
                            Some(info) => found.push((*x, info)),
                            None => say(format!("node {n}"), format!("no clip `{name}`")),
                        }
                    }
                    if found.is_empty() {
                        nodes.push(NodeDef::Missing);
                        continue;
                    }
                    found.sort_by(|a, b| a.0.total_cmp(&b.0));
                    let timed = timed_once.len() as u16;
                    timed_once.push(found.iter().all(|(_, i)| i.once));
                    let first = leaves.len() as u16;
                    leaves.extend(found.iter().map(|(x, info)| LeafDef {
                        clip: info.index,
                        length: info.length.max(1e-6),
                        once: info.once,
                        at: [*x, 0.0],
                        timed,
                    }));
                    NodeDef::Blend1d {
                        param: slot,
                        leaves: (first, leaves.len() as u16),
                        timed,
                        speed: *speed,
                    }
                }
                NodeDesc::Blend2d {
                    params: names,
                    points,
                    speed,
                } => {
                    let (Some((x, _)), Some((y, _))) = (slot_of(&names[0]), slot_of(&names[1]))
                    else {
                        say(
                            format!("node {n}"),
                            format!("no parameters `{}` and `{}`", names[0], names[1]),
                        );
                        nodes.push(NodeDef::Missing);
                        continue;
                    };
                    let mut found: Vec<([f32; 2], ClipInfo)> = Vec::new();
                    for (at, name) in points {
                        match clip(name) {
                            Some(info) => found.push((*at, info)),
                            None => say(format!("node {n}"), format!("no clip `{name}`")),
                        }
                    }
                    if found.is_empty() {
                        nodes.push(NodeDef::Missing);
                        continue;
                    }
                    let timed = timed_once.len() as u16;
                    timed_once.push(found.iter().all(|(_, i)| i.once));
                    let first = leaves.len() as u16;
                    leaves.extend(found.iter().map(|(at, info)| LeafDef {
                        clip: info.index,
                        length: info.length.max(1e-6),
                        once: info.once,
                        at: *at,
                        timed,
                    }));
                    let at: Vec<[f32; 2]> = found.iter().map(|(at, _)| *at).collect();
                    let tri_first = triangles.len() as u16;
                    triangles.extend(
                        delaunay(&at)
                            .into_iter()
                            .map(|t| t.map(|k| first + k as u16)),
                    );
                    NodeDef::Blend2d {
                        params: [x, y],
                        leaves: (first, leaves.len() as u16),
                        triangles: (tri_first, triangles.len() as u16),
                        timed,
                        speed: *speed,
                    }
                }
                NodeDesc::Machine { .. } => NodeDef::Machine {
                    machine: machine_of_node[n],
                },
            };
            nodes.push(def);
        }
        // States, of their machines; the machines' entries.
        let mut states = Vec::with_capacity(desc.states.len());
        for (s, state) in desc.states.iter().enumerate() {
            let machine = machine_of_node.get(state.machine).copied().unwrap_or(NONE);
            if machine == NONE {
                say(format!("state {s}"), String::from("its machine is not one"));
            }
            if state.pose >= nodes.len() {
                say(format!("state {s}"), String::from("no node to show"));
            }
            states.push(StateDef {
                name: state.name.as_str().into(),
                machine,
                pose: if state.pose < nodes.len() {
                    state.pose as u16
                } else {
                    NONE
                },
                transitions: (0, 0),
            });
        }
        for (n, node) in desc.nodes.iter().enumerate() {
            if let NodeDesc::Machine { entry } = node {
                let m = machine_of_node[n];
                match states.get(*entry) {
                    Some(s) if s.machine == m => machines[m as usize].entry = *entry as u16,
                    _ => say(
                        format!("node {n}"),
                        String::from("no entry state of its own"),
                    ),
                }
            }
        }
        // Transitions: compiled, grouped by source (a machine's from any state first), each
        // group by priority.
        let mut compiled: Vec<(u16, bool, i32, usize, TransitionDef)> = Vec::new();
        for (t, desc_t) in desc.transitions.iter().enumerate() {
            let Some(to) = states.get(desc_t.to) else {
                say(format!("transition {t}"), String::from("no state to go to"));
                continue;
            };
            let machine = to.machine;
            if machine == NONE {
                continue;
            }
            let (source, any) = match desc_t.from {
                None => (machine, true),
                Some(from) => match states.get(from) {
                    Some(s) if s.machine == machine => (from as u16, false),
                    Some(_) => {
                        say(
                            format!("transition {t}"),
                            String::from("between two machines"),
                        );
                        continue;
                    }
                    None => {
                        say(format!("transition {t}"), String::from("no state to leave"));
                        continue;
                    }
                },
            };
            let condition = match Condition::compile(&desc_t.when, slot_of) {
                Ok(c) => c,
                Err(e) => {
                    say(format!("transition {t}"), e.to_string());
                    continue;
                }
            };
            compiled.push((
                source,
                any,
                desc_t.priority,
                t,
                TransitionDef {
                    to: desc_t.to as u16,
                    condition,
                    blend: desc_t.blend.max(0.0),
                    exit: desc_t.exit.max(0.0),
                    offset: desc_t.offset,
                    keep_phase: desc_t.keep_phase,
                },
            ));
        }
        // Any first (by machine), then by state; by priority, then as written.
        compiled.sort_by_key(|(source, any, priority, t, _)| (!*any, *source, *priority, *t));
        let mut transitions = Vec::with_capacity(compiled.len());
        for (source, any, _, _, def) in compiled {
            let k = transitions.len() as u16;
            let range = if any {
                &mut machines[source as usize].any
            } else {
                &mut states[source as usize].transitions
            };
            if range.0 == range.1 {
                *range = (k, k);
            }
            range.1 = k + 1;
            transitions.push(def);
        }
        // Layers, their masks per joint (deeper joints over their ancestors).
        let joints = skeleton.joint_count();
        let mut layers = Vec::with_capacity(desc.layers.len());
        for (l, layer) in desc.layers.iter().enumerate() {
            if layer.root >= nodes.len() {
                say(format!("layer {l}"), String::from("no node to play"));
                continue;
            }
            let weight_param = match &layer.weight_param {
                Some(name) => match slot_of(name) {
                    Some((slot, _)) => slot,
                    None => {
                        say(format!("layer {l}"), format!("no parameter `{name}`"));
                        NONE
                    }
                },
                None => NONE,
            };
            let mask = (!layer.mask.is_empty()).then(|| {
                let mut found: Vec<(usize, f32)> = Vec::new();
                for (name, w) in &layer.mask {
                    match skeleton.find(name) {
                        Some(j) => found.push((j, *w)),
                        None => say(format!("layer {l}"), format!("no joint `{name}`")),
                    }
                }
                found.sort_by_key(|(j, _)| *j);
                let mut weights = vec![0.0f32; joints];
                for (j, w) in found {
                    for v in &mut weights[j..skeleton.subtree_end(j).min(joints)] {
                        *v = w.clamp(0.0, 1.0);
                    }
                }
                weights.into_boxed_slice()
            });
            layers.push(LayerDef {
                root: layer.root as u16,
                additive: layer.additive,
                weight: layer.weight.clamp(0.0, 1.0),
                weight_param,
                mask,
            });
        }
        let graph = Self {
            params: params.into_boxed_slice(),
            layers: layers.into_boxed_slice(),
            nodes: nodes.into_boxed_slice(),
            machines: machines.into_boxed_slice(),
            states: states.into_boxed_slice(),
            transitions: transitions.into_boxed_slice(),
            leaves: leaves.into_boxed_slice(),
            triangles: triangles.into_boxed_slice(),
            timed_once: timed_once.into_boxed_slice(),
            references: Box::default(),
            joints,
        };
        (graph, problems)
    }

    /// The slot of parameter `name`.
    pub fn param(&self, name: &str) -> Option<u16> {
        self.params
            .iter()
            .position(|(n, ..)| &**n == name)
            .map(|k| k as u16)
    }

    /// Its parameters: each one's name, kind and starting value, by slot.
    pub fn params(&self) -> impl Iterator<Item = (&str, GraphParamKind, f32)> {
        self.params.iter().map(|(n, k, v)| (&**n, *k, *v))
    }

    /// How many layers play.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// The joints of the rig it was cooked for.
    pub fn joints(&self) -> usize {
        self.joints
    }

    /// The state layer `layer`'s machine is in now (its root's, when that is a machine): its
    /// name.
    pub fn state_name<'a>(&'a self, s: &GraphState, layer: usize) -> Option<&'a str> {
        let root = self.layers.get(layer)?.root;
        let NodeDef::Machine { machine } = self.nodes.get(usize::from(root))? else {
            return None;
        };
        let state = s.machines.get(usize::from(*machine))?.state;
        self.states.get(usize::from(state)).map(|st| &*st.name)
    }

    /// One tick of `dt` seconds of character state `s`: its machines' transitions taken, its
    /// clips weighed and their phases moved on. Allocates nothing.
    pub fn tick(&self, s: &mut GraphState, dt: f32) {
        s.phase_before.copy_from_slice(&s.phase);
        s.leaf_weight_before.copy_from_slice(&s.leaf_weight);
        let dt = dt.max(0.0);
        // Each machine's transitions (taken at the tick's start), then its fade on by the
        // tick: a fade begun now is `dt` in at its end.
        for m in 0..self.machines.len() {
            for _ in 0..CHAIN {
                let Some(t) = self.choose(m, s) else {
                    break;
                };
                self.take(m, t, s);
            }
            let ms = &mut s.machines[m];
            if ms.from != NONE {
                ms.faded += dt;
                if ms.faded >= ms.fade {
                    ms.from = NONE;
                }
            }
        }
        // What every clip weighs.
        s.node_weight.fill(0.0);
        s.leaf_weight.fill(0.0);
        let leaves = self.leaves.len();
        for (l, layer) in self.layers.iter().enumerate() {
            let range = l * leaves..(l + 1) * leaves;
            self.weigh(
                layer.root,
                1.0,
                &s.params,
                &s.machines,
                &mut s.leaf_weight[range],
                &mut s.node_weight,
                0,
            );
        }
        // The weighed clips and blends move on: a blend's at the pace of its clips' lengths
        // weighed together, so its clips keep in step.
        for node in self.nodes.iter() {
            let (timed, speed, length) = match *node {
                NodeDef::Clip { leaf, timed, speed } => {
                    (timed, speed, self.leaves[usize::from(leaf)].length)
                }
                NodeDef::Blend1d {
                    leaves: (a, b),
                    timed,
                    speed,
                    ..
                }
                | NodeDef::Blend2d {
                    leaves: (a, b),
                    timed,
                    speed,
                    ..
                } => {
                    let (mut sum, mut weighed) = (0.0f32, 0.0f32);
                    for k in a..b {
                        let mut w = 0.0;
                        for l in 0..self.layers.len() {
                            w += s.leaf_weight[l * leaves + usize::from(k)];
                        }
                        sum += w;
                        weighed += w * self.leaves[usize::from(k)].length;
                    }
                    let length = if sum > EPSILON {
                        weighed / sum
                    } else {
                        self.leaves[usize::from(a)].length
                    };
                    (timed, speed, length)
                }
                NodeDef::Machine { .. } | NodeDef::Missing => continue,
            };
            let t = usize::from(timed);
            if s.node_weight[t] <= EPSILON {
                continue;
            }
            let mut phase = s.phase[t] + dt * speed / length.max(1e-6);
            if self.timed_once[t] {
                phase = phase.clamp(0.0, 1.0);
            } else if phase > 1024.0 {
                // Whole loops dropped from both, so the step between them stays.
                let whole = phase.floor();
                phase -= whole;
                s.phase_before[t] -= whole;
            }
            s.phase[t] = phase;
        }
        if !s.ticked {
            // The first tick: nothing before it to blend from.
            s.phase_before.copy_from_slice(&s.phase);
            s.leaf_weight_before.copy_from_slice(&s.leaf_weight);
            s.ticked = true;
        }
    }

    /// The transition machine `m` takes now, if any: from any state first, then its state's,
    /// each in priority order; one whose exit time has passed and whose condition holds.
    fn choose(&self, m: usize, s: &GraphState) -> Option<u16> {
        let ms = s.machines[m];
        let state = self.states.get(usize::from(ms.state))?;
        let any = self.machines[m].any;
        let own = state.transitions;
        (any.0..any.1).chain(own.0..own.1).find(|&t| {
            let def = &self.transitions[usize::from(t)];
            // From any state, not into the one it is in.
            if t < any.1 && t >= any.0 && def.to == ms.state {
                return false;
            }
            self.exit_passed(state, def.exit, s) && def.condition.holds(&s.params)
        })
    }

    /// Whether state `state` has played fraction `exit` of its pose (in its loop, for one
    /// that loops); always without an exit time or for a machine's pose.
    fn exit_passed(&self, state: &StateDef, exit: f32, s: &GraphState) -> bool {
        if exit <= 0.0 {
            return true;
        }
        let Some(timed) = self.timed_of(state.pose) else {
            return true;
        };
        let t = usize::from(timed);
        let (now, before) = (s.phase[t], s.phase_before[t]);
        if self.timed_once[t] {
            return now >= exit.min(1.0);
        }
        let exit = exit.fract();
        now - now.floor() >= exit || (before - exit).floor() < (now - exit).floor()
    }

    /// The clip or blend a node plays at its own phase.
    fn timed_of(&self, node: u16) -> Option<u16> {
        match self.nodes.get(usize::from(node))? {
            NodeDef::Clip { timed, .. }
            | NodeDef::Blend1d { timed, .. }
            | NodeDef::Blend2d { timed, .. } => Some(*timed),
            NodeDef::Machine { .. } | NodeDef::Missing => None,
        }
    }

    /// Transition `t` taken by machine `m`: its triggers used up, its destination entered at
    /// its offset or the source's fraction, the source fading out over its blend.
    fn take(&self, m: usize, t: u16, s: &mut GraphState) {
        let def = &self.transitions[usize::from(t)];
        for &slot in def.condition.triggers() {
            if let Some(p) = s.params.get_mut(usize::from(slot)) {
                *p = 0.0;
            }
        }
        let source = s.machines[m].state;
        let source_phase = self
            .states
            .get(usize::from(source))
            .and_then(|st| self.timed_of(st.pose))
            .map_or(0.0, |timed| s.phase[usize::from(timed)]);
        let ms = &mut s.machines[m];
        if def.blend > 0.0 {
            ms.from = source;
            ms.faded = 0.0;
            ms.fade = def.blend;
        } else {
            ms.from = NONE;
        }
        ms.state = def.to;
        let start = if def.keep_phase {
            source_phase - source_phase.floor()
        } else {
            def.offset
        };
        self.enter(def.to, start, s, 0);
    }

    /// State `state` entered: its pose from `start` (a fraction), a machine it shows at its
    /// entry from the start.
    fn enter(&self, state: u16, start: f32, s: &mut GraphState, depth: u32) {
        let Some(pose) = self.states.get(usize::from(state)).map(|st| st.pose) else {
            return;
        };
        if let Some(timed) = self.timed_of(pose) {
            let t = usize::from(timed);
            s.phase[t] = start;
            s.phase_before[t] = start;
        } else if let Some(NodeDef::Machine { machine }) = self.nodes.get(usize::from(pose))
            && depth < DEPTH
        {
            let m = usize::from(*machine);
            let entry = self.machines[m].entry;
            s.machines[m] = MachineState {
                state: entry,
                from: NONE,
                faded: 0.0,
                fade: 0.0,
            };
            self.enter(entry, 0.0, s, depth + 1);
        }
    }

    /// Node `node` weighed `weight`: its clips' weights into `leaf_weight` (the layer's),
    /// its timed nodes' into `node_weight`.
    #[allow(clippy::too_many_arguments)]
    fn weigh(
        &self,
        node: u16,
        weight: f32,
        params: &[f32; MAX_PARAMS],
        machines: &[MachineState],
        leaf_weight: &mut [f32],
        node_weight: &mut [f32],
        depth: u32,
    ) {
        if weight <= EPSILON || depth > DEPTH {
            return;
        }
        let Some(def) = self.nodes.get(usize::from(node)) else {
            return;
        };
        let value = |slot: u16| params.get(usize::from(slot)).copied().unwrap_or(0.0);
        match *def {
            NodeDef::Missing => {}
            NodeDef::Clip { leaf, timed, .. } => {
                leaf_weight[usize::from(leaf)] += weight;
                node_weight[usize::from(timed)] += weight;
            }
            NodeDef::Blend1d {
                param,
                leaves: (a, b),
                timed,
                ..
            } => {
                node_weight[usize::from(timed)] += weight;
                let v = value(param);
                let xs = |k: u16| self.leaves[usize::from(k)].at[0];
                if v <= xs(a) || b - a == 1 {
                    leaf_weight[usize::from(a)] += weight;
                } else if v >= xs(b - 1) {
                    leaf_weight[usize::from(b - 1)] += weight;
                } else {
                    let k = (a..b - 1).find(|&k| v < xs(k + 1)).unwrap_or(b - 2);
                    let span = (xs(k + 1) - xs(k)).max(1e-6);
                    let t = ((v - xs(k)) / span).clamp(0.0, 1.0);
                    leaf_weight[usize::from(k)] += weight * (1.0 - t);
                    leaf_weight[usize::from(k + 1)] += weight * t;
                }
            }
            NodeDef::Blend2d {
                params: [x, y],
                leaves: (a, b),
                triangles: (ta, tb),
                timed,
                ..
            } => {
                node_weight[usize::from(timed)] += weight;
                let p = [value(x), value(y)];
                let at = |k: u16| self.leaves[usize::from(k)].at;
                let mut give = |k: u16, w: f32| leaf_weight[usize::from(k)] += weight * w;
                // Inside a triangle: its corners by barycentric weights.
                for tri in &self.triangles[usize::from(ta)..usize::from(tb)] {
                    if let Some(bary) = barycentric(p, at(tri[0]), at(tri[1]), at(tri[2])) {
                        for (k, w) in tri.iter().zip(bary) {
                            give(*k, w);
                        }
                        return;
                    }
                }
                // Outside them (or too few points for one): the nearest point of the
                // nearest segment between two clips.
                let mut best = (f32::MAX, a, a, 0.0f32);
                for i in a..b {
                    for j in i..b {
                        let (t, d) = nearest_on_segment(p, at(i), at(j));
                        if d < best.0 {
                            best = (d, i, j, t);
                        }
                    }
                }
                let (_, i, j, t) = best;
                if i == j {
                    give(i, 1.0);
                } else {
                    give(i, 1.0 - t);
                    give(j, t);
                }
            }
            NodeDef::Machine { machine } => {
                let Some(ms) = machines.get(usize::from(machine)) else {
                    return;
                };
                let fade = if ms.from == NONE || ms.fade <= 0.0 {
                    1.0
                } else {
                    (ms.faded / ms.fade).clamp(0.0, 1.0)
                };
                let pose = |state: u16| self.states.get(usize::from(state)).map(|st| st.pose);
                if let Some(p) = pose(ms.state) {
                    self.weigh(
                        p,
                        weight * fade,
                        params,
                        machines,
                        leaf_weight,
                        node_weight,
                        depth + 1,
                    );
                }
                if ms.from != NONE
                    && let Some(p) = pose(ms.from)
                {
                    self.weigh(
                        p,
                        weight * (1.0 - fade),
                        params,
                        machines,
                        leaf_weight,
                        node_weight,
                        depth + 1,
                    );
                }
            }
        }
    }

    /// Character state `s`'s pose a frame `alpha` of the way from its last tick to its next,
    /// into `out` (local transforms): each layer's clips sampled (by `clip`, the character's
    /// clip of an index) at their phases and weights between the two ticks, blended, and the
    /// layers stacked over the rest pose `rest`. Allocates nothing; false when nothing plays
    /// (`out` is then the rest pose).
    pub fn pose<'c>(
        &self,
        s: &GraphState,
        alpha: f32,
        clip: impl Fn(u16) -> Option<&'c CompressedClip>,
        rest: &Pose,
        work: &mut GraphWork,
        out: &mut Pose,
    ) -> bool {
        out.copy_from(rest);
        let alpha = alpha.clamp(0.0, 1.0);
        let leaves = self.leaves.len();
        let mut posed = false;
        for (l, layer) in self.layers.iter().enumerate() {
            let weight = if layer.weight_param == NONE {
                layer.weight
            } else {
                s.params
                    .get(usize::from(layer.weight_param))
                    .copied()
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0)
            };
            if weight <= EPSILON {
                continue;
            }
            // The layer's clips this frame, the heaviest when more than room.
            let mut chosen = [(0u16, 0.0f32); MAX_LEAVES];
            let mut count = 0usize;
            for k in 0..leaves {
                let w0 = s.leaf_weight_before[l * leaves + k];
                let w1 = s.leaf_weight[l * leaves + k];
                let w = w0 + (w1 - w0) * alpha;
                if w <= EPSILON {
                    continue;
                }
                if count < MAX_LEAVES {
                    chosen[count] = (k as u16, w);
                    count += 1;
                } else if let Some(lightest) = chosen
                    .iter_mut()
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .filter(|c| c.1 < w)
                {
                    *lightest = (k as u16, w);
                }
            }
            if count == 0 {
                continue;
            }
            let sampled = self.sample_leaves(s, &chosen[..count], alpha, false, &clip, rest, work);
            if sampled == 0 {
                continue;
            }
            blend_sampled(&chosen[..count], rest, work, false);
            if layer.additive {
                // The difference from the clips' first frames (prepared once, else sampled),
                // added.
                if self.references.len() == self.leaves.len() {
                    self.blend_references(&chosen[..count], rest, work);
                } else {
                    self.sample_leaves(s, &chosen[..count], alpha, true, &clip, rest, work);
                    blend_sampled(&chosen[..count], rest, work, true);
                }
                let GraphWork {
                    layer: now,
                    reference,
                    delta,
                    ..
                } = work;
                now.delta_from(reference, delta);
                add_into(out, delta, weight, layer.mask.as_deref());
            } else {
                lerp_into(out, &work.layer, weight, layer.mask.as_deref());
            }
            posed = true;
        }
        posed
    }

    /// The first frames of the clips additive layers play, sampled once for the rig (`clip`
    /// its clip of an index, `rest` its rest pose): [`Self::pose`] then takes their
    /// differences from these, not from samples made each frame.
    pub fn prepare<'c>(&mut self, clip: impl Fn(u16) -> Option<&'c CompressedClip>, rest: &Pose) {
        let mut additive = vec![false; self.leaves.len()];
        for layer in self.layers.iter().filter(|l| l.additive) {
            self.mark_leaves(layer.root, &mut additive, 0);
        }
        self.references = self
            .leaves
            .iter()
            .zip(&additive)
            .map(|(leaf, &used)| {
                used.then(|| {
                    let mut pose = Pose::identity(rest.len());
                    match clip(leaf.clip) {
                        Some(c) => c.sample(0.0, rest, &mut pose),
                        None => pose.copy_from(rest),
                    }
                    pose
                })
            })
            .collect();
    }

    /// Every leaf node `node` can weigh, marked in `out`.
    fn mark_leaves(&self, node: u16, out: &mut [bool], depth: u32) {
        if depth > DEPTH {
            return;
        }
        match self.nodes.get(usize::from(node)) {
            Some(NodeDef::Clip { leaf, .. }) => out[usize::from(*leaf)] = true,
            Some(
                NodeDef::Blend1d { leaves: (a, b), .. } | NodeDef::Blend2d { leaves: (a, b), .. },
            ) => out[usize::from(*a)..usize::from(*b)].fill(true),
            Some(NodeDef::Machine { machine }) => {
                let m = *machine;
                for st in self.states.iter().filter(|st| st.machine == m) {
                    self.mark_leaves(st.pose, out, depth + 1);
                }
            }
            Some(NodeDef::Missing) | None => {}
        }
    }

    /// The prepared first frames of `chosen` blended by their weights into `work.reference`.
    fn blend_references(&self, chosen: &[(u16, f32)], rest: &Pose, work: &mut GraphWork) {
        let blank = Layer {
            pose: rest,
            weight: 0.0,
            mask: None,
            mode: LayerMode::Normal,
        };
        let mut layers = [blank; MAX_LEAVES];
        for (k, &(leaf, w)) in chosen.iter().enumerate() {
            let pose = self
                .references
                .get(usize::from(leaf))
                .and_then(Option::as_ref)
                .unwrap_or(rest);
            layers[k] = Layer {
                pose,
                weight: w,
                mask: None,
                mode: LayerMode::Normal,
            };
        }
        work.blender
            .blend(rest, &layers[..chosen.len()], &mut work.reference);
    }

    /// The clips `chosen` (leaf, weight) sampled into `work.poses` in order: at their phases
    /// between the ticks, or at their first frames (`first`). A clip `clip` lacks samples as
    /// the rest pose. How many there are.
    #[allow(clippy::too_many_arguments)]
    fn sample_leaves<'c>(
        &self,
        s: &GraphState,
        chosen: &[(u16, f32)],
        alpha: f32,
        first: bool,
        clip: &impl Fn(u16) -> Option<&'c CompressedClip>,
        rest: &Pose,
        work: &mut GraphWork,
    ) -> usize {
        for (slot, &(leaf, _)) in chosen.iter().enumerate() {
            let def = self.leaves[usize::from(leaf)];
            let pose = &mut work.poses[slot];
            match clip(def.clip) {
                Some(c) => {
                    let time = if first {
                        0.0
                    } else {
                        let t = usize::from(def.timed);
                        let (p0, p1) = (s.phase_before[t], s.phase[t]);
                        let p = p0 + (p1 - p0) * alpha;
                        let p = if def.once {
                            p.clamp(0.0, 1.0)
                        } else {
                            p.rem_euclid(1.0)
                        };
                        p * def.length
                    };
                    c.sample(time, rest, pose);
                }
                None => pose.copy_from(rest),
            }
        }
        chosen.len()
    }
}

/// The poses sampled for `chosen` in `work.poses` blended by their weights into `work.layer`
/// (or `work.reference`).
fn blend_sampled(chosen: &[(u16, f32)], rest: &Pose, work: &mut GraphWork, reference: bool) {
    let GraphWork {
        poses,
        layer,
        reference: reference_pose,
        blender,
        ..
    } = work;
    let blank = Layer {
        pose: rest,
        weight: 0.0,
        mask: None,
        mode: LayerMode::Normal,
    };
    let mut layers = [blank; MAX_LEAVES];
    for (k, &(_, w)) in chosen.iter().enumerate() {
        layers[k] = Layer {
            pose: &poses[k],
            weight: w,
            mask: None,
            mode: LayerMode::Normal,
        };
    }
    let out = if reference { reference_pose } else { layer };
    blender.blend(rest, &layers[..chosen.len()], out);
}

/// `out` moved toward `pose` by `weight` (times each joint's `mask` weight).
fn lerp_into(out: &mut Pose, pose: &Pose, weight: f32, mask: Option<&[f32]>) {
    let n = out.len().min(pose.len());
    for j in 0..n {
        let w = weight * mask.and_then(|m| m.get(j)).copied().unwrap_or(1.0);
        if w <= EPSILON {
            continue;
        }
        if w >= 1.0 {
            out.translations[j] = pose.translations[j];
            out.rotations[j] = pose.rotations[j];
            out.scales[j] = pose.scales[j];
            continue;
        }
        out.translations[j] = out.translations[j].lerp(pose.translations[j], w);
        out.scales[j] = out.scales[j].lerp(pose.scales[j], w);
        let to = same_hemisphere(pose.rotations[j], out.rotations[j]);
        let q = Vec4::from(out.rotations[j]).lerp(Vec4::from(to), w);
        out.rotations[j] = Quat::from_vec4(q.normalize_or(Vec4::W));
    }
}

/// Differences `delta` added to `out` by `weight` (times each joint's `mask` weight), as an
/// additive layer is.
fn add_into(out: &mut Pose, delta: &Pose, weight: f32, mask: Option<&[f32]>) {
    let n = out.len().min(delta.len());
    for j in 0..n {
        let w = weight * mask.and_then(|m| m.get(j)).copied().unwrap_or(1.0);
        if w <= EPSILON {
            continue;
        }
        out.translations[j] += delta.translations[j] * w;
        out.scales[j] *= Vec3A::ONE.lerp(delta.scales[j], w);
        let d = same_hemisphere(delta.rotations[j], Quat::IDENTITY);
        let partial = Vec4::W.lerp(Vec4::from(d), w).normalize_or(Vec4::W);
        out.rotations[j] = (out.rotations[j] * Quat::from_vec4(partial)).normalize();
    }
}

/// Point `p`'s barycentric weights in triangle `a b c`, if it lies inside (or on) it.
fn barycentric(p: [f32; 2], a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> Option<[f32; 3]> {
    let (v0, v1, v2) = (
        [b[0] - a[0], b[1] - a[1]],
        [c[0] - a[0], c[1] - a[1]],
        [p[0] - a[0], p[1] - a[1]],
    );
    let den = v0[0] * v1[1] - v1[0] * v0[1];
    if den.abs() < 1e-12 {
        return None;
    }
    let v = (v2[0] * v1[1] - v1[0] * v2[1]) / den;
    let w = (v0[0] * v2[1] - v2[0] * v0[1]) / den;
    let u = 1.0 - v - w;
    let tolerance = -1e-5;
    (u >= tolerance && v >= tolerance && w >= tolerance).then(|| {
        let (u, v, w) = (u.max(0.0), v.max(0.0), w.max(0.0));
        let sum = (u + v + w).max(1e-12);
        [u / sum, v / sum, w / sum]
    })
}

/// The nearest point to `p` on segment `a b`: its fraction from `a`, and its squared
/// distance.
fn nearest_on_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> (f32, f32) {
    let d = [b[0] - a[0], b[1] - a[1]];
    let len = d[0] * d[0] + d[1] * d[1];
    let t = if len < 1e-12 {
        0.0
    } else {
        (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / len).clamp(0.0, 1.0)
    };
    let q = [a[0] + d[0] * t - p[0], a[1] + d[1] * t - p[1]];
    (t, q[0] * q[0] + q[1] * q[1])
}

/// Points `points`' Delaunay triangles (Bowyer–Watson), by index; none for fewer than three
/// points or all on a line.
fn delaunay(points: &[[f32; 2]]) -> Vec<[usize; 3]> {
    let n = points.len();
    if n < 3 {
        return Vec::new();
    }
    // A triangle around them all, its corners past the points.
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for p in points {
        for k in 0..2 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    let size = (hi[0] - lo[0]).max(hi[1] - lo[1]).max(1e-3);
    let mid = [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5];
    let mut all: Vec<[f32; 2]> = points.to_vec();
    all.push([mid[0] - 20.0 * size, mid[1] - size]);
    all.push([mid[0], mid[1] + 20.0 * size]);
    all.push([mid[0] + 20.0 * size, mid[1] - size]);
    let mut tris: Vec<[usize; 3]> = vec![[n, n + 1, n + 2]];
    let circle = |t: &[usize; 3], p: [f32; 2]| -> bool {
        let [a, b, c] = t.map(|k| all[k]);
        let (ax, ay) = (a[0] - p[0], a[1] - p[1]);
        let (bx, by) = (b[0] - p[0], b[1] - p[1]);
        let (cx, cy) = (c[0] - p[0], c[1] - p[1]);
        let det = (ax * ax + ay * ay) * (bx * cy - cx * by)
            - (bx * bx + by * by) * (ax * cy - cx * ay)
            + (cx * cx + cy * cy) * (ax * by - bx * ay);
        // Counterclockwise triangles: inside when positive.
        let orient = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
        if orient > 0.0 { det > 0.0 } else { det < 0.0 }
    };
    for (i, &p) in points.iter().enumerate() {
        let (bad, good): (Vec<[usize; 3]>, Vec<[usize; 3]>) =
            tris.iter().partition(|t| circle(t, p));
        // The hole's edges: those of one bad triangle only.
        let mut edges: Vec<[usize; 2]> = Vec::new();
        for t in &bad {
            for e in [[t[0], t[1]], [t[1], t[2]], [t[2], t[0]]] {
                let shared = bad
                    .iter()
                    .filter(|u| u.contains(&e[0]) && u.contains(&e[1]))
                    .count();
                if shared == 1 {
                    edges.push(e);
                }
            }
        }
        tris = good;
        for e in edges {
            tris.push([e[0], e[1], i]);
        }
    }
    tris.retain(|t| t.iter().all(|&k| k < n));
    // Leave out triangles with no area (points on a line).
    tris.retain(|t| {
        let [a, b, c] = t.map(|k| points[k]);
        ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs() > 1e-9
    });
    tris
}

impl GraphState {
    /// A character's state of graph `graph`: its parameters at their starting values, its
    /// machines at their entries, its clips at their starts.
    pub fn new(graph: &AnimGraph) -> Self {
        let mut params = [0.0f32; MAX_PARAMS];
        for (p, (_, _, v)) in params.iter_mut().zip(graph.params.iter()) {
            *p = *v;
        }
        let timed = graph.timed_once.len();
        let leaves = graph.leaves.len() * graph.layers.len();
        Self {
            params,
            machines: graph
                .machines
                .iter()
                .map(|m| MachineState {
                    state: m.entry,
                    from: NONE,
                    faded: 0.0,
                    fade: 0.0,
                })
                .collect(),
            phase: vec![0.0; timed].into_boxed_slice(),
            phase_before: vec![0.0; timed].into_boxed_slice(),
            node_weight: vec![0.0; timed].into_boxed_slice(),
            leaf_weight: vec![0.0; leaves].into_boxed_slice(),
            leaf_weight_before: vec![0.0; leaves].into_boxed_slice(),
            ticked: false,
        }
    }

    /// Parameter `slot`'s value (0 for none).
    pub fn get(&self, slot: u16) -> f32 {
        self.params.get(usize::from(slot)).copied().unwrap_or(0.0)
    }

    /// Parameter `slot` set to `value` (a flag's 0 or 1).
    pub fn set(&mut self, slot: u16, value: f32) {
        if let Some(p) = self.params.get_mut(usize::from(slot)) {
            *p = value;
        }
    }

    /// Trigger `slot` fired: it holds until a transition takes it.
    pub fn fire(&mut self, slot: u16) {
        if let Some(p) = self.params.get_mut(usize::from(slot)) {
            *p += 1.0;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::{
        CompressSettings, Interpolation, JointDesc, RawClip, RawTrack, Transform, compress,
        rotation_angle,
    };
    use engine_core::math::Vec3;

    /// A root and an arm; clips turning the arm about z by a constant angle, a second long.
    fn rig() -> (Skeleton, Vec<CompressedClip>, Vec<(&'static str, f32)>) {
        let (skeleton, _) = Skeleton::new(&[
            JointDesc {
                name: "root".into(),
                parent: None,
                rest: Transform::IDENTITY,
            },
            JointDesc {
                name: "arm".into(),
                parent: Some(0),
                rest: Transform::new(Vec3::Y, Quat::IDENTITY, Vec3::ONE),
            },
        ])
        .unwrap();
        let angles = vec![
            ("Idle", 0.0),
            ("Walk", 1.0),
            ("Run", 2.0),
            ("Left", -1.0),
            ("Jump", 0.5),
            // Turning from 0 to 1 over its second.
            ("Turn", 0.0),
        ];
        let clips = angles
            .iter()
            .map(|(name, angle)| {
                let mut raw = RawClip {
                    name: (*name).into(),
                    duration: 1.0,
                    joints: vec![Default::default(); 2],
                };
                let end = if *name == "Turn" { 1.0 } else { *angle };
                raw.joints[1].rotation = Some(RawTrack {
                    interpolation: Interpolation::Linear,
                    times: vec![0.0, 1.0],
                    values: vec![Quat::from_rotation_z(*angle), Quat::from_rotation_z(end)],
                });
                compress(&raw, &skeleton, &CompressSettings::default()).0
            })
            .collect();
        (skeleton, clips, angles)
    }

    fn info<'a>(angles: &'a [(&'static str, f32)]) -> impl Fn(&str) -> Option<ClipInfo> + 'a {
        move |name| {
            let k = angles.iter().position(|(n, _)| *n == name)?;
            Some(ClipInfo {
                index: k as u16,
                length: 1.0,
                once: name == "Jump",
            })
        }
    }

    /// Idle and a walk-to-run blend by speed, a jump fired by a trigger from any state.
    fn locomotion() -> GraphDesc {
        GraphDesc {
            params: vec![
                ParamDesc {
                    name: "speed".into(),
                    ..ParamDesc::default()
                },
                ParamDesc {
                    name: "jump".into(),
                    kind: GraphParamKind::Trigger,
                    value: 0.0,
                },
            ],
            layers: vec![LayerDesc {
                name: "Base".into(),
                root: 0,
                weight: 1.0,
                ..LayerDesc::default()
            }],
            nodes: vec![
                NodeDesc::Machine { entry: 0 },
                NodeDesc::Clip {
                    clip: "Idle".into(),
                    speed: 1.0,
                },
                NodeDesc::Blend1d {
                    param: "speed".into(),
                    points: vec![(4.0, "Run".into()), (1.0, "Walk".into())],
                    speed: 1.0,
                },
                NodeDesc::Clip {
                    clip: "Jump".into(),
                    speed: 1.0,
                },
            ],
            states: vec![
                StateDesc {
                    name: "Idle".into(),
                    machine: 0,
                    pose: 1,
                },
                StateDesc {
                    name: "Move".into(),
                    machine: 0,
                    pose: 2,
                },
                StateDesc {
                    name: "Jump".into(),
                    machine: 0,
                    pose: 3,
                },
            ],
            transitions: vec![
                TransitionDesc {
                    from: Some(0),
                    to: 1,
                    when: "speed > 0.1".into(),
                    blend: 0.25,
                    ..TransitionDesc::default()
                },
                TransitionDesc {
                    from: Some(1),
                    to: 0,
                    when: "speed <= 0.1".into(),
                    blend: 0.25,
                    ..TransitionDesc::default()
                },
                TransitionDesc {
                    from: None,
                    to: 2,
                    when: "jump".into(),
                    ..TransitionDesc::default()
                },
                TransitionDesc {
                    from: Some(2),
                    to: 0,
                    exit: 0.9,
                    blend: 0.1,
                    ..TransitionDesc::default()
                },
            ],
        }
    }

    /// The arm's turn about z in a pose.
    fn arm(pose: &Pose) -> f32 {
        let (axis, angle) = pose.rotations[1].to_axis_angle();
        angle * axis.z.signum()
    }

    fn posed(g: &AnimGraph, s: &GraphState, clips: &[CompressedClip], skeleton: &Skeleton) -> f32 {
        let mut work = GraphWork::new(skeleton.joint_count());
        let mut out = Pose::identity(skeleton.joint_count());
        g.pose(
            s,
            1.0,
            |k| clips.get(usize::from(k)),
            skeleton.rest(),
            &mut work,
            &mut out,
        );
        arm(&out)
    }

    /// Idle until it moves, the blend by speed after the fade, a trigger taking it to the jump
    /// and used up, the jump's exit time taking it back.
    #[test]
    fn a_graph_moves_through_its_states_as_its_parameters_say() {
        let (skeleton, clips, angles) = rig();
        let (g, problems) = AnimGraph::cook(&locomotion(), info(&angles), &skeleton);
        assert!(problems.is_empty(), "{problems:?}");
        let mut s = GraphState::new(&g);
        let dt = 1.0 / 64.0;
        g.tick(&mut s, dt);
        assert_eq!(g.state_name(&s, 0), Some("Idle"));
        assert!(posed(&g, &s, &clips, &skeleton).abs() < 1e-3);
        // Moving at 2.5: halfway between the walk (1) and the run (4), after the fade.
        let speed = g.param("speed").unwrap();
        s.set(speed, 2.5);
        g.tick(&mut s, dt);
        assert_eq!(g.state_name(&s, 0), Some("Move"));
        let fading = posed(&g, &s, &clips, &skeleton);
        assert!(fading > 0.0 && fading < 1.4, "fading in: {fading}");
        for _ in 0..20 {
            g.tick(&mut s, dt);
        }
        let moving = posed(&g, &s, &clips, &skeleton);
        assert!((moving - 1.5).abs() < 1e-2, "walk and run halved: {moving}");
        // The trigger: to the jump at once, used up.
        let jump = g.param("jump").unwrap();
        s.fire(jump);
        g.tick(&mut s, dt);
        assert_eq!(g.state_name(&s, 0), Some("Jump"));
        assert_eq!(s.get(jump), 0.0, "the trigger used up");
        assert!((posed(&g, &s, &clips, &skeleton) - 0.5).abs() < 1e-2);
        // Back to idle once nine tenths of the jump played (speed 0 meanwhile).
        s.set(speed, 0.0);
        let mut ticks = 0;
        while g.state_name(&s, 0) == Some("Jump") {
            g.tick(&mut s, dt);
            ticks += 1;
            assert!(ticks < 200, "the jump's exit time");
        }
        let played = ticks as f32 * dt;
        assert!((played - 0.9).abs() <= 2.0 * dt, "left after {played} s");
        assert_eq!(g.state_name(&s, 0), Some("Idle"));
    }

    /// A blend's clips weighed by its parameter: a 1D blend between its two points around the
    /// value, clamped outside them; a 2D blend by barycentric weights inside its triangles and
    /// by the nearest edge outside them.
    #[test]
    fn blends_weigh_their_clips_by_their_parameters() {
        let (skeleton, clips, angles) = rig();
        let mut desc = locomotion();
        desc.transitions.clear();
        desc.states[0].pose = 2;
        let (g, _) = AnimGraph::cook(&desc, info(&angles), &skeleton);
        let mut s = GraphState::new(&g);
        let speed = g.param("speed").unwrap();
        for (v, angle) in [(0.0, 1.0), (1.0, 1.0), (2.5, 1.5), (4.0, 2.0), (9.0, 2.0)] {
            s.set(speed, v);
            g.tick(&mut s, 0.01);
            g.tick(&mut s, 0.01);
            let got = posed(&g, &s, &clips, &skeleton);
            assert!((got - angle).abs() < 1e-2, "speed {v}: {got} for {angle}");
        }
        // 2D: Walk at (0, 1), Run at (0, 2), Left at (-1, 1), Idle at (0, 0).
        let mut desc2 = desc.clone();
        desc2.params.push(ParamDesc {
            name: "turn".into(),
            ..ParamDesc::default()
        });
        desc2.nodes[2] = NodeDesc::Blend2d {
            params: ["turn".into(), "speed".into()],
            points: vec![
                ([0.0, 0.0], "Idle".into()),
                ([0.0, 1.0], "Walk".into()),
                ([0.0, 2.0], "Run".into()),
                ([-1.0, 1.0], "Left".into()),
            ],
            speed: 1.0,
        };
        let (g2, problems) = AnimGraph::cook(&desc2, info(&angles), &skeleton);
        assert!(problems.is_empty(), "{problems:?}");
        let mut s2 = GraphState::new(&g2);
        let (turn, speed) = (g2.param("turn").unwrap(), g2.param("speed").unwrap());
        let at = |s2: &mut GraphState, x: f32, y: f32| {
            s2.set(turn, x);
            s2.set(speed, y);
            g2.tick(s2, 0.01);
            g2.tick(s2, 0.01);
            posed(&g2, s2, &clips, &skeleton)
        };
        assert!((at(&mut s2, 0.0, 1.0) - 1.0).abs() < 1e-2, "on Walk");
        // Halfway from Walk to Left: the two halved.
        assert!(at(&mut s2, -0.5, 1.0).abs() < 1e-2, "Walk and Left halved");
        // Outside, right of Walk: the nearest point of the hull (Walk itself).
        assert!((at(&mut s2, 3.0, 1.0) - 1.0).abs() < 1e-2, "nearest edge");
    }

    /// A layer over the base on the joints its mask names: the arm only, by its weight.
    #[test]
    fn layers_stack_on_the_joints_their_masks_name() {
        let (skeleton, clips, angles) = rig();
        let mut desc = locomotion();
        desc.nodes.push(NodeDesc::Clip {
            clip: "Run".into(),
            speed: 1.0,
        });
        desc.layers.push(LayerDesc {
            name: "Arms".into(),
            root: 4,
            weight: 0.5,
            mask: vec![("arm".into(), 1.0)],
            ..LayerDesc::default()
        });
        let (g, problems) = AnimGraph::cook(&desc, info(&angles), &skeleton);
        assert!(problems.is_empty(), "{problems:?}");
        let mut s = GraphState::new(&g);
        g.tick(&mut s, 0.01);
        // Idle (0) under Run (2) at half: 1.
        let got = posed(&g, &s, &clips, &skeleton);
        assert!((got - 1.0).abs() < 1e-2, "{got}");
        // A mask on the root alone leaves the arm to the base.
        desc.layers[1].mask = vec![("root".into(), 1.0), ("arm".into(), 0.0)];
        let (g, _) = AnimGraph::cook(&desc, info(&angles), &skeleton);
        let mut s = GraphState::new(&g);
        g.tick(&mut s, 0.01);
        assert!(posed(&g, &s, &clips, &skeleton).abs() < 1e-2);
    }

    /// What the cook leaves out is named: a clip the rig lacks, a condition that does not
    /// compile, a mask's joint the skeleton lacks; the rest still plays.
    #[test]
    fn what_cannot_be_cooked_is_named_and_left_out() {
        let (skeleton, clips, angles) = rig();
        let mut desc = locomotion();
        desc.nodes[1] = NodeDesc::Clip {
            clip: "Dance".into(),
            speed: 1.0,
        };
        desc.transitions[0].when = "speeed > 0.1".into();
        desc.layers[0].mask = vec![("tail".into(), 1.0)];
        let (g, problems) = AnimGraph::cook(&desc, info(&angles), &skeleton);
        let named = |what: &str, why: &str| {
            problems
                .iter()
                .any(|p| p.what == what && p.why.contains(why))
        };
        assert!(named("node 1", "no clip `Dance`"), "{problems:?}");
        assert!(
            named("transition 0", "no parameter `speeed`"),
            "{problems:?}"
        );
        assert!(named("layer 0", "no joint `tail`"), "{problems:?}");
        // Idle shows nothing (its clip is missing): the rest pose, and the jump still goes.
        let mut s = GraphState::new(&g);
        g.tick(&mut s, 0.01);
        assert!(posed(&g, &s, &clips, &skeleton).abs() < 1e-3);
        s.fire(g.param("jump").unwrap());
        g.tick(&mut s, 0.01);
        assert_eq!(g.state_name(&s, 0), Some("Jump"));
    }

    /// The frame between two ticks: the phases and weights between them.
    #[test]
    fn a_frame_between_ticks_blends_between_them() {
        let (skeleton, clips, angles) = rig();
        let (g, _) = AnimGraph::cook(&locomotion(), info(&angles), &skeleton);
        let mut s = GraphState::new(&g);
        g.tick(&mut s, 0.01);
        s.set(g.param("speed").unwrap(), 4.0);
        g.tick(&mut s, 0.125);
        // Half a fade of 0.25 s in: the move weighs half at the tick, a quarter halfway to it.
        let mut work = GraphWork::new(2);
        let mut out = Pose::identity(2);
        let at = |alpha: f32, work: &mut GraphWork, out: &mut Pose| {
            g.pose(
                &s,
                alpha,
                |k| clips.get(usize::from(k)),
                skeleton.rest(),
                work,
                out,
            );
            arm(out)
        };
        let (start, half, end) = (
            at(0.0, &mut work, &mut out),
            at(0.5, &mut work, &mut out),
            at(1.0, &mut work, &mut out),
        );
        assert!(start.abs() < 1e-3, "{start}");
        assert!(start < half && half < end, "{start} {half} {end}");
        assert!((end - 1.0).abs() < 0.05, "Run at half: {end}");
    }

    /// An additive layer: its clip's difference from its first frame added over the base (a
    /// clip turning from 0 to 1 adds 0.3 at 0.3 s; one holding still adds nothing); the
    /// first frames prepared once pose as those sampled every frame.
    #[test]
    fn an_additive_layer_adds_its_difference_and_prepared_references_match() {
        let (skeleton, clips, angles) = rig();
        let mut desc = locomotion();
        desc.nodes.push(NodeDesc::Clip {
            clip: "Turn".into(),
            speed: 1.0,
        });
        desc.layers.push(LayerDesc {
            name: "Lean".into(),
            root: 4,
            additive: true,
            weight: 1.0,
            ..LayerDesc::default()
        });
        let (mut g, problems) = AnimGraph::cook(&desc, info(&angles), &skeleton);
        assert!(problems.is_empty(), "{problems:?}");
        let mut s = GraphState::new(&g);
        g.tick(&mut s, 0.3);
        // Idle (0) with the turn's 0.3 added.
        let sampled = posed(&g, &s, &clips, &skeleton);
        assert!((sampled - 0.3).abs() < 1e-2, "{sampled}");
        g.prepare(|k| clips.get(usize::from(k)), skeleton.rest());
        let prepared = posed(&g, &s, &clips, &skeleton);
        assert!((prepared - sampled).abs() < 1e-5, "{prepared} vs {sampled}");
        // A clip holding still (Jump's 0.5 throughout) adds nothing.
        let mut still = desc.clone();
        still.nodes[4] = NodeDesc::Clip {
            clip: "Jump".into(),
            speed: 1.0,
        };
        let (g, _) = AnimGraph::cook(&still, info(&angles), &skeleton);
        let mut s = GraphState::new(&g);
        g.tick(&mut s, 0.3);
        assert!(posed(&g, &s, &clips, &skeleton).abs() < 1e-3);
    }

    /// Delaunay: a square's two triangles; points on a line, none.
    #[test]
    fn delaunay_triangulates_the_points() {
        let tris = delaunay(&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]);
        assert_eq!(tris.len(), 2, "{tris:?}");
        assert!(delaunay(&[[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]]).is_empty());
        let five = delaunay(&[[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0], [1.0, 1.0]]);
        assert_eq!(five.len(), 4, "{five:?}");
        let _ = rotation_angle(Quat::IDENTITY, Quat::IDENTITY);
    }
}
