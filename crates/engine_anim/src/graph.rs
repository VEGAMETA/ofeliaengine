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
//! - **A transition blends** by a cross-fade (both states play) or by inertialization
//!   (Bollo's: only the destination plays, and the source's offset from it, with its
//!   velocity, decays to nothing over the blend). A transition taken while its machine
//!   still blends, or into the pose it leaves, goes on as an inertialization from the pose
//!   shown then: no frozen pose, no stack of sources. [`AnimGraph::capture`], after each
//!   tick, takes the pose such a transition starts from.

use crate::condition::{Condition, ParamSlot};
use crate::pose::same_hemisphere;
use crate::{Blender, CompressedClip, Layer, LayerMode, Pose, Skeleton};
use engine_core::math::{Quat, Vec3, Vec3A, Vec4};

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
/// An offset (or how far its velocity takes it over the blend) below it decays nothing.
const TINY: f32 = 1e-6;
/// The part of the tick before a transition its source's velocity is taken over: the
/// frames move a tick's clips evenly, but a decay playing then bends within the tick.
const SOURCE_STEP: f32 = 0.25;

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
    /// Seconds its blend takes (0: at once).
    pub blend: f32,
    /// Tried in this order among its source's: the smallest first.
    pub priority: i32,
    /// How much of the source must have played first, as a fraction (0: none).
    pub exit: f32,
    /// Where the destination starts, as a fraction.
    pub offset: f32,
    /// The destination starts at the fraction the source played, in place of `offset`.
    pub keep_phase: bool,
    /// Blends by inertialization in place of a cross-fade: only the destination plays, the
    /// source's offset from it decaying over `blend` (half the sampling; best under about
    /// 0.4 s, a cross-fade for longer blends between very different poses).
    pub inertialize: bool,
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
    inertialize: bool,
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
    /// Whether each layer plays each machine, layer by layer.
    layer_machines: Box<[bool]>,
    /// Whether each machine may inertialize: an inertialized transition, a fade something
    /// can cut short, a pose entered again.
    machine_inertial: Box<[bool]>,
    /// Whether each layer may inertialize (it plays such a machine).
    layer_inertial: Box<[bool]>,
    joints: usize,
}

/// A machine's state at run time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MachineState {
    state: u16,
    /// The state fading out (`NONE`: none), and how far into its blend.
    from: u16,
    faded: f32,
    fade: f32,
    /// In an inertialization (its source is not played: its layers decay its offset).
    inertial: bool,
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
    /// Each clip's and blend's phase where the last tick's step began (after its
    /// transitions): what an exit time is checked against, so one that falls within a step
    /// across a loop's seam is still passed (`phase_before` is the next tick's start by
    /// then).
    phase_stepped_from: Box<[f32]>,
    /// Each timed node's weight this tick (over all layers).
    node_weight: Box<[f32]>,
    /// Each layer's leaves' weights, layer by layer.
    leaf_weight: Box<[f32]>,
    leaf_weight_before: Box<[f32]>,
    /// Ticks since it was made: the first one has no tick before to blend from.
    ticked: bool,
    /// The ticks' seconds so far, and the last tick's: where a frame between the last two
    /// ticks is.
    clock: f64,
    dt: f32,
    /// Each clip's and blend's phase and each layer's leaves' weights at the tick before
    /// `phase_before` and `leaf_weight_before`: where a source's velocity comes from (empty
    /// for a graph that never inertializes).
    phase_earlier: Box<[f32]>,
    leaf_weight_earlier: Box<[f32]>,
    /// The next tick has marks of this one's transitions to clear in `inertia`.
    marked: bool,
    /// The clock until which some layer's offsets decay.
    decaying_until: f64,
    /// Its inertializations (only for a graph that may inertialize): a tick touches them
    /// only when it takes a transition, and the tick after.
    inertia: Option<Box<Inertia>>,
}

/// A character's inertializations: the source the tick's transitions leave, and each
/// layer's joints' offsets decaying.
#[derive(Debug, Clone, PartialEq)]
struct Inertia {
    /// The source the tick's transitions leave: phases, then weights, as the tick before
    /// began (the first half) and ended (the second).
    source_phase: Box<[f32]>,
    source_weight: Box<[f32]>,
    /// The tick before's seconds.
    source_dt: f32,
    /// This tick has kept the source.
    taken: bool,
    /// Some layer waits for [`AnimGraph::capture`].
    pending: bool,
    /// Per layer.
    layers: Box<[LayerInertia]>,
}

/// A layer's inertialization.
#[derive(Debug, Clone, PartialEq, Default)]
struct LayerInertia {
    /// The clock when it began, and the seconds it takes.
    began: f64,
    time: f32,
    /// Its joints' offsets decay over what the layer plays (until `time` after `began`).
    live: bool,
    /// Begun by the last tick: waits for [`AnimGraph::capture`].
    pending: bool,
    /// The decay that played when it began goes into its source, `carried` seconds in.
    carry: bool,
    carried: f32,
    /// A transition cut to its destination at once this tick, and at the tick before: the
    /// tick after a cut tells nothing of the source's velocity.
    cut: bool,
    cut_before: bool,
    /// Per joint (none: the layer never inertializes).
    joints: Box<[JointDecay]>,
}

/// A joint's offsets from its destination decaying ([`decay_weights`]): the rotation's,
/// translation's and scale's at the transition and their velocities, then each one's
/// seconds (0: none) and brake.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct JointDecay {
    rotation: Vec4,
    spin: Vec4,
    translation: Vec3,
    shift: Vec3,
    scale: Vec3,
    stretch: Vec3,
    time: [f32; 3],
    brake: [f32; 3],
}

/// What [`AnimGraph::pose`] and [`AnimGraph::capture`] work in: poses for a layer's clips,
/// sized for a rig. Made once per rig size; posing allocates nothing.
#[derive(Debug, Clone)]
pub struct GraphWork {
    poses: Vec<Pose>,
    layer: Pose,
    reference: Pose,
    delta: Pose,
    /// A capture's source a step before the transition and at it, then its destination at
    /// the transition and a tick after.
    held: [Pose; 4],
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
            held: std::array::from_fn(|_| Pose::identity(joints)),
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
                    inertialize: desc_t.inertialize,
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
        let mut graph = Self {
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
            layer_machines: Box::default(),
            machine_inertial: Box::default(),
            layer_inertial: Box::default(),
            joints,
        };
        graph.plan_inertia();
        (graph, problems)
    }

    /// Which machines may inertialize (an inertialized transition; a fade into a state that
    /// can be left, so something may cut it short; a pose entered again, which cannot fade
    /// into itself), and which layers play them: only those get room for it.
    fn plan_inertia(&mut self) {
        let machines = self.machines.len();
        let leaves_state = |state: u16| {
            self.states.get(usize::from(state)).is_some_and(|st| {
                st.transitions.0 < st.transitions.1
                    || self
                        .machines
                        .get(usize::from(st.machine))
                        .is_some_and(|m| m.any.0 < m.any.1)
            })
        };
        let needs = |def: &TransitionDef, from_pose: Option<u16>| {
            let to_pose = self.states.get(usize::from(def.to)).map(|st| st.pose);
            def.blend > 0.0
                && (def.inertialize
                    || leaves_state(def.to)
                    || (from_pose.is_some() && from_pose == to_pose))
        };
        let mut inertial = vec![false; machines];
        for st in self.states.iter() {
            let own =
                &self.transitions[usize::from(st.transitions.0)..usize::from(st.transitions.1)];
            if own.iter().any(|def| needs(def, Some(st.pose)))
                && let Some(flag) = inertial.get_mut(usize::from(st.machine))
            {
                *flag = true;
            }
        }
        for (m, def) in self.machines.iter().enumerate() {
            let any = &self.transitions[usize::from(def.any.0)..usize::from(def.any.1)];
            if any.iter().any(|def| needs(def, None)) {
                inertial[m] = true;
            }
        }
        let mut reach = vec![false; self.layers.len() * machines];
        for (l, layer) in self.layers.iter().enumerate() {
            self.mark_machines(layer.root, &mut reach[l * machines..(l + 1) * machines], 0);
        }
        self.layer_inertial = (0..self.layers.len())
            .map(|l| (0..machines).any(|m| reach[l * machines + m] && inertial[m]))
            .collect();
        self.layer_machines = reach.into_boxed_slice();
        self.machine_inertial = inertial.into_boxed_slice();
    }

    /// Every machine node `node` plays (itself, and those its states show), marked in `out`.
    fn mark_machines(&self, node: u16, out: &mut [bool], depth: u32) {
        if depth > DEPTH {
            return;
        }
        let Some(NodeDef::Machine { machine }) = self.nodes.get(usize::from(node)) else {
            return;
        };
        match out.get_mut(usize::from(*machine)) {
            Some(seen) if !*seen => *seen = true,
            _ => return,
        }
        for st in self.states.iter().filter(|st| st.machine == *machine) {
            self.mark_machines(st.pose, out, depth + 1);
        }
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
    /// clips weighed and their phases moved on. Allocates nothing. Where a transition
    /// inertializes, [`Self::capture`] takes the pose it starts from before the next tick.
    pub fn tick(&self, s: &mut GraphState, dt: f32) {
        if !s.phase_earlier.is_empty() {
            // The tick before's start kept, for a source's velocity.
            std::mem::swap(&mut s.phase_earlier, &mut s.phase_before);
            std::mem::swap(&mut s.leaf_weight_earlier, &mut s.leaf_weight_before);
        }
        if s.marked
            && let Some(i) = s.inertia.as_deref_mut()
        {
            // The last tick's marks cleared (a transition never captured plays without its
            // offsets); a cut's kept a tick more.
            i.taken = false;
            i.pending = false;
            let mut cut = false;
            for layer in i.layers.iter_mut() {
                layer.pending = false;
                layer.cut_before = layer.cut;
                layer.cut = false;
                cut |= layer.cut_before;
            }
            s.marked = cut;
        }
        s.phase_before.copy_from_slice(&s.phase);
        s.leaf_weight_before.copy_from_slice(&s.leaf_weight);
        let dt = dt.max(0.0);
        // Each machine's transitions (taken at the tick's start), then its blend on by the
        // tick: a blend begun now is `dt` in at its end.
        for m in 0..self.machines.len() {
            for _ in 0..CHAIN {
                let Some(t) = self.choose(m, s) else {
                    break;
                };
                self.take(m, t, s);
            }
            let ms = &mut s.machines[m];
            if ms.from != NONE || ms.inertial {
                ms.faded += dt;
                if ms.faded >= ms.fade {
                    ms.from = NONE;
                    ms.inertial = false;
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
        // A layer inertializing from this tick shows its destination alone from the tick's
        // start (its offset makes up the difference).
        if s.marked
            && let Some(i) = s.inertia.as_deref()
            && i.pending
        {
            for (l, layer) in i.layers.iter().enumerate() {
                if layer.pending {
                    let range = l * leaves..(l + 1) * leaves;
                    s.leaf_weight_before[range.clone()].copy_from_slice(&s.leaf_weight[range]);
                }
            }
        }
        s.phase_stepped_from.copy_from_slice(&s.phase);
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
                // Whole loops dropped from all, so the step between them stays.
                let whole = phase.floor();
                phase -= whole;
                s.phase_before[t] -= whole;
                s.phase_stepped_from[t] -= whole;
            }
            s.phase[t] = phase;
        }
        s.clock += f64::from(dt);
        s.dt = dt;
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
        let (now, before) = (s.phase[t], s.phase_stepped_from[t]);
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
    /// its offset or the source's fraction, the source fading out over its blend, or its
    /// layers inertializing from what they show (an inertialized transition, one taken while
    /// the machine still blends, one into the pose it leaves).
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
        // The source as the tick before left it, kept once a tick before anything is
        // entered: a later transition of the tick may blend from it.
        if s.ticked
            && let Some(i) = s.inertia.as_deref_mut()
            && !i.taken
        {
            let timed = s.phase.len();
            i.source_phase[..timed].copy_from_slice(&s.phase_earlier);
            i.source_phase[timed..].copy_from_slice(&s.phase_before);
            let n = s.leaf_weight.len();
            i.source_weight[..n].copy_from_slice(&s.leaf_weight_earlier);
            i.source_weight[n..].copy_from_slice(&s.leaf_weight_before);
            i.source_dt = s.dt;
            i.taken = true;
            s.marked = true;
        }
        let ms = s.machines[m];
        let pose_of = |state: u16| self.states.get(usize::from(state)).map(|st| st.pose);
        let wants = def.blend > 0.0
            && (def.inertialize
                || ms.from != NONE
                || ms.inertial
                || pose_of(source) == pose_of(def.to));
        let inertial = wants
            && s.ticked
            && s.inertia.is_some()
            && self.machine_inertial.get(m).copied().unwrap_or(false);
        let ms = &mut s.machines[m];
        ms.faded = 0.0;
        ms.fade = def.blend;
        ms.inertial = inertial;
        // A cross-fade; else the source is not played (inertialized, taken at once, or at
        // the first tick, where nothing was shown to blend from).
        ms.from = if def.blend > 0.0 && !inertial && (s.ticked || !wants) {
            source
        } else {
            NONE
        };
        ms.state = def.to;
        self.begin(m, def.blend, inertial, s);
        let start = if def.keep_phase {
            source_phase - source_phase.floor()
        } else {
            def.offset
        };
        self.enter(def.to, start, s, 0);
    }

    /// Machine `m`'s layers after a transition: inertializing over `time` seconds from what
    /// they show (a decay playing carried in), waiting for [`Self::capture`]; or, for a
    /// transition taken at once, marked cut.
    fn begin(&self, m: usize, time: f32, inertial: bool, s: &mut GraphState) {
        let Some(i) = s.inertia.as_deref_mut() else {
            return;
        };
        if !inertial && time > 0.0 {
            return;
        }
        // Transitions are taken at the tick's start.
        let now = s.clock;
        let machines = self.machines.len();
        for (l, layer) in i.layers.iter_mut().enumerate() {
            if layer.joints.is_empty() || !self.layer_machines[l * machines + m] {
                continue;
            }
            s.marked = true;
            if !inertial {
                layer.cut = true;
                continue;
            }
            if !layer.pending {
                layer.carried = (now - layer.began) as f32;
                layer.carry = layer.live && layer.carried < layer.time;
                layer.live = false;
                layer.pending = true;
                layer.began = now;
            }
            layer.time = time;
            i.pending = true;
        }
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
                inertial: false,
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
    /// clip of an index) at their phases and weights between the two ticks, blended, an
    /// inertialization's offsets decayed over them, and the layers stacked over the rest
    /// pose `rest`. Allocates nothing; false when nothing plays (`out` is then the rest
    /// pose).
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
        // The frame's moment on the ticks' clock.
        let now = s.clock - f64::from(s.dt * (1.0 - alpha));
        let decaying = now < s.decaying_until;
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
            let range = l * leaves..(l + 1) * leaves;
            let weights = (&s.leaf_weight_before[range.clone()], &s.leaf_weight[range]);
            let phases = (&s.phase_before[..], &s.phase[..]);
            if !self.layer_pose(l, weights, phases, alpha, &clip, rest, work) {
                continue;
            }
            if decaying
                && let Some(inertia) = s.inertia.as_deref()
                && let Some(decay) = inertia.layers.get(l)
                && decay.live
            {
                decay_pose(&decay.joints, (now - decay.began) as f32, &mut work.layer);
            }
            if layer.additive {
                add_into(out, &work.layer, weight, layer.mask.as_deref());
            } else {
                lerp_into(out, &work.layer, weight, layer.mask.as_deref());
            }
            posed = true;
        }
        posed
    }

    /// The pose each layer the last tick began inertializing blends from, taken: the layer
    /// as it was shown at the transition and just before (a decay then playing included),
    /// against its destination at the transition and a tick after, each joint's offset and
    /// velocity starting its decay. Call it after each tick, before the next one and before
    /// [`Self::pose`], with the same clips: it samples nothing unless the tick took such a
    /// transition (a transition never captured plays without its offset). Allocates
    /// nothing; whether it took any.
    pub fn capture<'c>(
        &self,
        s: &mut GraphState,
        clip: impl Fn(u16) -> Option<&'c CompressedClip>,
        rest: &Pose,
        work: &mut GraphWork,
    ) -> bool {
        let GraphState {
            phase,
            phase_before,
            leaf_weight,
            leaf_weight_before,
            dt,
            decaying_until,
            inertia,
            ..
        } = s;
        let Some(i) = inertia.as_deref_mut() else {
            return false;
        };
        if !i.pending {
            return false;
        }
        i.pending = false;
        let leaves = self.leaves.len();
        let all = leaf_weight.len();
        let (earlier, before) = i.source_phase.split_at(phase.len());
        let mut captured = false;
        for l in 0..i.layers.len() {
            if !i.layers[l].pending {
                continue;
            }
            i.layers[l].pending = false;
            let range = l * leaves..(l + 1) * leaves;
            let at = |start: usize| &i.source_weight[start + l * leaves..start + (l + 1) * leaves];
            let weights = (at(0), at(all));
            let decay = &i.layers[l];
            // The source at the transition and a step before, as the frames showed it.
            let mut shown = true;
            for (k, alpha) in [1.0 - SOURCE_STEP, 1.0].into_iter().enumerate() {
                shown &= self.layer_pose(l, weights, (earlier, before), alpha, &clip, rest, work);
                if decay.carry {
                    let back = (1.0 - alpha) * i.source_dt;
                    decay_pose(&decay.joints, decay.carried - back, &mut work.layer);
                }
                std::mem::swap(&mut work.layer, &mut work.held[k]);
            }
            // The destination at the transition and a tick after.
            let destination = [
                (&leaf_weight_before[range.clone()], &phase_before[..]),
                (&leaf_weight[range], &phase[..]),
            ];
            for (k, (weights, phases)) in destination.into_iter().enumerate() {
                shown &= self.layer_pose(
                    l,
                    (weights, weights),
                    (phases, phases),
                    0.0,
                    &clip,
                    rest,
                    work,
                );
                std::mem::swap(&mut work.layer, &mut work.held[2 + k]);
            }
            let layer = &mut i.layers[l];
            if !shown {
                // Nothing played on one side: a cut.
                layer.live = false;
                continue;
            }
            // After a cut the tick before tells nothing of the source's velocity.
            let source_step = if layer.cut_before {
                0.0
            } else {
                SOURCE_STEP * i.source_dt
            };
            let [s0, s1, d0, d1] = &work.held;
            start_decays(
                &mut layer.joints,
                [s0, s1, d0, d1],
                (source_step, *dt),
                layer.time,
                self.layers[l].mask.as_deref(),
            );
            layer.live = true;
            *decaying_until = decaying_until.max(layer.began + f64::from(layer.time));
            captured = true;
        }
        captured
    }

    /// Layer `l`'s pose into `work.layer`: its clips (the heaviest [`MAX_LEAVES`]) at
    /// `weights` (its leaves', at the last two ticks) and `phases` (every clip's and
    /// blend's, at the last two ticks) `alpha` of the way between, blended; for an additive
    /// layer their difference from their first frames. False when nothing plays.
    #[allow(clippy::too_many_arguments)]
    fn layer_pose<'c>(
        &self,
        l: usize,
        weights: (&[f32], &[f32]),
        phases: (&[f32], &[f32]),
        alpha: f32,
        clip: &impl Fn(u16) -> Option<&'c CompressedClip>,
        rest: &Pose,
        work: &mut GraphWork,
    ) -> bool {
        let Some(layer) = self.layers.get(l) else {
            return false;
        };
        // The layer's clips, the heaviest when more than room.
        let mut chosen = [(0u16, 0.0f32); MAX_LEAVES];
        let mut count = 0usize;
        for (k, (&w0, &w1)) in weights.0.iter().zip(weights.1).enumerate() {
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
            return false;
        }
        let chosen = &chosen[..count];
        self.sample_leaves(chosen, phases, alpha, false, clip, rest, work);
        blend_sampled(chosen, rest, work, false);
        if layer.additive {
            // The difference from the clips' first frames (prepared once, else sampled).
            if self.references.len() == self.leaves.len() {
                self.blend_references(chosen, rest, work);
            } else {
                self.sample_leaves(chosen, phases, alpha, true, clip, rest, work);
                blend_sampled(chosen, rest, work, true);
            }
            let GraphWork {
                layer: now,
                reference,
                delta,
                ..
            } = work;
            now.delta_from(reference, delta);
            std::mem::swap(now, delta);
        }
        true
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
    /// `alpha` of the way between `phases` (every clip's and blend's at the last two ticks),
    /// or at their first frames (`first`). A clip `clip` lacks samples as the rest pose.
    #[allow(clippy::too_many_arguments)]
    fn sample_leaves<'c>(
        &self,
        chosen: &[(u16, f32)],
        phases: (&[f32], &[f32]),
        alpha: f32,
        first: bool,
        clip: &impl Fn(u16) -> Option<&'c CompressedClip>,
        rest: &Pose,
        work: &mut GraphWork,
    ) {
        for (slot, &(leaf, _)) in chosen.iter().enumerate() {
            let def = self.leaves[usize::from(leaf)];
            let pose = &mut work.poses[slot];
            match clip(def.clip) {
                Some(c) => {
                    let time = if first {
                        0.0
                    } else {
                        let t = usize::from(def.timed);
                        let (p0, p1) = (phases.0[t], phases.1[t]);
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
    }
}

/// Bollo's decay at `u` (0 to 1) of its `time`: the weights of an offset and of its velocity.
/// The quintic `x(t)` that starts at the offset `x0` with its velocity `v0` and initial
/// acceleration `a0`, and ends at nothing with no velocity or acceleration, written as
/// `x0·P(u) + v0·time·Q(u) + a0·time²·R(u)`; `brake` is `a0·time²/x0`.
/// - `P(u) = (1-u)³(1 + 3u + 6u²)`, `Q(u) = u(1-u)³(1 + 3u)`, `R(u) = u²(1-u)³/2`.
#[inline]
fn decay_weights(u: f32, time: f32, brake: f32) -> (f32, f32) {
    let w = 1.0 - u;
    let w3 = w * w * w;
    let offset = w3 * (1.0 + 3.0 * u + 6.0 * u * u + 0.5 * brake * u * u);
    let velocity = time * u * w3 * (1.0 + 3.0 * u);
    (offset, velocity)
}

/// How long an offset of length `size`, its velocity `along` it and `speed` in all, takes
/// to decay within `time`, and its brake (Bollo's rules): where it moves toward the
/// destination fast enough to pass it, braking from the start (`a0 > 0`), its time cut to
/// `5·size/|along|` if even that passes it; else a gentle start (`a0 = 0`). It never
/// overshoots along itself. `(0, 0)`: nothing to decay.
fn decay_shape(size: f32, along: f32, speed: f32, time: f32) -> (f32, f32) {
    if time <= 0.0 || (size <= TINY && speed * time <= TINY) {
        return (0.0, 0.0);
    }
    if size <= TINY {
        // Moving only: no direction to brake along.
        return (time, 0.0);
    }
    let time = if along < 0.0 {
        time.min(-5.0 * size / along)
    } else {
        time
    };
    let k = along * time / size;
    (time, (-8.0 * k - 20.0).max(0.0))
}

/// The Hamilton product of two quaternions held as 4 numbers (not only unit ones).
#[inline]
fn quat_mul(a: Vec4, b: Vec4) -> Vec4 {
    Vec4::new(
        a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
        a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
        a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
        a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z,
    )
}

/// Each joint's decays begun from the source's poses a step before the transition and at
/// it (`poses[0]`, `[1]`) and the destination's at it and a tick after (`[2]`, `[3]`),
/// those steps `dt` seconds (source, destination; 0: no velocity) long: the offset of the
/// source from the destination, and the difference of their velocities (rotations as
/// `Δ = source · destination⁻¹` and its rate), each decaying over `time`. Joints `mask`
/// leaves out decay nothing.
fn start_decays(
    joints: &mut [JointDecay],
    poses: [&Pose; 4],
    dt: (f32, f32),
    time: f32,
    mask: Option<&[f32]>,
) {
    let [s0, s1, d0, d1] = poses;
    let n = joints
        .len()
        .min(s0.len())
        .min(s1.len())
        .min(d0.len())
        .min(d1.len());
    let rate = |dt: f32| if dt > 0.0 { 1.0 / dt } else { 0.0 };
    let (source_rate, destination_rate) = (rate(dt.0), rate(dt.1));
    let linear = |a0: Vec3A, a1: Vec3A, b0: Vec3A, b1: Vec3A| {
        let offset = Vec3::from(a1 - b0);
        let velocity = Vec3::from((a1 - a0) * source_rate - (b1 - b0) * destination_rate);
        let size = offset.length();
        let along = if size > TINY {
            velocity.dot(offset) / size
        } else {
            0.0
        };
        let (t, brake) = decay_shape(size, along, velocity.length(), time);
        (offset, velocity, t, brake)
    };
    for (j, decay) in joints.iter_mut().enumerate().take(n) {
        *decay = JointDecay::default();
        if mask.and_then(|m| m.get(j)).copied().unwrap_or(1.0) <= EPSILON {
            continue;
        }
        // Rotation: the offset `Δ` and its rate `Δ'`, as 4 numbers (`Δ'` along the sphere).
        let q1 = Vec4::from(s1.rotations[j]);
        let q0 = Vec4::from(same_hemisphere(s0.rotations[j], s1.rotations[j]));
        let p0 = Vec4::from(d0.rotations[j]);
        let p1 = Vec4::from(same_hemisphere(d1.rotations[j], d0.rotations[j]));
        let inverse = Vec4::new(-p0.x, -p0.y, -p0.z, p0.w);
        let mut offset = quat_mul(q1, inverse);
        // (s·d⁻¹)' = s'·d⁻¹ − s·d⁻¹·d'·d⁻¹
        let mut spin = quat_mul((q1 - q0) * source_rate, inverse)
            - quat_mul(quat_mul(offset, (p1 - p0) * destination_rate), inverse);
        if offset.w < 0.0 {
            offset = -offset;
            spin = -spin;
        }
        spin -= offset * offset.dot(spin);
        let chord = offset - Vec4::W;
        let size = chord.length();
        let along = if size > TINY {
            spin.dot(chord) / size
        } else {
            0.0
        };
        let (t, brake) = decay_shape(size, along, spin.length(), time);
        decay.rotation = offset;
        decay.spin = spin;
        decay.time[0] = t;
        decay.brake[0] = brake;
        let (offset, velocity, t, brake) = linear(
            s0.translations[j],
            s1.translations[j],
            d0.translations[j],
            d1.translations[j],
        );
        decay.translation = offset;
        decay.shift = velocity;
        decay.time[1] = t;
        decay.brake[1] = brake;
        let (offset, velocity, t, brake) =
            linear(s0.scales[j], s1.scales[j], d0.scales[j], d1.scales[j]);
        decay.scale = offset;
        decay.stretch = velocity;
        decay.time[2] = t;
        decay.brake[2] = brake;
    }
}

/// Joints' decays `at` seconds in applied over `pose` (the destination): the offsets and
/// velocities weighed by [`decay_weights`] added, a rotation's as `Δ(t) · rotation` with
/// `Δ(t)` from `Δ` toward no turn along the sphere's chord, normalized.
fn decay_pose(joints: &[JointDecay], at: f32, pose: &mut Pose) {
    let at = at.max(0.0);
    let n = joints.len().min(pose.len());
    for (j, decay) in joints.iter().enumerate().take(n) {
        let [rotation, translation, scale] = decay.time;
        if at < rotation {
            let (p, q) = decay_weights(at / rotation, rotation, decay.brake[0]);
            let turn = Vec4::W * (1.0 - p) + decay.rotation * p + decay.spin * q;
            let turn = Quat::from_vec4(turn.normalize_or(Vec4::W));
            pose.rotations[j] = (turn * pose.rotations[j]).normalize();
        }
        if at < translation {
            let (p, q) = decay_weights(at / translation, translation, decay.brake[1]);
            pose.translations[j] += Vec3A::from(decay.translation * p + decay.shift * q);
        }
        if at < scale {
            let (p, q) = decay_weights(at / scale, scale, decay.brake[2]);
            pose.scales[j] += Vec3A::from(decay.scale * p + decay.stretch * q);
        }
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
        // Room to inertialize only where the graph may: per joint of the layers that may.
        let inertial = graph.layer_inertial.iter().any(|&b| b);
        let earlier = |n: usize| vec![0.0; if inertial { n } else { 0 }].into_boxed_slice();
        let inertia = inertial.then(|| {
            Box::new(Inertia {
                source_phase: vec![0.0; 2 * timed].into_boxed_slice(),
                source_weight: vec![0.0; 2 * leaves].into_boxed_slice(),
                source_dt: 0.0,
                taken: false,
                pending: false,
                layers: graph
                    .layer_inertial
                    .iter()
                    .map(|&inertial| LayerInertia {
                        joints: if inertial {
                            vec![JointDecay::default(); graph.joints].into_boxed_slice()
                        } else {
                            Box::default()
                        },
                        ..LayerInertia::default()
                    })
                    .collect(),
            })
        });
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
                    inertial: false,
                })
                .collect(),
            phase: vec![0.0; timed].into_boxed_slice(),
            phase_before: vec![0.0; timed].into_boxed_slice(),
            phase_stepped_from: vec![0.0; timed].into_boxed_slice(),
            node_weight: vec![0.0; timed].into_boxed_slice(),
            leaf_weight: vec![0.0; leaves].into_boxed_slice(),
            leaf_weight_before: vec![0.0; leaves].into_boxed_slice(),
            ticked: false,
            clock: 0.0,
            dt: 0.0,
            phase_earlier: earlier(timed),
            leaf_weight_earlier: earlier(leaves),
            marked: false,
            decaying_until: 0.0,
            inertia,
        }
    }

    /// Bytes in memory: what a character playing its graph costs, an inertializing graph's
    /// decays (every joint of each layer that may inertialize) included.
    pub fn size_bytes(&self) -> usize {
        let floats = self.phase.len() * 4
            + self.leaf_weight.len() * 2
            + self.phase_earlier.len()
            + self.leaf_weight_earlier.len();
        let mut bytes = size_of::<Self>()
            + floats * size_of::<f32>()
            + self.machines.len() * size_of::<MachineState>();
        if let Some(i) = &self.inertia {
            bytes += size_of::<Inertia>()
                + (i.source_phase.len() + i.source_weight.len()) * size_of::<f32>()
                + i.layers.len() * size_of::<LayerInertia>()
                + i.layers
                    .iter()
                    .map(|l| l.joints.len() * size_of::<JointDecay>())
                    .sum::<usize>();
        }
        bytes
    }

    /// Whether the last tick began an inertialization that waits for
    /// [`AnimGraph::capture`]: a caller can leave its clips and work out until one does.
    pub fn wants_capture(&self) -> bool {
        self.marked && self.inertia.as_deref().is_some_and(|i| i.pending)
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

    /// An exit time within a tick's step of a looping clip's end is passed when the step
    /// crosses the loop's seam: at 64 Hz a 1 s clip goes 0.984 → 1.0 (its fraction 0), past
    /// 0.99 between the two ticks. Before 2026-10-30 the check compared the phase with
    /// itself and never took it.
    #[test]
    fn an_exit_time_across_a_loops_seam_is_passed() {
        let (skeleton, _clips, angles) = rig();
        let mut desc = locomotion();
        desc.transitions = vec![TransitionDesc {
            from: Some(0),
            to: 2,
            exit: 0.99,
            ..TransitionDesc::default()
        }];
        let (g, problems) = AnimGraph::cook(&desc, info(&angles), &skeleton);
        assert!(problems.is_empty(), "{problems:?}");
        let mut s = GraphState::new(&g);
        let taken = (1..=200).find(|_| {
            g.tick(&mut s, 1.0 / 64.0);
            g.state_name(&s, 0) == Some("Jump")
        });
        // Idle's phase reaches 1.0 at the 65th tick's start: the transition is taken then.
        assert_eq!(taken, Some(65), "taken at the seam's crossing");
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

    const DT: f32 = 1.0 / 64.0;

    /// The arm's turn `alpha` of the way from the last tick to the next, and how many times
    /// clip `watched` was sampled for it.
    fn arm_at(
        g: &AnimGraph,
        s: &GraphState,
        clips: &[CompressedClip],
        skeleton: &Skeleton,
        alpha: f32,
        watched: u16,
    ) -> (f32, usize) {
        let mut work = GraphWork::new(skeleton.joint_count());
        let mut out = Pose::identity(skeleton.joint_count());
        let seen = std::cell::Cell::new(0usize);
        g.pose(
            s,
            alpha,
            |k| {
                if k == watched {
                    seen.set(seen.get() + 1);
                }
                clips.get(usize::from(k))
            },
            skeleton.rest(),
            &mut work,
            &mut out,
        );
        (arm(&out), seen.get())
    }

    /// A tick, then its capture.
    fn step(
        g: &AnimGraph,
        s: &mut GraphState,
        clips: &[CompressedClip],
        skeleton: &Skeleton,
    ) -> bool {
        let mut work = GraphWork::new(skeleton.joint_count());
        g.tick(s, DT);
        g.capture(s, |k| clips.get(usize::from(k)), skeleton.rest(), &mut work)
    }

    /// The arm's speed just before the last tick's start (its last quarter of a tick before)
    /// and just after it (its first quarter), from the frames around it: `before` posed
    /// before that tick ran.
    fn speeds(before: (f32, f32), after: (f32, f32)) -> (f32, f32) {
        (
            (before.1 - before.0) / (0.25 * DT),
            (after.1 - after.0) / (0.25 * DT),
        )
    }

    /// Spinning (the turn clip) until `go`, then running (a still pose) after an
    /// inertialized blend of a quarter second; back to spinning when `go` is cleared.
    fn spin_then_run(inertialize: bool) -> GraphDesc {
        GraphDesc {
            params: vec![ParamDesc {
                name: "go".into(),
                kind: GraphParamKind::Bool,
                value: 0.0,
            }],
            layers: vec![LayerDesc {
                name: "Base".into(),
                root: 0,
                weight: 1.0,
                ..LayerDesc::default()
            }],
            nodes: vec![
                NodeDesc::Machine { entry: 0 },
                NodeDesc::Clip {
                    clip: "Turn".into(),
                    speed: 1.0,
                },
                NodeDesc::Clip {
                    clip: "Run".into(),
                    speed: 1.0,
                },
            ],
            states: vec![
                StateDesc {
                    name: "Spin".into(),
                    machine: 0,
                    pose: 1,
                },
                StateDesc {
                    name: "Run".into(),
                    machine: 0,
                    pose: 2,
                },
            ],
            transitions: vec![
                TransitionDesc {
                    from: Some(0),
                    to: 1,
                    when: "go".into(),
                    blend: 0.25,
                    inertialize,
                    ..TransitionDesc::default()
                },
                TransitionDesc {
                    from: Some(1),
                    to: 0,
                    when: "not go".into(),
                    blend: 0.25,
                    inertialize,
                    ..TransitionDesc::default()
                },
            ],
        }
    }

    /// An inertialized transition: the pose right after it is the source's, moving at the
    /// source's speed; only the destination is sampled while it blends; after its blend the
    /// pose is the destination's exactly.
    #[test]
    fn an_inertialized_transition_starts_at_its_source_and_ends_at_its_destination() {
        let (skeleton, clips, angles) = rig();
        let (g, problems) = AnimGraph::cook(&spin_then_run(true), info(&angles), &skeleton);
        assert!(problems.is_empty(), "{problems:?}");
        let turn = g.leaves[0].clip;
        let mut s = GraphState::new(&g);
        for _ in 0..20 {
            step(&g, &mut s, &clips, &skeleton);
        }
        let at = |s: &GraphState, alpha| arm_at(&g, s, &clips, &skeleton, alpha, turn);
        let before = (at(&s, 0.75).0, at(&s, 1.0).0);
        assert!(
            (before.1 - 20.0 * DT).abs() < 1e-2,
            "spinning: {}",
            before.1
        );
        s.set(g.param("go").unwrap(), 1.0);
        assert!(step(&g, &mut s, &clips, &skeleton), "the source captured");
        assert_eq!(g.state_name(&s, 0), Some("Run"));
        let after = (at(&s, 0.0).0, at(&s, 0.25).0);
        assert!(
            (after.0 - before.1).abs() < 1e-5,
            "no pop: {} after {}",
            after.0,
            before.1
        );
        let (was, is) = speeds(before, after);
        assert!(
            (is - was).abs() < 0.05 * was.abs(),
            "speed {is} after {was}"
        );
        // While it blends (16 ticks of a quarter second), the turn is never sampled; then
        // the run alone.
        for _ in 0..16 {
            for alpha in [0.0, 0.5, 1.0] {
                let (angle, sampled) = at(&s, alpha);
                assert_eq!(sampled, 0, "only the destination is sampled");
                assert!(angle > before.1 - 1e-4 && angle < 2.0 + 1e-4, "{angle}");
            }
            step(&g, &mut s, &clips, &skeleton);
        }
        for alpha in [0.0, 0.5, 1.0] {
            let (angle, _) = at(&s, alpha);
            assert!(
                (angle - 2.0).abs() < 1e-5,
                "ended at the destination: {angle}"
            );
        }
        assert!(
            s.clock - f64::from(s.dt) >= s.decaying_until,
            "nothing decays"
        );
    }

    /// A fade cut short by the transition back: it goes on from the pose shown then (no
    /// pop, the same speed), never samples the state fading out again, and ends at the new
    /// destination after its blend.
    #[test]
    fn an_interrupted_fade_goes_on_from_the_pose_it_shows() {
        let (skeleton, clips, angles) = rig();
        let (g, problems) = AnimGraph::cook(&locomotion(), info(&angles), &skeleton);
        assert!(problems.is_empty(), "{problems:?}");
        let run = angles.iter().position(|(n, _)| *n == "Run").unwrap() as u16;
        let mut s = GraphState::new(&g);
        step(&g, &mut s, &clips, &skeleton);
        let speed = g.param("speed").unwrap();
        s.set(speed, 4.0);
        for _ in 0..8 {
            step(&g, &mut s, &clips, &skeleton);
        }
        let at = |s: &GraphState, alpha| arm_at(&g, s, &clips, &skeleton, alpha, run);
        let before = (at(&s, 0.75).0, at(&s, 1.0).0);
        assert!(before.1 > 0.5 && before.1 < 1.5, "fading in: {}", before.1);
        // Back to idle halfway through the fade.
        s.set(speed, 0.0);
        assert!(
            step(&g, &mut s, &clips, &skeleton),
            "the fade's pose captured"
        );
        assert_eq!(g.state_name(&s, 0), Some("Idle"));
        assert_eq!(
            s.machines[0].from, NONE,
            "nothing fades: no stack of sources"
        );
        let after = (at(&s, 0.0).0, at(&s, 0.25).0);
        assert!(
            (after.0 - before.1).abs() < 1e-4,
            "no pop: {} after {}",
            after.0,
            before.1
        );
        let (was, is) = speeds(before, after);
        assert!(
            (is - was).abs() < 0.05 * was.abs(),
            "speed {is} after {was}"
        );
        for _ in 0..16 {
            assert_eq!(at(&s, 0.5).1, 0, "the run is not sampled again");
            step(&g, &mut s, &clips, &skeleton);
        }
        assert!(at(&s, 0.0).0.abs() < 1e-5, "at idle: {}", at(&s, 0.0).0);
    }

    /// An inertialization cut short by another: it goes on from the pose shown, its decay
    /// carried in.
    #[test]
    fn an_inertialization_cut_short_goes_on_from_the_pose_it_shows() {
        let (skeleton, clips, angles) = rig();
        let (g, _) = AnimGraph::cook(&spin_then_run(true), info(&angles), &skeleton);
        let mut s = GraphState::new(&g);
        let go = g.param("go").unwrap();
        for _ in 0..20 {
            step(&g, &mut s, &clips, &skeleton);
        }
        s.set(go, 1.0);
        for _ in 0..6 {
            step(&g, &mut s, &clips, &skeleton);
        }
        let at = |s: &GraphState, alpha| arm_at(&g, s, &clips, &skeleton, alpha, NONE).0;
        let before = (at(&s, 0.75), at(&s, 1.0));
        s.set(go, 0.0);
        assert!(step(&g, &mut s, &clips, &skeleton));
        assert_eq!(g.state_name(&s, 0), Some("Spin"));
        let layer = &s.inertia.as_ref().unwrap().layers[0];
        assert!(layer.live && layer.carry, "the running decay carried in");
        let after = (at(&s, 0.0), at(&s, 0.25));
        assert!(
            (after.0 - before.1).abs() < 1e-4,
            "{} after {}",
            after.0,
            before.1
        );
        let (was, is) = speeds(before, after);
        assert!(
            (is - was).abs() < 0.05 * was.abs().max(1.0),
            "speed {is} after {was}"
        );
        for _ in 0..17 {
            step(&g, &mut s, &clips, &skeleton);
        }
        // Spinning again from the turn's start, nothing left of either offset.
        let mut pose = Pose::identity(2);
        clips[usize::from(g.leaves[0].clip)].sample(s.phase[0], skeleton.rest(), &mut pose);
        assert!((at(&s, 1.0) - arm(&pose)).abs() < 1e-5);
    }

    /// A transition into the pose it leaves (a fade cannot play a clip at two times):
    /// inertialized, the clip starting again with no pop.
    #[test]
    fn a_transition_into_the_pose_it_leaves_starts_it_again_smoothly() {
        let (skeleton, clips, angles) = rig();
        let mut desc = spin_then_run(false);
        desc.params.push(ParamDesc {
            name: "again".into(),
            kind: GraphParamKind::Trigger,
            value: 0.0,
        });
        desc.transitions.push(TransitionDesc {
            from: Some(0),
            to: 0,
            when: "again".into(),
            blend: 0.2,
            ..TransitionDesc::default()
        });
        let (g, problems) = AnimGraph::cook(&desc, info(&angles), &skeleton);
        assert!(problems.is_empty(), "{problems:?}");
        let mut s = GraphState::new(&g);
        for _ in 0..40 {
            step(&g, &mut s, &clips, &skeleton);
        }
        let at = |s: &GraphState, alpha| arm_at(&g, s, &clips, &skeleton, alpha, NONE).0;
        let before = at(&s, 1.0);
        s.fire(g.param("again").unwrap());
        assert!(step(&g, &mut s, &clips, &skeleton), "inertialized");
        assert!(
            (at(&s, 0.0) - before).abs() < 1e-5,
            "{} after {before}",
            at(&s, 0.0)
        );
        assert!(
            s.phase[0] < 2.0 * DT,
            "the turn started again: {}",
            s.phase[0]
        );
    }

    /// Room to inertialize only where a graph may: none for transitions taken at once; the
    /// first tick takes an inertialized transition at once (nothing was shown before it);
    /// a transition never captured plays without its offset.
    #[test]
    fn graphs_inertialize_only_where_they_may() {
        let (skeleton, clips, angles) = rig();
        let mut desc = spin_then_run(true);
        for t in &mut desc.transitions {
            t.blend = 0.0;
        }
        let (g, _) = AnimGraph::cook(&desc, info(&angles), &skeleton);
        let plain = GraphState::new(&g);
        assert!(plain.inertia.is_none(), "cuts only: no room");
        let (g, _) = AnimGraph::cook(&spin_then_run(true), info(&angles), &skeleton);
        let mut s = GraphState::new(&g);
        assert!(s.inertia.is_some());
        assert!(s.size_bytes() > plain.size_bytes() + 2 * size_of::<JointDecay>());
        // The first tick: straight to the run.
        s.set(g.param("go").unwrap(), 1.0);
        assert!(!step(&g, &mut s, &clips, &skeleton), "nothing to capture");
        let at = |s: &GraphState, alpha| arm_at(&g, s, &clips, &skeleton, alpha, NONE).0;
        assert!((at(&s, 0.0) - 2.0).abs() < 1e-5);
        // Never captured: the destination at once.
        s.set(g.param("go").unwrap(), 0.0);
        g.tick(&mut s, DT);
        assert_eq!(g.state_name(&s, 0), Some("Spin"));
        assert!(at(&s, 0.0).abs() < 1e-3, "{}", at(&s, 0.0));
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
