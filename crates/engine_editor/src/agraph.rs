//! An animation graph as the editor holds it (ADR 0124 E12-5d-1): its `.agraph` file open in
//! a tab of its own on the material graph's canvas (ADR 0123): a machine's states as boxes,
//! its transitions as arrows between them, and the canvas's edits, each a file step of the
//! one history. The tab, its history, saves and autosaves are a material graph's
//! ([`GraphDoc`] holds this document in its `anim`).
//!
//! - **The canvas** shows the first layer's machine (`layers_in_order`, the first whose root
//!   is one): each state a box with its name and what its pose plays (`Walk`, `Blend1d
//!   speed: Walk · Run`, `Machine`); the entry state's title bar amber with an "Entry" tag
//!   and arrow into it; an "Any State" box, where transitions from any state start, at its
//!   machine's own place (its node's `at`, which no other canvas shows); the transitions as
//!   arrows edge to edge, two between one pair side by side, one into its own state a loop
//!   over its box.
//! - **The pointer:** a press on a state selects it (Shift adds or takes it out) and drags
//!   the selection, whole units at its end; on its edge, or with Ctrl, it draws a transition
//!   to the state it is dropped on (dropped on the empty canvas, a new state there); a click
//!   on an arrow selects it; a box on the empty canvas selects the states it meets. The
//!   middle or the right button pans, the wheel zooms, Escape takes a drag back.
//! - **Edits**, each a step: states added (the search at the pointer), moved, deleted with
//!   their transitions, connected, the entry set; the inspector's ([`AnimRow`]): a state's
//!   name, what it plays, its clip or its blend's parameters and clips, its pace; a
//!   transition's condition, compiled as it is typed, its blend, priority, exit time,
//!   offset and switches; with nothing selected, the graph's parameters. New ids are short
//!   and random (ADR 0123's sketch, `a3f9`), so two branches' new parts merge apart.
//! - **Problems** (`AnimGraphFile::problems`, conditions that do not compile) are marked on
//!   their boxes and arrows in red with why, as a material graph's errors are, and said in
//!   the bar and the inspector.

use crate::graph::{
    self, BODY, CLICK_SLOP, GraphDoc, HOVERED, Look, Outcome, PAD, PORT_REACH, PORT_TEXT, Pointer,
    RADIUS, ROW, SELECTED, TITLE, TITLE_TEXT,
};
use crate::paint::UiText;
use engine_anim::{Condition, ConditionError, MachineNow};
use engine_core::math::{Vec2, Vec4};
use engine_render2d::{Canvas, Order, Quad};
use engine_scene::{
    AnimGraphFile, AssetId, BlendPoint, LayerBlend, LayerFile, NodeFile, NodeKind, ParamFile,
    ParamKind, StateFile, TransitionFile,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// A state's box: its least width and its height (its name, then what its pose plays).
pub(crate) const STATE_WIDTH: f32 = 140.0;
pub(crate) const STATE_HEIGHT: f32 = TITLE + ROW + 6.0;
/// The Any State box.
pub(crate) const ANY_SIZE: [f32; 2] = [110.0, TITLE + 8.0];
/// What a box says its pose plays: at most this many characters.
const SUMMARY_MAX: usize = 40;
/// Between two transitions of one pair of boxes, side by side.
const LANE: f32 = 12.0;
/// An arrow's head: its length and half its width.
const HEAD: [f32; 2] = [11.0, 5.5];
/// The entry's tag left of its state, and the gap its arrow crosses.
const ENTRY_TAG: [f32; 2] = [52.0, 20.0];
const ENTRY_GAP: f32 = 36.0;
/// A transition into its own state: a loop this high over its box's top edge and this wide,
/// this far in from its right corner.
const LOOP: [f32; 3] = [18.0, 22.0, 12.0];
/// The edge band of a box where a press draws a transition, in panel pixels (at most a
/// quarter of the box's height).
const EDGE: f32 = 6.0;
/// The Entry tag's and the Any State box's colours (linear: about sRGB 40 110 60, 30 95
/// 105), and the entry state's title bar (about sRGB 120 75 20).
const ENTRY_COLOR: Vec4 = Vec4::new(0.021, 0.155, 0.045, 1.0);
const ANY_COLOR: Vec4 = Vec4::new(0.013, 0.115, 0.14, 1.0);
const ENTRY_TITLE: Vec4 = Vec4::new(0.19, 0.07, 0.007, 1.0);
/// A state's title bar (about sRGB 55 75 120), a machine's (about sRGB 85 60 120).
const STATE_TITLE: Vec4 = Vec4::new(0.038, 0.07, 0.188, 1.0);
const MACHINE_TITLE: Vec4 = Vec4::new(0.09, 0.045, 0.188, 1.0);
/// A box's edge under the pointer, where a press draws a transition (about sRGB 110 175 245).
const EDGE_COLOR: Vec4 = Vec4::new(0.155, 0.43, 0.91, 1.0);

/// What a state's pose can be, in the inspector's list (a machine is made otherwise).
pub(crate) const POSE_KINDS: [&str; 4] = ["Clip", "Blend1d", "Blend2d", "Machine"];
/// What a parameter can hold, in the inspector's lists.
pub(crate) const PARAM_KINDS: [&str; 4] = ["Float", "Bool", "Int", "Trigger"];

/// How a layer goes over the layers before it (ADR 0124's E12-5d-2b).
pub(crate) const LAYER_BLENDS: [&str; 2] = ["Normal", "Additive"];
/// A blend's clips' rows' names, by place (`clip 9` on are all `clip`).
const CLIP_ROWS: [&str; 8] = [
    "clip 1", "clip 2", "clip 3", "clip 4", "clip 5", "clip 6", "clip 7", "clip 8",
];
const AT_ROWS: [&str; 8] = [
    "at 1", "at 2", "at 3", "at 4", "at 5", "at 6", "at 7", "at 8",
];

/// Whether the file at `path` is an animation graph's (`.agraph`).
pub(crate) fn is_anim_graph(path: &str) -> bool {
    path.rsplit('.')
        .next()
        .is_some_and(|e| e.eq_ignore_ascii_case(AnimGraphFile::EXTENSION))
}

/// What a new state plays (the canvas's search).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateKind {
    /// One clip.
    Clip,
    /// Clips blended along a parameter.
    Blend1d,
    /// Clips blended over two parameters.
    Blend2d,
    /// A state machine of its own inside (ADR 0124's E12-5d-2c).
    Machine,
}

impl StateKind {
    /// Every kind, in the search's order.
    pub(crate) const ALL: [Self; 4] = [Self::Clip, Self::Blend1d, Self::Blend2d, Self::Machine];

    /// Its pose node's kind.
    fn node(self) -> NodeKind {
        match self {
            Self::Clip => NodeKind::Clip,
            Self::Blend1d => NodeKind::Blend1d,
            Self::Blend2d => NodeKind::Blend2d,
            Self::Machine => NodeKind::Machine,
        }
    }

    /// Its row in the search, and what the row says beside it.
    pub(crate) fn label(self) -> (&'static str, &'static str) {
        match self {
            Self::Clip => ("State: Clip", "plays one clip"),
            Self::Blend1d => ("State: Blend by One Parameter", "clips along a parameter"),
            Self::Blend2d => (
                "State: Blend by Two Parameters",
                "clips over two parameters",
            ),
            Self::Machine => ("State: Machine", "a state machine inside it (Tab enters)"),
        }
    }
}

/// Where a transition starts: a state, or any state of its machine (the Any State box).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// Any state of the machine.
    Any,
    /// The state of this id.
    State(String),
}

impl Source {
    /// Its transition's `from`: the state's id, empty for any.
    fn id(&self) -> &str {
        match self {
            Self::Any => "",
            Self::State(id) => id,
        }
    }
}

/// A state of the machine shown, as the canvas draws it, in canvas units.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StateBox {
    /// Its state's id.
    pub(crate) id: String,
    /// Its name (its id while it has none).
    pub(crate) title: String,
    /// What its pose plays.
    pub(crate) summary: String,
    /// Its top left, and its width and height.
    pub(crate) at: [f32; 2],
    pub(crate) size: [f32; 2],
    /// It is its machine's entry.
    pub(crate) entry: bool,
    /// Its pose is a machine.
    pub(crate) machine: bool,
}

impl StateBox {
    /// Its corners.
    pub(crate) fn rect(&self) -> [[f32; 2]; 2] {
        [
            self.at,
            [self.at[0] + self.size[0], self.at[1] + self.size[1]],
        ]
    }
}

/// A transition as the canvas draws it: from where it leaves its box to where it arrives
/// (edge to edge), or a loop over its own state's box.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Arrow {
    /// Its transition's id.
    pub(crate) id: String,
    /// Where it leaves and where its head is.
    pub(crate) from: [f32; 2],
    pub(crate) to: [f32; 2],
    /// Into its own state: a loop from `from` up and over to `to`.
    pub(crate) looped: bool,
}

impl Arrow {
    /// A loop's corners: up from `from`, across, down to `to`.
    fn loop_points(&self) -> [[f32; 2]; 4] {
        let (a, d) = (self.from, self.to);
        [a, [a[0], a[1] - LOOP[0]], [d[0], d[1] - LOOP[0]], d]
    }

    /// How far canvas point `p` is from it.
    pub(crate) fn distance(&self, p: [f32; 2]) -> f32 {
        if self.looped {
            let [a, b, c, d] = self.loop_points();
            graph::segment_distance(p, a, b)
                .min(graph::segment_distance(p, b, c))
                .min(graph::segment_distance(p, c, d))
        } else {
            graph::segment_distance(p, self.from, self.to)
        }
    }

    /// Its middle, where its label goes.
    pub(crate) fn middle(&self) -> [f32; 2] {
        if self.looped {
            let [_, b, c, _] = self.loop_points();
            [(b[0] + c[0]) * 0.5, b[1]]
        } else {
            [
                (self.from[0] + self.to[0]) * 0.5,
                (self.from[1] + self.to[1]) * 0.5,
            ]
        }
    }
}

/// What a problem is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum On {
    /// A state (its own problem, or its pose's).
    State(String),
    /// A transition.
    Transition(String),
    /// The graph: what it is (`the machine`, `layer Base`).
    Graph(String),
}

/// A problem the canvas marks, and why it is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mark {
    /// What it is on.
    pub(crate) on: On,
    /// Why.
    pub(crate) why: String,
    /// It is a condition that does not compile.
    pub(crate) condition: bool,
}

/// What lies under a canvas point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    /// The empty canvas.
    Canvas,
    /// Box `k`'s inside: a press selects and moves.
    State(usize),
    /// Box `k`'s edge: a press draws a transition from it.
    Edge(usize),
    /// The Any State box's inside, and its edge.
    Any,
    AnyEdge,
    /// Arrow `k`.
    Arrow(usize),
}

/// A drag on the canvas under way.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Drag {
    /// The view moved by the middle or the right button.
    Pan,
    /// The selection moved.
    Move {
        /// The canvas point pressed.
        from: [f32; 2],
        /// Each selected state's place then.
        starts: Vec<(String, [f32; 2])>,
        /// The Any State box's place then, where it is selected.
        any: Option<[f32; 2]>,
        /// The pointer has moved past [`CLICK_SLOP`].
        moved: bool,
    },
    /// A transition drawn from a state or from the Any State box.
    Link {
        /// Where it starts.
        from: Source,
    },
    /// A box drawn from canvas point `from`; Shift adds what it meets to the selection.
    Select {
        /// Where it started.
        from: [f32; 2],
        /// It adds to the selection.
        add: bool,
    },
}

/// A row of the inspector while an animation graph's tab is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnimRow {
    /// The state selected alone: its name, whether its machine starts at it, what its pose
    /// is, a clip's name, its pace.
    Name,
    Entry,
    Pose,
    Clip,
    Speed,
    /// A blend's parameter (across for a 2D blend) and the one up its points.
    BlendParam,
    BlendParamY,
    /// A blend's clip `k` and where it lies (emptied, it goes); a clip's name typed adds one.
    PointClip(usize),
    PointAt(usize),
    AddPoint,
    /// What does not hold together on the state or the transition selected (a condition
    /// that does not compile, at its place).
    Problem,
    /// The transition selected alone: its condition, its blend's seconds, its priority, its
    /// exit time and offset (fractions of a clip), its two switches.
    When,
    Blend,
    Priority,
    Exit,
    Offset,
    KeepPhase,
    Inertialize,
    /// With nothing selected, the layer shown (ADR 0124's E12-5d-2b): its name, how it goes
    /// over the layers before it, its weight, and the parameter that sets it instead.
    LayerName,
    LayerBlend,
    LayerWeight,
    LayerWeightParam,
    /// The joint picked in the shown layer's mask (ADR 0124's E12-5d-2b): its weight, its
    /// own or what it inherits; ↺ gives it back to what it inherits.
    MaskWeight,
    /// With nothing selected, parameter `k` (in their names' order): its name, kind and
    /// starting value.
    ParamName(usize),
    ParamKind(usize),
    ParamValue(usize),
    /// A parameter of a kind added.
    AddParam,
}

impl AnimRow {
    /// Its name in the inspector.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Name | Self::ParamName(_) | Self::LayerName => "name",
            Self::LayerBlend => "blend",
            Self::LayerWeight => "weight",
            Self::LayerWeightParam => "weight from",
            Self::MaskWeight => "joint weight",
            Self::Entry => "entry",
            Self::Pose => "plays",
            Self::Clip => "clip",
            Self::Speed => "speed",
            Self::BlendParam => "parameter",
            Self::BlendParamY => "parameter y",
            Self::PointClip(k) => CLIP_ROWS.get(k).copied().unwrap_or("clip"),
            Self::PointAt(k) => AT_ROWS.get(k).copied().unwrap_or("at"),
            Self::AddPoint => "add clip",
            Self::Problem => "\u{26a0}",
            Self::When => "when",
            Self::Blend => "blend",
            Self::Priority => "priority",
            Self::Exit => "exit time",
            Self::Offset => "offset",
            Self::KeepPhase => "keep phase",
            Self::Inertialize => "inertialize",
            Self::ParamKind(_) => "kind",
            Self::ParamValue(_) => "starts at",
            Self::AddParam => "add",
        }
    }
}

/// How a row shows in the inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowLook {
    /// Text typed (a name, a clip, a condition).
    Text,
    /// Numbers dragged or typed: how many, and whether whole.
    Numbers(u8, bool),
    /// A switch, and whether it is on.
    Check(bool),
    /// One of a list ([`AnimDoc::options`]).
    Choice,
    /// What does not hold together, in the error colour.
    Problem,
}

/// A section of the inspector while an animation graph's tab is shown, with its rows' range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnimSection {
    /// The state or the transition selected alone (titled by [`AnimDoc::title`]), or several
    /// selected (no rows).
    Selected,
    /// With nothing selected, the layer shown (ADR 0124's E12-5d-2b), titled by
    /// [`AnimDoc::layer_title`]: its × deletes it, its ↑ and ↓ move it among the layers.
    Layer,
    /// Its mask, after it (E12-5d-2b, the team's а): the skeleton's joints with their check
    /// boxes under its header, the picked joint's weight its row.
    Mask,
    /// Parameter `k`, titled by its name; its × deletes it.
    Param(usize),
    /// Adding a parameter.
    Add,
}

/// The watched character's machine in Play (ADR 0124's E12-5e), as of the editor's last
/// frame: the canvas lights its state and the one it fades from. Off while nothing plays the
/// graph; its texts keep their room.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Live {
    pub(crate) on: bool,
    /// The watched character's name.
    pub(crate) who: String,
    /// The state it is in, and the one it fades from (empty: none), by name.
    pub(crate) state: String,
    pub(crate) from: String,
    /// How far into the transition under way, 0 to 1 (1 with none), and how far the
    /// state's pose has played, 0 to 1.
    pub(crate) faded: f32,
    pub(crate) progress: f32,
    /// What it does, in a line ([`write_now`]).
    pub(crate) line: String,
}

impl Live {
    /// Watching `who`, whose machine is as `now` says.
    pub(crate) fn set(&mut self, who: &str, now: &MachineNow<'_>) {
        self.on = true;
        self.who.clear();
        self.who.push_str(who);
        self.state.clear();
        self.state.push_str(now.state);
        self.from.clear();
        if let Some(from) = now.from {
            self.from.push_str(from);
        }
        self.faded = now.faded;
        self.progress = now.progress;
        write_now(&mut self.line, now);
    }
}

/// What a running machine does, in a line (ADR 0124's E12-5e): `Run · 0.42 s`, then
/// `· from Walk 30 %` through a cross-fade, `· inertializing 30 %` through an
/// inertialization.
pub(crate) fn write_now(out: &mut String, now: &MachineNow<'_>) {
    out.clear();
    let _ = write!(out, "{} \u{b7} {:.2} s", now.state, now.seconds);
    if let Some(from) = now.from {
        let _ = write!(out, " \u{b7} from {from} {:.0} %", now.faded * 100.0);
    } else if now.inertial && now.faded < 1.0 {
        let _ = write!(out, " \u{b7} inertializing {:.0} %", now.faded * 100.0);
    }
}

/// An animation graph's file as its tab holds it, and what its canvas shows.
#[derive(Debug)]
pub(crate) struct AnimDoc {
    /// The file.
    pub(crate) file: AnimGraphFile,
    /// The layer shown (ADR 0124's E12-5d-2b), by id: the one chosen on the bar, else the
    /// first playing a machine (empty without layers); and the layers' ids in their order.
    pub(crate) layer: String,
    pub(crate) layer_ids: Vec<String>,
    /// The skeleton masks are edited over (E12-5d-2b): a character's playing the graph, its
    /// joints' names, parents and depths in its order (parents first); empty until one is
    /// found. And the joint picked in the shown layer's mask, by name.
    /// The machines entered from the shown layer's (ADR 0124's E12-5d-2c), by node id, each
    /// the pose of a state of the one before: the canvas shows the last.
    pub(crate) path: Vec<String>,
    pub(crate) joints: Vec<String>,
    pub(crate) joint_parents: Vec<Option<usize>>,
    pub(crate) joint_depths: Vec<u16>,
    pub(crate) mask_joint: String,
    /// The machine the canvas shows, by id (empty without one): the shown layer's root,
    /// where it is one.
    pub(crate) machine: String,
    /// The selected states and transitions, by id, and whether the Any State box is.
    pub(crate) selected: BTreeSet<String>,
    pub(crate) transitions: BTreeSet<String>,
    pub(crate) any: bool,
    /// The machine's states' boxes as last laid out, in their ids' order, and its
    /// transitions' arrows.
    pub(crate) boxes: Vec<StateBox>,
    pub(crate) arrows: Vec<Arrow>,
    /// What does not hold together, as of the file's last change.
    pub(crate) marks: Vec<Mark>,
    /// What the pointer is over.
    pub(crate) hover: Hit,
    /// The revision its boxes were laid out at.
    laid_out: Option<u64>,
    /// The revision last given to the player, which plays it in place of the file (ADR
    /// 0124's E12-5e).
    pub(crate) given: Option<u64>,
    /// In Play, the watched character's machine.
    pub(crate) live: Live,
}

impl AnimDoc {
    /// The document of `file`, nothing selected.
    pub(crate) fn new(file: AnimGraphFile) -> Self {
        let mut doc = Self {
            file,
            layer: String::new(),
            layer_ids: Vec::new(),
            path: Vec::new(),
            joints: Vec::new(),
            joint_parents: Vec::new(),
            joint_depths: Vec::new(),
            mask_joint: String::new(),
            machine: String::new(),
            selected: BTreeSet::new(),
            transitions: BTreeSet::new(),
            any: false,
            boxes: Vec::new(),
            arrows: Vec::new(),
            marks: Vec::new(),
            hover: Hit::Canvas,
            laid_out: None,
            given: None,
            live: Live::default(),
        };
        doc.refresh();
        doc
    }

    /// The machine shown and what does not hold together found again (the file changed):
    /// the file's problems each on the state or the transition it is about (a pose's on
    /// each state showing it), the others on the graph; then each condition that does not
    /// compile.
    pub(crate) fn refresh(&mut self) {
        self.layer_ids.clear();
        self.layer_ids.extend(
            self.file
                .layers_in_order()
                .iter()
                .map(|(id, _)| (*id).to_owned()),
        );
        let layer = shown_layer(&self.file, &self.layer).unwrap_or_default();
        self.layer = layer;
        let root = shown_machine(&self.file, &self.layer);
        // The machines entered, while each is still the pose of a state of the one before.
        let mut keep = 0;
        for (k, m) in self.path.iter().enumerate() {
            let parent = if k == 0 { &root } else { &self.path[k - 1] };
            let held = is_machine(&self.file, m)
                && self
                    .file
                    .states
                    .values()
                    .any(|s| s.machine == *parent && s.pose == *m);
            if !held {
                break;
            }
            keep = k + 1;
        }
        self.path.truncate(keep);
        self.machine = self.path.last().cloned().unwrap_or(root);
        self.marks.clear();
        for (what, why) in self.file.problems() {
            let on = match what.split_once(' ') {
                Some(("state", id)) => On::State(id.to_owned()),
                Some(("transition", id)) => On::Transition(id.to_owned()),
                Some(("node", id)) => {
                    let mut shown = false;
                    for (state, s) in &self.file.states {
                        if s.pose == id {
                            self.marks.push(Mark {
                                on: On::State(state.clone()),
                                why: why.clone(),
                                condition: false,
                            });
                            shown = true;
                        }
                    }
                    if shown {
                        continue;
                    }
                    if id == self.machine {
                        On::Graph(String::from("the machine"))
                    } else {
                        On::Graph(what.clone())
                    }
                }
                Some(("layer", id)) => {
                    let name = self
                        .file
                        .layers
                        .get(id)
                        .filter(|l| !l.name.is_empty())
                        .map_or(id, |l| l.name.as_str());
                    On::Graph(format!("layer {name}"))
                }
                _ => On::Graph(what.clone()),
            };
            self.marks.push(Mark {
                on,
                why,
                condition: false,
            });
        }
        for (id, t) in &self.file.transitions {
            if let Err(e) = compile(&self.file, &t.when) {
                self.marks.push(Mark {
                    on: On::Transition(id.clone()),
                    why: condition_why(&t.when, &e),
                    condition: true,
                });
            }
        }
    }

    /// Its file put back (undo, redo): what is selected and gone let go.
    pub(crate) fn put(&mut self, file: AnimGraphFile) {
        self.file = file;
        self.refresh();
        let (states, transitions, machine) =
            (&self.file.states, &self.file.transitions, &self.machine);
        self.selected
            .retain(|id| states.get(id).is_some_and(|s| s.machine == *machine));
        self.transitions.retain(|id| transitions.contains_key(id));
        self.any &= !self.machine.is_empty();
        self.laid_out = None;
    }

    /// Its boxes and arrows laid out again where the file changed since `revision`
    /// (`measure`: a text's width at a size, at zoom 1).
    pub(crate) fn relayout(&mut self, revision: u64, measure: &mut dyn FnMut(&str, f32) -> f32) {
        if self.laid_out == Some(revision) {
            return;
        }
        self.laid_out = Some(revision);
        self.boxes.clear();
        if !self.machine.is_empty() {
            let entry = self
                .file
                .nodes
                .get(&self.machine)
                .map_or("", |n| n.entry.as_str());
            for (id, state) in self.file.states_of(&self.machine) {
                let title = if state.name.is_empty() {
                    id.to_owned()
                } else {
                    state.name.clone()
                };
                let mut summary = String::new();
                pose_summary(&self.file, &state.pose, &mut summary);
                let width = (measure(&title, TITLE_TEXT).max(measure(&summary, PORT_TEXT))
                    + 2.0 * PAD)
                    .max(STATE_WIDTH)
                    .ceil();
                let machine = self
                    .file
                    .nodes
                    .get(&state.pose)
                    .is_some_and(|n| n.kind == NodeKind::Machine);
                self.boxes.push(StateBox {
                    id: id.to_owned(),
                    title,
                    summary,
                    at: state.at,
                    size: [width, STATE_HEIGHT],
                    entry: id == entry,
                    machine,
                });
            }
        }
        self.layout_arrows();
    }

    /// The transitions' arrows between the boxes laid out: each one whose states the canvas
    /// shows (from the Any State box without `from`), edge to edge, its lane beside the
    /// others between the same two boxes.
    fn layout_arrows(&mut self) {
        self.arrows.clear();
        let any = self.any_rect();
        let rect_of = |id: &str| {
            let k = self
                .boxes
                .binary_search_by(|b| b.id.as_str().cmp(id))
                .ok()?;
            Some(self.boxes[k].rect())
        };
        for (id, t) in &self.file.transitions {
            let Some(to) = rect_of(&t.to) else {
                continue;
            };
            let from = if t.from.is_empty() {
                any
            } else {
                rect_of(&t.from)
            };
            let Some(from) = from else {
                continue;
            };
            if t.from == t.to {
                // Into its own state: a loop over its box's top right corner.
                let (x, y) = (to[1][0] - LOOP[2], to[0][1]);
                self.arrows.push(Arrow {
                    id: id.clone(),
                    from: [x - LOOP[1], y],
                    to: [x, y],
                    looped: true,
                });
                continue;
            }
            // Its lane: those between the same two side by side, centred while they all
            // go one way, each to its right where some come back.
            let same = |u: &TransitionFile| u.from == t.from && u.to == t.to;
            let back = !t.from.is_empty()
                && self
                    .file
                    .transitions
                    .values()
                    .any(|u| u.from == t.to && u.to == t.from);
            let lane = self
                .file
                .transitions
                .iter()
                .take_while(|(k, _)| *k != id)
                .filter(|(_, u)| same(u))
                .count() as f32;
            let n = self.file.transitions.values().filter(|u| same(u)).count() as f32;
            let offset = if back {
                (lane + 0.5) * LANE
            } else {
                (lane - (n - 1.0) * 0.5) * LANE
            };
            if let Some((a, b)) = edge_points(from, to, offset) {
                self.arrows.push(Arrow {
                    id: id.clone(),
                    from: a,
                    to: b,
                    looped: false,
                });
            }
        }
    }

    /// The Any State box's corners, where a machine shows: at its machine's own place.
    pub(crate) fn any_rect(&self) -> Option<[[f32; 2]; 2]> {
        let at = self.file.nodes.get(&self.machine)?.at;
        Some([at, [at[0] + ANY_SIZE[0], at[1] + ANY_SIZE[1]]])
    }

    /// State `id`'s box.
    pub(crate) fn box_of(&self, id: &str) -> Option<usize> {
        self.boxes.binary_search_by(|b| b.id.as_str().cmp(id)).ok()
    }

    /// Whether box `k` is selected.
    pub(crate) fn is_selected(&self, k: usize) -> bool {
        self.boxes
            .get(k)
            .is_some_and(|b| self.selected.contains(&b.id))
    }

    /// The boxes in their drawing order: the unselected, then the selected (on top).
    pub(crate) fn order(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        let n = self.boxes.len();
        (0..n)
            .filter(|&k| !self.is_selected(k))
            .chain((0..n).filter(|&k| self.is_selected(k)))
    }

    /// Nothing selected.
    pub(crate) fn clear_selection(&mut self) {
        self.selected.clear();
        self.transitions.clear();
        self.any = false;
    }

    /// What lies under canvas point `p`: the top box first (its edge within `reach`, at
    /// most a quarter of its height, before its inside), the Any State box, then the
    /// nearest arrow within `reach`.
    pub(crate) fn hit(&self, p: [f32; 2], reach: f32) -> Hit {
        for k in self.order().rev() {
            match edge_or_inside(self.boxes[k].rect(), p, reach) {
                Some(true) => return Hit::Edge(k),
                Some(false) => return Hit::State(k),
                None => {}
            }
        }
        if let Some(r) = self.any_rect() {
            match edge_or_inside(r, p, reach) {
                Some(true) => return Hit::AnyEdge,
                Some(false) => return Hit::Any,
                None => {}
            }
        }
        let mut best: Option<(usize, f32)> = None;
        for (k, a) in self.arrows.iter().enumerate() {
            let d = a.distance(p);
            if d <= reach && best.is_none_or(|(_, b)| d < b) {
                best = Some((k, d));
            }
        }
        best.map_or(Hit::Canvas, |(k, _)| Hit::Arrow(k))
    }

    /// The canvas rectangle around the selected states' boxes, or around every box and the
    /// Any State box with none selected.
    pub(crate) fn extent(&self) -> Option<[[f32; 2]; 2]> {
        let any = self.selected.iter().any(|id| self.box_of(id).is_some());
        let mut area: Option<[[f32; 2]; 2]> = None;
        let mut add = |[lo, hi]: [[f32; 2]; 2]| {
            area = Some(match area {
                None => [lo, hi],
                Some([a, z]) => [
                    [a[0].min(lo[0]), a[1].min(lo[1])],
                    [z[0].max(hi[0]), z[1].max(hi[1])],
                ],
            });
        };
        for (k, b) in self.boxes.iter().enumerate() {
            if !any || self.is_selected(k) {
                add(b.rect());
            }
        }
        if !any && let Some(r) = self.any_rect() {
            add(r);
        }
        area
    }

    /// The state the inspector shows: the one selected alone, no transition with it.
    pub(crate) fn inspected_state(&self) -> Option<&str> {
        if !self.transitions.is_empty() {
            return None;
        }
        let mut ids = self.selected.iter();
        match (ids.next(), ids.next()) {
            (Some(id), None) if self.file.states.contains_key(id) => Some(id),
            _ => None,
        }
    }

    /// The transition the inspector shows: the one selected alone, no state with it.
    pub(crate) fn inspected_transition(&self) -> Option<&str> {
        if !self.selected.is_empty() {
            return None;
        }
        let mut ids = self.transitions.iter();
        match (ids.next(), ids.next()) {
            (Some(id), None) if self.file.transitions.contains_key(id) => Some(id),
            _ => None,
        }
    }

    /// The first problem on state `id`.
    pub(crate) fn state_mark(&self, id: &str) -> Option<&str> {
        self.marks
            .iter()
            .find(|m| matches!(&m.on, On::State(s) if s == id))
            .map(|m| m.why.as_str())
    }

    /// The first problem on transition `id`: its condition's, else the file's.
    pub(crate) fn transition_mark(&self, id: &str) -> Option<&str> {
        let on = |m: &&Mark| matches!(&m.on, On::Transition(t) if t == id);
        self.marks
            .iter()
            .filter(on)
            .find(|m| m.condition)
            .or_else(|| self.marks.iter().find(on))
            .map(|m| m.why.as_str())
    }

    /// The press at canvas point `c` (`reach`: a port's reach in canvas units): the drag it
    /// starts, the selection it makes.
    fn press(&mut self, c: [f32; 2], reach: f32, shift: bool, ctrl: bool) -> Option<Drag> {
        match self.hit(c, reach) {
            Hit::Edge(k) => Some(Drag::Link {
                from: Source::State(self.boxes[k].id.clone()),
            }),
            Hit::State(k) if ctrl => Some(Drag::Link {
                from: Source::State(self.boxes[k].id.clone()),
            }),
            Hit::AnyEdge => Some(Drag::Link { from: Source::Any }),
            Hit::Any if ctrl => Some(Drag::Link { from: Source::Any }),
            Hit::State(k) => {
                let id = self.boxes[k].id.clone();
                if shift {
                    if !self.selected.remove(&id) {
                        self.selected.insert(id);
                    }
                } else if !self.selected.contains(&id) {
                    self.clear_selection();
                    self.selected.insert(id);
                }
                Some(self.move_drag(c))
            }
            Hit::Any => {
                if shift {
                    self.any = !self.any;
                } else if !self.any {
                    self.clear_selection();
                    self.any = true;
                }
                Some(self.move_drag(c))
            }
            Hit::Arrow(k) => {
                let id = self.arrows[k].id.clone();
                if shift {
                    if !self.transitions.remove(&id) {
                        self.transitions.insert(id);
                    }
                } else {
                    self.clear_selection();
                    self.transitions.insert(id);
                }
                None
            }
            Hit::Canvas => Some(Drag::Select {
                from: c,
                add: shift,
            }),
        }
    }

    /// A move of the selection from canvas point `c`.
    fn move_drag(&self, c: [f32; 2]) -> Drag {
        let starts = self
            .selected
            .iter()
            .filter_map(|id| Some((id.clone(), self.file.states.get(id)?.at)))
            .collect();
        let any = if self.any {
            self.file.nodes.get(&self.machine).map(|m| m.at)
        } else {
            None
        };
        Drag::Move {
            from: c,
            starts,
            any,
            moved: false,
        }
    }

    /// The inspector's rows into `rows` and its sections with their rows' ranges into
    /// `sections` (their rooms kept): the state's or the transition's selected alone, none
    /// for several, the parameters with nothing selected.
    pub(crate) fn rows(
        &self,
        rows: &mut Vec<AnimRow>,
        sections: &mut Vec<(AnimSection, usize, usize)>,
    ) {
        rows.clear();
        sections.clear();
        if let Some(id) = self.inspected_state() {
            rows.extend([AnimRow::Name, AnimRow::Entry, AnimRow::Pose]);
            let pose = self
                .file
                .states
                .get(id)
                .and_then(|s| self.file.nodes.get(&s.pose));
            match pose.map(|n| (n.kind, n.points.len())) {
                Some((NodeKind::Clip, _)) => rows.push(AnimRow::Clip),
                Some((kind @ (NodeKind::Blend1d | NodeKind::Blend2d), points)) => {
                    rows.push(AnimRow::BlendParam);
                    if kind == NodeKind::Blend2d {
                        rows.push(AnimRow::BlendParamY);
                    }
                    for k in 0..points {
                        rows.push(AnimRow::PointClip(k));
                        rows.push(AnimRow::PointAt(k));
                    }
                    rows.push(AnimRow::AddPoint);
                }
                Some((NodeKind::Machine, _)) | None => {}
            }
            if pose.is_some() {
                rows.push(AnimRow::Speed);
            }
            if self.state_mark(id).is_some() {
                rows.push(AnimRow::Problem);
            }
            sections.push((AnimSection::Selected, 0, rows.len()));
            return;
        }
        if let Some(id) = self.inspected_transition() {
            rows.push(AnimRow::When);
            if self.transition_mark(id).is_some() {
                rows.push(AnimRow::Problem);
            }
            rows.extend([
                AnimRow::Blend,
                AnimRow::Priority,
                AnimRow::Exit,
                AnimRow::Offset,
                AnimRow::KeepPhase,
                AnimRow::Inertialize,
            ]);
            sections.push((AnimSection::Selected, 0, rows.len()));
            return;
        }
        if !self.selected.is_empty() || !self.transitions.is_empty() {
            sections.push((AnimSection::Selected, 0, 0));
            return;
        }
        if self.file.layers.contains_key(&self.layer) {
            rows.extend([
                AnimRow::LayerName,
                AnimRow::LayerBlend,
                AnimRow::LayerWeight,
                AnimRow::LayerWeightParam,
            ]);
            sections.push((AnimSection::Layer, 0, rows.len()));
            let from = rows.len();
            if self.mask_joint().is_some() {
                rows.push(AnimRow::MaskWeight);
            }
            sections.push((AnimSection::Mask, from, rows.len()));
        }
        for (k, p) in self.file.params.values().enumerate() {
            let from = rows.len();
            rows.extend([AnimRow::ParamName(k), AnimRow::ParamKind(k)]);
            if p.kind != ParamKind::Trigger {
                rows.push(AnimRow::ParamValue(k));
            }
            sections.push((AnimSection::Param(k), from, rows.len()));
        }
        let from = rows.len();
        rows.push(AnimRow::AddParam);
        sections.push((AnimSection::Add, from, rows.len()));
    }

    /// The layer shown (ADR 0124's E12-5d-2b).
    pub(crate) fn shown_layer(&self) -> Option<&LayerFile> {
        self.file.layers.get(&self.layer)
    }

    /// The names of the states whose machines are entered, outermost first (the bar's crumbs,
    /// ADR 0124's E12-5d-2c), into `out` (its room kept).
    pub(crate) fn crumbs<'a>(&'a self, out: &mut Vec<&'a str>) {
        out.clear();
        for (k, m) in self.path.iter().enumerate() {
            let parent = if k == 0 {
                self.file
                    .layers
                    .get(&self.layer)
                    .map_or("", |l| l.root.as_str())
            } else {
                self.path[k - 1].as_str()
            };
            let name = self
                .file
                .states
                .values()
                .find(|s| s.machine == parent && s.pose == *m)
                .map_or("", |s| s.name.as_str());
            out.push(name);
        }
    }

    /// The shown machine's node's place among the file's nodes (in their ids' order, as the
    /// player's graph has them; ADR 0124's E12-5d-2c), for its live state.
    pub(crate) fn machine_node_index(&self) -> Option<usize> {
        if self.machine.is_empty() {
            return None;
        }
        self.file.nodes.keys().position(|k| *k == self.machine)
    }

    /// The shown layer's place among the layers in their order (the player's layer index).
    pub(crate) fn layer_index(&self) -> Option<usize> {
        self.layer_ids.iter().position(|id| *id == self.layer)
    }

    /// The skeleton masks are edited over (a character's playing the graph): its joints'
    /// names and parents in its order, their depths worked out.
    pub(crate) fn set_joints(
        &mut self,
        names: &[String],
        parents: impl Iterator<Item = Option<usize>>,
    ) {
        self.joints.clear();
        self.joints.extend(names.iter().cloned());
        self.joint_parents.clear();
        self.joint_parents.extend(parents.take(names.len()));
        self.joint_parents.resize(names.len(), None);
        self.joint_depths.clear();
        for j in 0..names.len() {
            let depth = self.joint_parents[j]
                .and_then(|p| self.joint_depths.get(p).copied())
                .map_or(0, |d| d.saturating_add(1));
            self.joint_depths.push(depth);
        }
    }

    /// The joint picked in the shown layer's mask, by its place in the skeleton.
    pub(crate) fn mask_joint(&self) -> Option<usize> {
        if self.mask_joint.is_empty() || self.shown_layer().is_none() {
            return None;
        }
        self.joints.iter().position(|n| *n == self.mask_joint)
    }

    /// Joint `j`'s weight in the shown layer's mask (as the cook binds it): its own, else its
    /// nearest ancestor's, else 1 for an empty mask and 0 for another.
    pub(crate) fn mask_weight(&self, j: usize) -> f32 {
        self.shown_layer().map_or(1.0, |l| {
            mask_weight_of(&l.mask, &self.joints, &self.joint_parents, j)
        })
    }

    /// The mask's section's title into `out`: `Mask · all joints`, `Mask · 3 set`, or what
    /// it needs while no skeleton is known.
    pub(crate) fn mask_title(&self, out: &mut String) {
        out.clear();
        let Some(l) = self.shown_layer() else {
            return;
        };
        let _ = match (l.mask.len(), self.joints.is_empty()) {
            (0, _) => write!(out, "Mask \u{b7} all joints"),
            (n, false) => write!(out, "Mask \u{b7} {n} set"),
            (n, true) => write!(
                out,
                "Mask \u{b7} {n} set (a character playing the graph shows its joints)"
            ),
        };
    }

    /// The shown layer's section's title into `out`: `Layer · Base`.
    pub(crate) fn layer_title(&self, out: &mut String) {
        out.clear();
        if let Some(l) = self.shown_layer() {
            let _ = write!(out, "Layer \u{b7} {}", l.name);
        }
    }

    /// The inspected state's pose when it is a blend (ADR 0124's E12-5d-2a): what the
    /// inspector's blend panel draws.
    pub(crate) fn inspected_blend(&self) -> Option<&NodeFile> {
        self.pose()
            .filter(|n| matches!(n.kind, NodeKind::Blend1d | NodeKind::Blend2d))
    }

    /// The inspected state's pose node.
    fn pose(&self) -> Option<&NodeFile> {
        let state = self.file.states.get(self.inspected_state()?)?;
        self.file.nodes.get(&state.pose)
    }

    /// The inspected transition.
    fn transition(&self) -> Option<&TransitionFile> {
        self.file.transitions.get(self.inspected_transition()?)
    }

    /// Parameter `k` (in their names' order) and its name.
    pub(crate) fn param(&self, k: usize) -> Option<(&str, &ParamFile)> {
        self.file
            .params
            .iter()
            .nth(k)
            .map(|(name, p)| (name.as_str(), p))
    }

    /// How row `row` shows.
    pub(crate) fn look(&self, row: AnimRow) -> RowLook {
        match row {
            AnimRow::Name
            | AnimRow::Clip
            | AnimRow::PointClip(_)
            | AnimRow::AddPoint
            | AnimRow::When
            | AnimRow::LayerName
            | AnimRow::ParamName(_) => RowLook::Text,
            AnimRow::Entry => RowLook::Check(self.inspected_state().is_some_and(|id| {
                let machine = self.file.states.get(id).map_or("", |s| s.machine.as_str());
                self.file.nodes.get(machine).is_some_and(|m| m.entry == id)
            })),
            AnimRow::KeepPhase => RowLook::Check(self.transition().is_some_and(|t| t.keep_phase)),
            AnimRow::Inertialize => {
                RowLook::Check(self.transition().is_some_and(|t| t.inertialize))
            }
            AnimRow::Pose
            | AnimRow::BlendParam
            | AnimRow::BlendParamY
            | AnimRow::LayerBlend
            | AnimRow::LayerWeightParam
            | AnimRow::ParamKind(_)
            | AnimRow::AddParam => RowLook::Choice,
            AnimRow::Problem => RowLook::Problem,
            AnimRow::Speed
            | AnimRow::Blend
            | AnimRow::Exit
            | AnimRow::Offset
            | AnimRow::LayerWeight
            | AnimRow::MaskWeight => RowLook::Numbers(1, false),
            AnimRow::Priority => RowLook::Numbers(1, true),
            AnimRow::PointAt(_) => {
                let wide = self.pose().is_some_and(|n| n.kind == NodeKind::Blend2d);
                RowLook::Numbers(if wide { 2 } else { 1 }, false)
            }
            AnimRow::ParamValue(k) => match self.param(k).map(|(_, p)| p.kind) {
                Some(ParamKind::Bool) => {
                    RowLook::Check(self.param(k).is_some_and(|(_, p)| p.value != 0.0))
                }
                Some(ParamKind::Int) => RowLook::Numbers(1, true),
                _ => RowLook::Numbers(1, false),
            },
        }
    }

    /// Row `row`'s choices into `out`: what a pose can be, what a parameter can hold, the
    /// parameters a blend can follow (numbers and whole numbers).
    pub(crate) fn options<'a>(&'a self, row: AnimRow, out: &mut Vec<&'a str>) {
        out.clear();
        match row {
            AnimRow::Pose => out.extend(POSE_KINDS),
            AnimRow::ParamKind(_) | AnimRow::AddParam => out.extend(PARAM_KINDS),
            AnimRow::BlendParam | AnimRow::BlendParamY => out.extend(
                self.file
                    .params
                    .iter()
                    .filter(|(_, p)| matches!(p.kind, ParamKind::Float | ParamKind::Int))
                    .map(|(name, _)| name.as_str()),
            ),
            AnimRow::LayerBlend => out.extend(LAYER_BLENDS),
            // None first: the layer's own weight.
            AnimRow::LayerWeightParam => {
                out.push("none");
                out.extend(
                    self.file
                        .params
                        .iter()
                        .filter(|(_, p)| matches!(p.kind, ParamKind::Float | ParamKind::Int))
                        .map(|(name, _)| name.as_str()),
                );
            }
            _ => {}
        }
    }

    /// Row `row`'s numbers: a pace, a blend's seconds, a priority, an exit time or offset, a
    /// clip's place in its blend, a parameter's starting value.
    pub(crate) fn numbers(&self, row: AnimRow) -> [f32; 4] {
        let one = |v: f32| [v, 0.0, 0.0, 0.0];
        match row {
            AnimRow::Speed => one(self.pose().map_or(1.0, |n| n.speed)),
            AnimRow::PointAt(k) => self
                .pose()
                .and_then(|n| n.points.get(k))
                .map_or([0.0; 4], |p| [p.at[0], p.at[1], 0.0, 0.0]),
            AnimRow::Blend => one(self.transition().map_or(0.25, |t| t.blend)),
            AnimRow::Priority => one(self.transition().map_or(0.0, |t| t.priority as f32)),
            AnimRow::Exit => one(self.transition().map_or(0.0, |t| t.exit)),
            AnimRow::Offset => one(self.transition().map_or(0.0, |t| t.offset)),
            AnimRow::ParamValue(k) => one(self.param(k).map_or(0.0, |(_, p)| p.value)),
            AnimRow::LayerWeight => one(self.shown_layer().map_or(1.0, |l| l.weight)),
            AnimRow::MaskWeight => one(self.mask_joint().map_or(1.0, |j| self.mask_weight(j))),
            _ => [0.0; 4],
        }
    }

    /// Whether row `row` holds other than its default: its reset button.
    pub(crate) fn changed(&self, row: AnimRow) -> bool {
        let pose = self.pose();
        let t = self.transition();
        match row {
            AnimRow::Clip => pose.is_some_and(|n| !n.clip.is_empty()),
            AnimRow::Speed => pose.is_some_and(|n| n.speed != 1.0),
            AnimRow::BlendParam => pose.is_some_and(|n| !n.param.is_empty()),
            AnimRow::BlendParamY => pose.is_some_and(|n| !n.param_y.is_empty()),
            AnimRow::PointAt(k) => pose
                .and_then(|n| n.points.get(k))
                .is_some_and(|p| p.at != [0.0; 2]),
            AnimRow::When => t.is_some_and(|t| !t.when.is_empty()),
            AnimRow::Blend => t.is_some_and(|t| t.blend != 0.25),
            AnimRow::Priority => t.is_some_and(|t| t.priority != 0),
            AnimRow::Exit => t.is_some_and(|t| t.exit != 0.0),
            AnimRow::Offset => t.is_some_and(|t| t.offset != 0.0),
            AnimRow::KeepPhase => t.is_some_and(|t| t.keep_phase),
            AnimRow::Inertialize => t.is_some_and(|t| t.inertialize),
            AnimRow::ParamKind(k) => self
                .param(k)
                .is_some_and(|(_, p)| p.kind != ParamKind::Float),
            AnimRow::ParamValue(k) => self.param(k).is_some_and(|(_, p)| p.value != 0.0),
            AnimRow::LayerBlend => self
                .shown_layer()
                .is_some_and(|l| l.blend != LayerBlend::Normal),
            AnimRow::LayerWeight => self.shown_layer().is_some_and(|l| l.weight != 1.0),
            AnimRow::LayerWeightParam => self
                .shown_layer()
                .is_some_and(|l| !l.weight_param.is_empty()),
            AnimRow::LayerName => false,
            AnimRow::MaskWeight => self
                .shown_layer()
                .is_some_and(|l| l.mask.contains_key(&self.mask_joint)),
            AnimRow::Name
            | AnimRow::Entry
            | AnimRow::Pose
            | AnimRow::PointClip(_)
            | AnimRow::AddPoint
            | AnimRow::Problem
            | AnimRow::ParamName(_)
            | AnimRow::AddParam => false,
        }
    }

    /// Row `row`'s shown texts into `texts` from `at` on (each number's, a name, a list's
    /// current choice, a problem's why), their rooms reused: how many.
    pub(crate) fn row_texts(&self, row: AnimRow, texts: &mut Vec<String>, at: usize) -> usize {
        let n = match self.look(row) {
            RowLook::Numbers(n, _) => usize::from(n),
            _ => 1,
        };
        while texts.len() < at + n {
            texts.push(String::new());
        }
        let numbers = self.numbers(row);
        let state = self
            .inspected_state()
            .and_then(|id| self.file.states.get(id));
        let pose = self.pose();
        let t = self.transition();
        for (k, text) in texts[at..at + n].iter_mut().enumerate() {
            text.clear();
            match row {
                AnimRow::Speed
                | AnimRow::PointAt(_)
                | AnimRow::Blend
                | AnimRow::Priority
                | AnimRow::Exit
                | AnimRow::Offset
                | AnimRow::LayerWeight
                | AnimRow::MaskWeight
                | AnimRow::ParamValue(_) => graph::value_text(text, &numbers[k..=k]),
                AnimRow::LayerName => {
                    text.push_str(self.shown_layer().map_or("", |l| l.name.as_str()));
                }
                AnimRow::LayerBlend => text.push_str(match self.shown_layer().map(|l| l.blend) {
                    Some(LayerBlend::Additive) => "Additive",
                    _ => "Normal",
                }),
                AnimRow::LayerWeightParam => {
                    let name = self.shown_layer().map_or("", |l| l.weight_param.as_str());
                    text.push_str(if name.is_empty() { "none" } else { name });
                }
                AnimRow::Name => text.push_str(state.map_or("", |s| s.name.as_str())),
                AnimRow::Pose => text.push_str(match pose.map(|n| n.kind) {
                    Some(NodeKind::Clip) => "Clip",
                    Some(NodeKind::Blend1d) => "Blend1d",
                    Some(NodeKind::Blend2d) => "Blend2d",
                    Some(NodeKind::Machine) => "Machine",
                    None => "nothing",
                }),
                AnimRow::Clip => text.push_str(pose.map_or("", |n| n.clip.as_str())),
                AnimRow::BlendParam | AnimRow::BlendParamY => {
                    let name = pose.map_or("", |n| {
                        if row == AnimRow::BlendParam {
                            n.param.as_str()
                        } else {
                            n.param_y.as_str()
                        }
                    });
                    text.push_str(if name.is_empty() { "none" } else { name });
                }
                AnimRow::PointClip(p) => text.push_str(
                    pose.and_then(|n| n.points.get(p))
                        .map_or("", |p| p.clip.as_str()),
                ),
                AnimRow::When => text.push_str(t.map_or("", |t| t.when.as_str())),
                AnimRow::Problem => {
                    let why = match (self.inspected_state(), self.inspected_transition()) {
                        (Some(id), _) => self.state_mark(id),
                        (_, Some(id)) => self.transition_mark(id),
                        _ => None,
                    };
                    text.push_str(why.unwrap_or(""));
                }
                AnimRow::ParamName(p) => text.push_str(self.param(p).map_or("", |(n, _)| n)),
                AnimRow::ParamKind(p) => text.push_str(
                    self.param(p)
                        .map_or("", |(_, p)| PARAM_KINDS[p.kind as usize]),
                ),
                AnimRow::AddParam => text.push('\u{2026}'),
                AnimRow::Entry | AnimRow::AddPoint | AnimRow::KeepPhase | AnimRow::Inertialize => {}
            }
        }
        n
    }

    /// The selection's section's title into `out`: `State · Idle`, `Transition · Idle →
    /// Walk`, `3 selected`, or `Parameters` with nothing selected.
    pub(crate) fn title(&self, out: &mut String) {
        out.clear();
        let name = |id| state_name(&self.file, id);
        if let Some(id) = self.inspected_state() {
            let _ = write!(out, "State \u{b7} {}", name(id));
        } else if let Some(t) = self.transition() {
            let from = if t.from.is_empty() {
                "Any State"
            } else {
                name(&t.from)
            };
            let _ = write!(out, "Transition \u{b7} {from} \u{2192} {}", name(&t.to));
        } else if !self.selected.is_empty() || !self.transitions.is_empty() {
            let _ = write!(
                out,
                "{} selected",
                self.selected.len() + self.transitions.len()
            );
        } else {
            out.push_str("Parameters");
        }
    }

    /// The bar's status into `out` (zoom `zoom`): the machine's states and transitions, the
    /// selection, the zoom, and what does not hold together.
    pub(crate) fn status(&self, zoom: f32, out: &mut String) {
        out.clear();
        if self.machine.is_empty() {
            out.push_str("no state machine: Space adds a state");
        } else {
            let n = self.file.states_of(&self.machine).count();
            let t = self
                .file
                .transitions
                .values()
                .filter(|t| {
                    self.file
                        .states
                        .get(&t.to)
                        .is_some_and(|s| s.machine == self.machine)
                })
                .count();
            let s = if n == 1 { "" } else { "s" };
            let ts = if t == 1 { "" } else { "s" };
            let _ = write!(out, "{n} state{s} \u{b7} {t} transition{ts}");
        }
        let selected = self.selected.len() + self.transitions.len() + usize::from(self.any);
        if selected > 0 {
            let _ = write!(out, " \u{b7} {selected} selected");
        }
        let _ = write!(out, " \u{b7} {:.0} %", zoom * 100.0);
        if let Some(first) = self.marks.first() {
            let n = self.marks.len();
            let s = if n == 1 { "" } else { "s" };
            let _ = write!(out, " \u{b7} \u{26a0} {n} problem{s}: ");
            let name = |id| state_name(&self.file, id);
            match &first.on {
                On::State(id) => {
                    let _ = write!(out, "{}: ", name(id));
                }
                On::Transition(id) => {
                    if let Some(t) = self.file.transitions.get(id) {
                        let from = if t.from.is_empty() {
                            "Any State"
                        } else {
                            name(&t.from)
                        };
                        let _ = write!(out, "{from} \u{2192} {}: ", name(&t.to));
                    }
                }
                On::Graph(what) => {
                    let _ = write!(out, "{what}: ");
                }
            }
            out.push_str(&first.why);
        }
        if self.live.on {
            let _ = write!(out, " \u{b7} {}: {}", self.live.who, self.live.line);
        }
    }
}

/// State `id`'s name in `file`: its id where it has none or is not there.
fn state_name<'a>(file: &'a AnimGraphFile, id: &'a str) -> &'a str {
    match file.states.get(id) {
        Some(s) if !s.name.is_empty() => &s.name,
        _ => id,
    }
}

/// The layer the canvas shows (ADR 0124's E12-5d-2b): `chosen` while the graph has it, else
/// the first (`layers_in_order`) playing a machine, else the first; none without layers.
fn shown_layer(file: &AnimGraphFile, chosen: &str) -> Option<String> {
    if file.layers.contains_key(chosen) {
        return Some(chosen.to_owned());
    }
    let layers = file.layers_in_order();
    layers
        .iter()
        .find(|(_, l)| is_machine(file, &l.root))
        .or(layers.first())
        .map(|(id, _)| (*id).to_owned())
}

/// Joint `j`'s weight under mask `mask`, as the cook binds it (ADR 0124's E12-5d-2b): its
/// own entry's, else its nearest ancestor's, else 1 for an empty mask (every joint) and 0
/// for another. `names` and `parents` in the skeleton's order. Allocates nothing.
pub(crate) fn mask_weight_of(
    mask: &BTreeMap<String, f32>,
    names: &[String],
    parents: &[Option<usize>],
    j: usize,
) -> f32 {
    let mut at = Some(j);
    while let Some(k) = at {
        if let Some(&w) = names.get(k).and_then(|n| mask.get(n)) {
            return w.clamp(0.0, 1.0);
        }
        at = parents.get(k).copied().flatten();
    }
    if mask.is_empty() { 1.0 } else { 0.0 }
}

/// Joint `k` lies under joint `j` (not `j` itself).
fn is_under(parents: &[Option<usize>], k: usize, j: usize) -> bool {
    let mut at = parents.get(k).copied().flatten();
    while let Some(p) = at {
        if p == j {
            return true;
        }
        at = parents.get(p).copied().flatten();
    }
    false
}

/// Mask `mask` with an empty one (every joint at 1) spelt out as its roots at 1, so a weight
/// set on one joint keeps the others in.
fn spell_out(mask: &mut BTreeMap<String, f32>, names: &[String], parents: &[Option<usize>]) {
    if mask.is_empty() {
        for (k, name) in names.iter().enumerate() {
            if parents.get(k).copied().flatten().is_none() {
                mask.insert(name.clone(), 1.0);
            }
        }
    }
}

/// Mask `mask` with joint `j`'s subtree turned in (from a weight of 0) or out (a check box
/// clicked; ADR 0124's E12-5d-2b, the team's а): the joints under `j` lose their own
/// weights, then [`simplify_mask`].
pub(crate) fn toggle_mask(
    mask: &mut BTreeMap<String, f32>,
    names: &[String],
    parents: &[Option<usize>],
    j: usize,
) {
    let Some(name) = names.get(j) else {
        return;
    };
    let to = if mask_weight_of(mask, names, parents, j) > 0.0 {
        0.0
    } else {
        1.0
    };
    spell_out(mask, names, parents);
    for (k, under) in names.iter().enumerate().skip(j + 1) {
        if is_under(parents, k, j) {
            mask.remove(under);
        }
    }
    mask.insert(name.clone(), to);
    simplify_mask(mask, names, parents);
}

/// Joint `j`'s own weight in mask `mask` set to `w` (held within 0 to 1), the others
/// keeping theirs (ADR 0124's E12-5d-2b), then [`simplify_mask`].
pub(crate) fn set_mask_weight(
    mask: &mut BTreeMap<String, f32>,
    names: &[String],
    parents: &[Option<usize>],
    j: usize,
    w: f32,
) {
    let Some(name) = names.get(j) else {
        return;
    };
    spell_out(mask, names, parents);
    mask.insert(name.clone(), w.clamp(0.0, 1.0));
    simplify_mask(mask, names, parents);
}

/// Joint `j`'s own weight in mask `mask` taken out: it inherits its ancestors' again (its
/// ↺; ADR 0124's E12-5d-2b), then [`simplify_mask`].
pub(crate) fn reset_mask_weight(
    mask: &mut BTreeMap<String, f32>,
    names: &[String],
    parents: &[Option<usize>],
    j: usize,
) {
    if let Some(name) = names.get(j)
        && mask.remove(name).is_some()
    {
        simplify_mask(mask, names, parents);
    }
}

/// Mask `mask` written its shortest way with the same weights (ADR 0124's E12-5d-2b): a
/// joint below a root whose own weight is what it inherits loses it; every root at 1 with
/// nothing else is the empty mask; a mask left empty that kept joints out holds its roots'
/// weights (an empty one would put every joint in). Entries naming joints the skeleton has
/// not stay.
pub(crate) fn simplify_mask(
    mask: &mut BTreeMap<String, f32>,
    names: &[String],
    parents: &[Option<usize>],
) {
    let target: Vec<f32> = (0..names.len())
        .map(|j| mask_weight_of(mask, names, parents, j))
        .collect();
    let is_root = |k: usize| parents.get(k).copied().flatten().is_none();
    for (k, name) in names.iter().enumerate() {
        if is_root(k) {
            continue;
        }
        let inherited = parents.get(k).copied().flatten().map_or(0.0, |p| target[p]);
        if mask.get(name).is_some_and(|w| (w - inherited).abs() < 1e-6) {
            mask.remove(name);
        }
    }
    let roots_only = mask
        .keys()
        .all(|n| names.iter().position(|m| m == n).is_some_and(is_root));
    let roots_in = names
        .iter()
        .enumerate()
        .filter(|&(k, _)| is_root(k))
        .all(|(_, n)| mask.get(n) == Some(&1.0));
    if roots_only && roots_in {
        mask.clear();
    }
    if mask.is_empty() && target.iter().any(|&w| w < 1.0) {
        for (k, name) in names.iter().enumerate() {
            if is_root(k) {
                mask.insert(name.clone(), target[k]);
            }
        }
    }
}

/// Node `id` is a state machine.
fn is_machine(file: &AnimGraphFile, id: &str) -> bool {
    file.nodes
        .get(id)
        .is_some_and(|n| n.kind == NodeKind::Machine)
}

/// The machine the canvas shows: layer `layer`'s root where it is one; none otherwise.
fn shown_machine(file: &AnimGraphFile, layer: &str) -> String {
    file.layers
        .get(layer)
        .map(|l| l.root.as_str())
        .filter(|root| is_machine(file, root))
        .unwrap_or("")
        .to_owned()
}

/// The layer whose root is node `root`, by id.
fn layer_of(file: &AnimGraphFile, root: &str) -> Option<String> {
    file.layers
        .iter()
        .find(|(_, l)| l.root == root)
        .map(|(id, _)| id.clone())
}

/// A name no layer of `file` has: `Base` for its first, else `Layer`, `Layer 2`…
fn fresh_layer_name(file: &AnimGraphFile) -> String {
    let taken = |n: &str| file.layers.values().any(|l| l.name == n);
    if file.layers.is_empty() && !taken("Base") {
        return String::from("Base");
    }
    if !taken("Layer") {
        return String::from("Layer");
    }
    (2..)
        .map(|k| format!("Layer {k}"))
        .find(|n| !taken(n))
        .unwrap_or_default()
}

/// Machine node `machine` given an Idle state as its entry where it has no state (ADR 0124's
/// E12-5d-2c: a state made a machine, or one added), playing a clip not named yet, its Any
/// State box above and left of it.
fn fill_machine(file: &mut AnimGraphFile, machine: &str) {
    if file.states.values().any(|s| s.machine == machine) {
        return;
    }
    let clip = fresh_id(file);
    file.nodes.insert(clip.clone(), NodeFile::default());
    let idle = fresh_id(file);
    file.states.insert(
        idle.clone(),
        StateFile {
            name: String::from("Idle"),
            machine: machine.to_owned(),
            pose: clip,
            at: [0.0, 0.0],
        },
    );
    if let Some(m) = file.nodes.get_mut(machine) {
        m.kind = NodeKind::Machine;
        m.at = [-180.0, -90.0];
        m.entry = idle;
    }
}

/// A layer put over `file`'s others (ADR 0124's E12-5d-2b), named by [`fresh_layer_name`],
/// playing a machine of its own whose entry state, Idle, plays a clip not named yet, the
/// machine's Any State box above and left of it: its id.
fn insert_layer(file: &mut AnimGraphFile) -> String {
    let machine = fresh_id(file);
    file.nodes.insert(
        machine.clone(),
        NodeFile {
            kind: NodeKind::Machine,
            at: [-180.0, -90.0],
            ..NodeFile::default()
        },
    );
    let order = file
        .layers
        .values()
        .map(|l| l.order.saturating_add(1))
        .max()
        .unwrap_or(0);
    let name = fresh_layer_name(file);
    let layer = fresh_id(file);
    file.layers.insert(
        layer.clone(),
        LayerFile {
            name,
            order,
            root: machine.clone(),
            ..LayerFile::default()
        },
    );
    let clip = fresh_id(file);
    file.nodes.insert(clip.clone(), NodeFile::default());
    let idle = fresh_id(file);
    file.states.insert(
        idle.clone(),
        StateFile {
            name: String::from("Idle"),
            machine: machine.clone(),
            pose: clip,
            at: [0.0, 0.0],
        },
    );
    if let Some(m) = file.nodes.get_mut(&machine) {
        m.entry = idle;
    }
    layer
}

/// Node `id` and what only it plays taken out of `file` (a layer deleted, ADR 0124's
/// E12-5d-2b): a machine's states with their poses and every transition into or out of
/// them. A node another layer or state still plays stays.
fn remove_tree(file: &mut AnimGraphFile, id: &str) {
    let used =
        file.layers.values().any(|l| l.root == id) || file.states.values().any(|s| s.pose == id);
    if used {
        return;
    }
    let Some(node) = file.nodes.remove(id) else {
        return;
    };
    if node.kind != NodeKind::Machine {
        return;
    }
    let states: Vec<String> = file
        .states
        .iter()
        .filter(|(_, s)| s.machine == id)
        .map(|(k, _)| k.clone())
        .collect();
    file.transitions
        .retain(|_, t| !states.contains(&t.to) && !states.contains(&t.from));
    let poses: Vec<String> = states
        .iter()
        .filter_map(|k| file.states.remove(k))
        .map(|s| s.pose)
        .collect();
    for pose in poses {
        remove_tree(file, &pose);
    }
}

/// What pose `pose` plays, as its state's box says it, into `out`: a clip's name, `Blend1d
/// speed: Walk · Run`, `Machine`; at most [`SUMMARY_MAX`] characters.
fn pose_summary(file: &AnimGraphFile, pose: &str, out: &mut String) {
    out.clear();
    match file.nodes.get(pose) {
        None => out.push_str("no pose"),
        Some(n) => match n.kind {
            NodeKind::Clip if n.clip.is_empty() => out.push_str("no clip"),
            NodeKind::Clip => out.push_str(&n.clip),
            NodeKind::Blend1d | NodeKind::Blend2d => {
                out.push_str(if n.kind == NodeKind::Blend1d {
                    "Blend1d "
                } else {
                    "Blend2d "
                });
                out.push_str(if n.param.is_empty() { "?" } else { &n.param });
                if n.kind == NodeKind::Blend2d {
                    out.push_str(", ");
                    out.push_str(if n.param_y.is_empty() {
                        "?"
                    } else {
                        &n.param_y
                    });
                }
                out.push(':');
                for (k, p) in n.points.iter().enumerate() {
                    out.push_str(if k == 0 { " " } else { " \u{b7} " });
                    out.push_str(&p.clip);
                }
            }
            NodeKind::Machine => out.push_str("Machine"),
        },
    }
    if let Some((cut, _)) = out.char_indices().nth(SUMMARY_MAX) {
        out.truncate(cut);
        out.push('\u{2026}');
    }
}

/// Where a transition from box `a` to box `b` (their corners) leaves and arrives: the line
/// between their middles moved `offset` to its right, cut at their edges; none where they
/// overlap.
fn edge_points(a: [[f32; 2]; 2], b: [[f32; 2]; 2], offset: f32) -> Option<([f32; 2], [f32; 2])> {
    let middle = |r: [[f32; 2]; 2]| [(r[0][0] + r[1][0]) * 0.5, (r[0][1] + r[1][1]) * 0.5];
    let (ca, cb) = (middle(a), middle(b));
    let d = [cb[0] - ca[0], cb[1] - ca[1]];
    let length = d[0].hypot(d[1]);
    if length < 1e-3 {
        return None;
    }
    let right = [-d[1] / length * offset, d[0] / length * offset];
    let p0 = [ca[0] + right[0], ca[1] + right[1]];
    let p1 = [cb[0] + right[0], cb[1] + right[1]];
    let leave = crossing(p0, p1, a).map_or(0.0, |(_, out)| out.clamp(0.0, 1.0));
    let arrive = crossing(p0, p1, b).map_or(1.0, |(into, _)| into.clamp(0.0, 1.0));
    if arrive <= leave {
        return None;
    }
    let at = |t: f32| [p0[0] + (p1[0] - p0[0]) * t, p0[1] + (p1[1] - p0[1]) * t];
    Some((at(leave), at(arrive)))
}

/// Where the line from `p0` (0) to `p1` (1) goes into and out of box `r`; none where it
/// misses it.
fn crossing(p0: [f32; 2], p1: [f32; 2], r: [[f32; 2]; 2]) -> Option<(f32, f32)> {
    let (mut into, mut out) = (f32::NEG_INFINITY, f32::INFINITY);
    for axis in 0..2 {
        let d = p1[axis] - p0[axis];
        if d.abs() < 1e-6 {
            if p0[axis] < r[0][axis] || p0[axis] > r[1][axis] {
                return None;
            }
        } else {
            let t0 = (r[0][axis] - p0[axis]) / d;
            let t1 = (r[1][axis] - p0[axis]) / d;
            into = into.max(t0.min(t1));
            out = out.min(t0.max(t1));
        }
    }
    (into <= out).then_some((into, out))
}

/// Whether canvas point `p` is on box `r`'s edge (a band `reach` wide, at most a quarter of
/// its height, half of it outside the box) or inside it; none off it.
fn edge_or_inside(r: [[f32; 2]; 2], p: [f32; 2], reach: f32) -> Option<bool> {
    let band = reach.min((r[1][1] - r[0][1]) * 0.25);
    let out = band * 0.5;
    if p[0] < r[0][0] - out || p[1] < r[0][1] - out || p[0] > r[1][0] + out || p[1] > r[1][1] + out
    {
        return None;
    }
    let inside = p[0] > r[0][0] + band
        && p[1] > r[0][1] + band
        && p[0] < r[1][0] - band
        && p[1] < r[1][1] - band;
    Some(!inside)
}

/// Condition `text` compiled against `file`'s parameters (their slots in their names'
/// order, a trigger said), as the game's cook compiles it.
///
/// # Errors
///
/// What does not compile, at its place.
pub(crate) fn compile(file: &AnimGraphFile, text: &str) -> Result<Condition, ConditionError> {
    Condition::compile(text, |name| {
        let (k, (_, p)) = file
            .params
            .iter()
            .enumerate()
            .find(|(_, (n, _))| *n == name)?;
        Some((u16::try_from(k).ok()?, p.kind == ParamKind::Trigger))
    })
}

/// Why condition `text` does not compile, saying where: its message and the character it is
/// at (`no parameter `speeed` (at 1)`), the part there where the message does not quote it.
pub(crate) fn condition_why(text: &str, e: &ConditionError) -> String {
    let column = text.get(..e.at).map_or(e.at, |s| s.chars().count()) + 1;
    let part = text.get(e.at..e.at + e.len).unwrap_or("");
    if part.is_empty() || e.message.contains(part) {
        format!("{} (at {column})", e.message)
    } else {
        format!("{}: `{part}` (at {column})", e.message)
    }
}

/// Whether `name` can name a parameter: a condition's word (a letter or `_` first, then
/// letters, digits and `_`), not one of its own (`and`, `or`, `not`, `true`, `false`).
pub(crate) fn is_param_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !matches!(name, "and" | "or" | "not" | "true" | "false")
}

/// Condition `text` with every word `old` made `new` (a parameter renamed), its numbers
/// and everything else as they were.
pub(crate) fn renamed(text: &str, old: &str, new: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        if c.is_ascii_alphabetic() || c == b'_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &text[start..i];
            out.push_str(if word == old { new } else { word });
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            // A number, as the conditions read one: digits and points, an exponent.
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                let mut j = i + 1;
                if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    i = j;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
        out.push_str(&text[start..i]);
    }
    out
}

/// A new part's id (ADR 0123's sketch: `a3f9`): four random hex digits no part of `file`
/// has, eight once a few tries met taken ones.
fn fresh_id(file: &AnimGraphFile) -> String {
    let taken = |id: &str| {
        file.states.contains_key(id)
            || file.nodes.contains_key(id)
            || file.transitions.contains_key(id)
            || file.layers.contains_key(id)
    };
    for _ in 0..32 {
        let id = format!("{:04x}", AssetId::new().as_u128() as u16);
        if !taken(&id) {
            return id;
        }
    }
    loop {
        let id = format!("{:08x}", AssetId::new().as_u128() as u32);
        if !taken(&id) {
            return id;
        }
    }
}

/// A name no state of machine `machine` has: `base`, else `base 2`, `base 3`…
fn fresh_state_name(file: &AnimGraphFile, machine: &str, base: &str) -> String {
    let taken = |n: &str| {
        file.states
            .values()
            .any(|s| s.machine == machine && s.name == n)
    };
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|k| format!("{base} {k}"))
        .find(|n| !taken(n))
        .unwrap_or_default()
}

/// A parameter's name no parameter of `file` has, from what it holds: `number`, `flag`,
/// `count`, `trigger`, numbered when taken (`number2`).
fn fresh_param(file: &AnimGraphFile, kind: ParamKind) -> String {
    let base = match kind {
        ParamKind::Float => "number",
        ParamKind::Bool => "flag",
        ParamKind::Int => "count",
        ParamKind::Trigger => "trigger",
    };
    if !file.params.contains_key(base) {
        return base.to_owned();
    }
    (2..)
        .map(|k| format!("{base}{k}"))
        .find(|n| !file.params.contains_key(n))
        .unwrap_or_default()
}

/// A new animation graph (New Animation Graph): a base layer playing a machine whose entry
/// state, Idle, plays a clip not named yet; the machine's Any State box above and left of
/// it.
pub(crate) fn starter() -> AnimGraphFile {
    let mut file = AnimGraphFile::default();
    insert_layer(&mut file);
    file
}

/// A state playing `kind` put into the machine `file` shows (a layer and a machine made
/// for it where it shows none) at `at`, named `New State` (numbered where its machine has
/// one), its machine's entry where it had none, with a transition into it from `from`: its
/// id.
fn insert_state(
    file: &mut AnimGraphFile,
    shown: &str,
    kind: StateKind,
    at: [f32; 2],
    from: Option<&Source>,
) -> String {
    let at = at.map(f32::round);
    let mut machine = shown.to_owned();
    if !is_machine(file, &machine) {
        machine.clear();
    }
    if machine.is_empty() {
        machine = fresh_id(file);
        file.nodes.insert(
            machine.clone(),
            NodeFile {
                kind: NodeKind::Machine,
                at: [at[0] - 180.0, at[1] - 90.0],
                ..NodeFile::default()
            },
        );
        let order = file
            .layers
            .values()
            .map(|l| l.order.saturating_add(1))
            .max()
            .unwrap_or(0);
        let name = if file.layers.is_empty() {
            "Base"
        } else {
            "Layer"
        };
        let layer = fresh_id(file);
        file.layers.insert(
            layer,
            LayerFile {
                name: String::from(name),
                order,
                root: machine.clone(),
                ..LayerFile::default()
            },
        );
    }
    let mut node = NodeFile {
        kind: kind.node(),
        ..NodeFile::default()
    };
    if kind != StateKind::Clip {
        // A blend follows the graph's first number, and its second up.
        let mut numbers = file
            .params
            .iter()
            .filter(|(_, p)| matches!(p.kind, ParamKind::Float | ParamKind::Int))
            .map(|(name, _)| name.clone());
        node.param = numbers.next().unwrap_or_default();
        if kind == StateKind::Blend2d {
            node.param_y = numbers.next().unwrap_or_default();
        }
    }
    let pose = fresh_id(file);
    file.nodes.insert(pose.clone(), node);
    if kind == StateKind::Machine {
        fill_machine(file, &pose);
    }
    let name = fresh_state_name(file, &machine, "New State");
    let id = fresh_id(file);
    file.states.insert(
        id.clone(),
        StateFile {
            name,
            machine: machine.clone(),
            pose,
            at,
        },
    );
    let started = file.nodes.get(&machine).is_some_and(|m| {
        file.states
            .get(&m.entry)
            .is_some_and(|s| s.machine == machine)
    });
    if !started && let Some(m) = file.nodes.get_mut(&machine) {
        m.entry.clone_from(&id);
    }
    if let Some(from) = from {
        let t = fresh_id(file);
        file.transitions.insert(t, drawn_transition(from.id(), &id));
    }
    id
}

/// A transition drawn on the canvas from state `from` (empty: Any State) to `to`: the file's
/// defaults, but inertialized (team decision 2026-10-30, ADR 0124's E12-5d-1: the
/// destination plays at full strength at once; the file's own default stays a cross-fade).
fn drawn_transition(from: &str, to: &str) -> TransitionFile {
    TransitionFile {
        from: from.to_owned(),
        to: to.to_owned(),
        inertialize: true,
        ..TransitionFile::default()
    }
}

/// States `states` taken out of `file` with every transition from or into them (and the
/// transitions `transitions`): their poses too where nothing else shows or plays them, a
/// machine's states with it; a machine whose entry went starts at its first state left.
fn remove(file: &mut AnimGraphFile, states: &BTreeSet<String>, transitions: &BTreeSet<String>) {
    let mut gone: Vec<String> = states.iter().cloned().collect();
    let mut k = 0;
    while k < gone.len() {
        let id = gone[k].clone();
        k += 1;
        let Some(state) = file.states.remove(&id) else {
            continue;
        };
        let used = file.states.values().any(|s| s.pose == state.pose)
            || file.layers.values().any(|l| l.root == state.pose);
        if used {
            continue;
        }
        if file
            .nodes
            .remove(&state.pose)
            .is_some_and(|n| n.kind == NodeKind::Machine)
        {
            gone.extend(
                file.states
                    .iter()
                    .filter(|(_, s)| s.machine == state.pose)
                    .map(|(id, _)| id.clone()),
            );
        }
    }
    let gone: BTreeSet<String> = gone.into_iter().collect();
    file.transitions.retain(|id, t| {
        !transitions.contains(id) && !gone.contains(&t.to) && !gone.contains(&t.from)
    });
    let restarted: Vec<(String, String)> = file
        .nodes
        .iter()
        .filter(|(_, n)| n.kind == NodeKind::Machine && gone.contains(&n.entry))
        .map(|(id, _)| {
            let first = file
                .states
                .iter()
                .find(|(_, s)| s.machine == *id)
                .map(|(state, _)| state.clone());
            (id.clone(), first.unwrap_or_default())
        })
        .collect();
    for (machine, first) in restarted {
        if let Some(m) = file.nodes.get_mut(&machine) {
            m.entry = first;
        }
    }
}

impl GraphDoc {
    /// Its animation graph's document, where it is one.
    pub(crate) fn anim(&self) -> Option<&AnimDoc> {
        self.anim.as_deref()
    }

    /// `edit` applied to its animation graph's file: its text before and after, when it
    /// changed.
    pub(crate) fn edit_anim(
        &mut self,
        edit: impl FnOnce(&mut AnimGraphFile),
    ) -> Option<(String, String)> {
        edit(&mut self.anim.as_deref_mut()?.file);
        self.settle()
    }

    /// A state playing `kind` added with its box's middle at canvas point `at` to the
    /// machine shown (a base layer and a machine made where the graph shows none), named
    /// `New State`, its machine's entry where it had none, and selected alone; with a
    /// transition into it from `from` (an arrow dropped on the empty canvas). A step.
    pub(crate) fn add_state(
        &mut self,
        kind: StateKind,
        at: [f32; 2],
        from: Option<&Source>,
    ) -> Option<Outcome> {
        let corner = [at[0] - STATE_WIDTH * 0.5, at[1] - STATE_HEIGHT * 0.5];
        let mut made = String::new();
        let shown = self.anim.as_deref()?.machine.clone();
        let (before, after) =
            self.edit_anim(|f| made = insert_state(f, &shown, kind, corner, from))?;
        let anim = self.anim.as_deref_mut()?;
        // A machine made for it (the layer shown played none): its layer shown.
        let machine = anim.file.states.get(&made).map(|s| s.machine.clone());
        if let Some(layer) = machine.and_then(|m| layer_of(&anim.file, &m))
            && layer != anim.layer
        {
            anim.layer = layer;
            anim.refresh();
            anim.laid_out = None;
        }
        anim.clear_selection();
        anim.selected.insert(made);
        Some(Outcome::Edited {
            name: "Add State",
            before,
            after,
        })
    }

    /// A transition from `from` into state `to` (of the machine shown) with the file's
    /// defaults (no condition yet, a quarter second's blend), selected alone so its
    /// condition is typed next. A step; refused into the state it leaves.
    pub(crate) fn connect(&mut self, from: &Source, to: &str) -> Option<Outcome> {
        if from.id() == to {
            return Some(Outcome::Refused("A state does not go into itself"));
        }
        let anim = self.anim.as_deref()?;
        if anim
            .file
            .states
            .get(to)
            .is_none_or(|s| s.machine != anim.machine)
        {
            return None;
        }
        let mut made = String::new();
        let (before, after) = self.edit_anim(|f| {
            made = fresh_id(f);
            f.transitions
                .insert(made.clone(), drawn_transition(from.id(), to));
        })?;
        let anim = self.anim.as_deref_mut()?;
        anim.clear_selection();
        anim.transitions.insert(made);
        Some(Outcome::Edited {
            name: "Add Transition",
            before,
            after,
        })
    }

    /// The selected states deleted with their transitions (their poses too where no other
    /// state shows them) and the selected transitions, as one step; a machine whose entry
    /// went starts at its first state left. The Any State box stays.
    pub(crate) fn delete_anim_selection(&mut self) -> Option<Outcome> {
        let anim = self.anim.as_deref_mut()?;
        if anim.selected.is_empty() && anim.transitions.is_empty() {
            return anim.any.then_some(Outcome::Refused(
                "Any State stays: the transitions from every state start there",
            ));
        }
        let states = core::mem::take(&mut anim.selected);
        let transitions = core::mem::take(&mut anim.transitions);
        anim.any = false;
        let name = match (states.len(), transitions.len()) {
            (1, 0) => "Delete State",
            (_, 0) => "Delete States",
            (0, 1) => "Delete Transition",
            (0, _) => "Delete Transitions",
            _ => "Delete",
        };
        let (before, after) = self.edit_anim(|f| remove(f, &states, &transitions))?;
        Some(Outcome::Edited {
            name,
            before,
            after,
        })
    }

    /// State `id` its machine's entry (where it starts), as a step.
    pub(crate) fn set_entry(&mut self, id: &str) -> Option<Outcome> {
        let anim = self.anim.as_deref()?;
        let machine = anim.file.states.get(id)?.machine.clone();
        if anim.file.nodes.get(&machine).is_some_and(|m| m.entry == id) {
            return Some(Outcome::Refused("It is its machine's entry already"));
        }
        let (before, after) = self.edit_anim(|f| {
            if let Some(m) = f.nodes.get_mut(&machine) {
                id.clone_into(&mut m.entry);
            }
        })?;
        Some(Outcome::Edited {
            name: "Set Entry State",
            before,
            after,
        })
    }

    /// A frame of the pointer on an animation graph's canvas whose panel has its top left
    /// at `origin`: the view panned and zoomed, the selection made, states moved and
    /// connected; `drag` is the drag under way, kept between frames.
    pub(crate) fn anim_pointer(
        &mut self,
        p: &Pointer,
        origin: [f32; 2],
        drag: &mut Option<Drag>,
    ) -> Option<Outcome> {
        let at = p.at.map(|q| self.view.canvas_point(q, origin));
        self.cursor = at;
        let zoom = self.view.zoom();
        let reach = PORT_REACH / zoom;
        let edge = EDGE / zoom;
        let anim = self.anim.as_deref_mut()?;
        anim.hover = match at {
            Some(c) if p.over || drag.is_some() => anim.hit(c, edge),
            _ => Hit::Canvas,
        };
        // Escape: the move under way taken back, or else the selection let go.
        if p.escape {
            match drag.take() {
                Some(Drag::Move { starts, any, .. }) => {
                    for (id, at) in starts {
                        if let Some(s) = anim.file.states.get_mut(&id) {
                            s.at = at;
                        }
                    }
                    if let Some(at) = any
                        && let Some(m) = anim.file.nodes.get_mut(&anim.machine)
                    {
                        m.at = at;
                    }
                    self.revision += 1;
                }
                Some(_) => {}
                None => anim.clear_selection(),
            }
            return None;
        }
        if p.over
            && p.wheel != 0.0
            && let Some(q) = p.at
        {
            let steps = p.wheel.signum() as i32 * p.wheel.abs().ceil().max(1.0) as i32;
            self.view.zoom_by(steps, q, origin);
        }
        if matches!(drag, Some(Drag::Pan)) {
            if p.pan_held {
                self.view.pan_by(p.moved);
            } else {
                *drag = None;
            }
            return None;
        }
        if drag.is_none() && p.over && p.pan_press {
            *drag = Some(Drag::Pan);
            return None;
        }
        if drag.is_none() {
            if p.press
                && p.over
                && let Some(c) = at
            {
                // A second press soon after the first, in its place, on a state playing a
                // machine: into it (E12-5d-2c, Unity's and Unreal's double-click).
                let now = std::time::Instant::now();
                let again = self.last_press.replace((now, c)).is_some_and(|(t, q)| {
                    now.duration_since(t) <= graph::DOUBLE_CLICK
                        && (q[0] - c[0]).hypot(q[1] - c[1]) * zoom <= CLICK_SLOP
                });
                if again
                    && let Hit::State(k) = anim.hover
                    && let Some(b) = anim.boxes.get(k).filter(|b| b.machine)
                {
                    let id = b.id.clone();
                    self.last_press = None;
                    self.enter_machine(&id);
                    return None;
                }
                *drag = anim.press(c, edge, p.shift, p.ctrl);
            }
            return None;
        }
        let ended = !p.held;
        match drag.as_mut()? {
            Drag::Move {
                from,
                starts,
                any,
                moved,
            } => {
                if let Some(c) = at {
                    let d = [c[0] - from[0], c[1] - from[1]];
                    if !*moved && d[0].hypot(d[1]) * zoom > CLICK_SLOP {
                        *moved = true;
                    }
                    if *moved {
                        for (id, s) in starts.iter() {
                            if let Some(state) = anim.file.states.get_mut(id) {
                                state.at = [s[0] + d[0], s[1] + d[1]];
                            }
                        }
                        if let Some(s) = any
                            && let Some(m) = anim.file.nodes.get_mut(&anim.machine)
                        {
                            m.at = [s[0] + d[0], s[1] + d[1]];
                        }
                        self.revision += 1;
                    }
                }
                if !ended {
                    return None;
                }
                let Some(Drag::Move {
                    starts, any, moved, ..
                }) = drag.take()
                else {
                    return None;
                };
                if !moved {
                    return None;
                }
                // It ends on whole units: the file keeps short numbers.
                for (id, _) in &starts {
                    if let Some(state) = anim.file.states.get_mut(id) {
                        state.at = state.at.map(f32::round);
                    }
                }
                if any.is_some()
                    && let Some(m) = anim.file.nodes.get_mut(&anim.machine)
                {
                    m.at = m.at.map(f32::round);
                }
                let name = match (starts.len(), any.is_some()) {
                    (0, true) => "Move Any State",
                    (1, false) => "Move State",
                    _ => "Move States",
                };
                self.settle().map(|(before, after)| Outcome::Edited {
                    name,
                    before,
                    after,
                })
            }
            Drag::Link { .. } => {
                if !ended {
                    return None;
                }
                let Some(Drag::Link { from }) = drag.take() else {
                    return None;
                };
                let on = at.map(|c| anim.hit(c, reach));
                match on {
                    Some(Hit::State(k) | Hit::Edge(k)) => {
                        let to = anim.boxes[k].id.clone();
                        self.connect(&from, &to)
                    }
                    // On the empty canvas: the state it goes to, made there.
                    Some(Hit::Canvas) => {
                        let c = at?;
                        self.add_state(StateKind::Clip, c, Some(&from))
                    }
                    Some(Hit::Any | Hit::AnyEdge) => Some(Outcome::Refused(
                        "Any State only starts transitions: drop the arrow on a state",
                    )),
                    Some(Hit::Arrow(_)) | None => None,
                }
            }
            Drag::Select { from, add } => {
                if !ended {
                    return None;
                }
                let (from, add) = (*from, *add);
                *drag = None;
                let to = at.unwrap_or(from);
                if !add {
                    anim.clear_selection();
                }
                let size = [(to[0] - from[0]).abs(), (to[1] - from[1]).abs()];
                if size[0].max(size[1]) * zoom > CLICK_SLOP {
                    let lo = [from[0].min(to[0]), from[1].min(to[1])];
                    let hi = [from[0].max(to[0]), from[1].max(to[1])];
                    let meets = |r: [[f32; 2]; 2]| {
                        r[0][0] <= hi[0] && r[0][1] <= hi[1] && r[1][0] >= lo[0] && r[1][1] >= lo[1]
                    };
                    for b in &anim.boxes {
                        if meets(b.rect()) {
                            anim.selected.insert(b.id.clone());
                        }
                    }
                    if anim.any_rect().is_some_and(meets) {
                        anim.any = true;
                    }
                }
                None
            }
            Drag::Pan => None,
        }
    }

    /// The inspected state's blend's clips put at `places` (each one's place among its
    /// points, and where; ADR 0124's E12-5d-2a, Place by Speed): its step's texts, when it
    /// changed.
    pub(crate) fn place_points(
        &mut self,
        places: &[(usize, [f32; 2])],
    ) -> Option<(String, String)> {
        let state = self.anim.as_deref()?.inspected_state()?.to_owned();
        self.edit_anim(|f| {
            if let Some(n) = Self::anim_pose_mut(f, &state) {
                for &(k, at) in places {
                    if let Some(p) = n.points.get_mut(k) {
                        p.at = at;
                    }
                }
            }
        })
    }

    /// The inspected state's pose node, to change.
    fn anim_pose_mut<'f>(f: &'f mut AnimGraphFile, state: &str) -> Option<&'f mut NodeFile> {
        let pose = f.states.get(state)?.pose.clone();
        f.nodes.get_mut(&pose)
    }

    /// Row `row` set to `values` (a drag's or what was typed; a priority whole, seconds and
    /// fractions not below 0): its step's texts, when it changed.
    pub(crate) fn anim_set_numbers(
        &mut self,
        row: AnimRow,
        values: [f32; 4],
    ) -> Option<(String, String)> {
        let anim = self.anim.as_deref()?;
        let state = anim.inspected_state().map(str::to_owned);
        let transition = anim.inspected_transition().map(str::to_owned);
        let layer = anim.layer.clone();
        let param = match row {
            AnimRow::ParamValue(k) => anim.param(k).map(|(n, p)| (n.to_owned(), p.kind)),
            _ => None,
        };
        let joint = anim
            .mask_joint()
            .map(|j| (anim.joints.clone(), anim.joint_parents.clone(), j));
        let v = values[0];
        self.edit_anim(|f| match row {
            AnimRow::Speed => {
                if let Some(n) = state.as_deref().and_then(|s| Self::anim_pose_mut(f, s)) {
                    n.speed = v;
                }
            }
            AnimRow::PointAt(k) => {
                if let Some(p) = state
                    .as_deref()
                    .and_then(|s| Self::anim_pose_mut(f, s))
                    .and_then(|n| n.points.get_mut(k))
                {
                    p.at = [values[0], values[1]];
                }
            }
            AnimRow::LayerWeight => {
                if let Some(l) = f.layers.get_mut(&layer) {
                    l.weight = v.clamp(0.0, 1.0);
                }
            }
            AnimRow::MaskWeight => {
                if let (Some(l), Some((names, parents, j))) = (f.layers.get_mut(&layer), &joint) {
                    set_mask_weight(&mut l.mask, names, parents, *j, v);
                }
            }
            AnimRow::Blend | AnimRow::Priority | AnimRow::Exit | AnimRow::Offset => {
                let Some(t) = transition.as_deref().and_then(|t| f.transitions.get_mut(t)) else {
                    return;
                };
                match row {
                    AnimRow::Blend => t.blend = v.max(0.0),
                    AnimRow::Priority => t.priority = v.round() as i32,
                    AnimRow::Exit => t.exit = v.max(0.0),
                    _ => t.offset = v.max(0.0),
                }
            }
            AnimRow::ParamValue(_) => {
                if let Some((name, kind)) = &param
                    && let Some(p) = f.params.get_mut(name)
                {
                    p.value = match kind {
                        ParamKind::Int => v.round(),
                        ParamKind::Bool => f32::from(u8::from(v != 0.0)),
                        ParamKind::Trigger => 0.0,
                        ParamKind::Float => v,
                    };
                }
            }
            _ => {}
        })
    }

    /// Row `row`'s text set: a state's name (one its machine's other states lack), a clip's
    /// name, a blend's clip (emptied, it goes) or one more, a condition (kept as typed, its
    /// problem shown), a parameter's name (a condition's word no other parameter has; the
    /// conditions, blends and layers naming it follow).
    ///
    /// # Errors
    ///
    /// A name that cannot be, said why.
    pub(crate) fn anim_set_text(
        &mut self,
        row: AnimRow,
        text: &str,
    ) -> Result<Option<(String, String)>, String> {
        let Some(anim) = self.anim.as_deref() else {
            return Ok(None);
        };
        let state = anim.inspected_state().map(str::to_owned);
        let transition = anim.inspected_transition().map(str::to_owned);
        let layer = anim.layer.clone();
        let trimmed = text.trim();
        match row {
            AnimRow::LayerName => {
                if trimmed.is_empty() {
                    return Err(String::from("A layer has a name: the bar shows it"));
                }
                let taken = anim
                    .file
                    .layers
                    .iter()
                    .any(|(k, l)| *k != layer && l.name == trimmed);
                if taken {
                    return Err(format!("Another layer is named {trimmed}"));
                }
                Ok(self.edit_anim(|f| {
                    if let Some(l) = f.layers.get_mut(&layer) {
                        trimmed.clone_into(&mut l.name);
                    }
                }))
            }
            AnimRow::Name => {
                let Some(id) = state else {
                    return Ok(None);
                };
                if trimmed.is_empty() {
                    return Err(String::from("A state has a name: scripts read it"));
                }
                let machine = anim.file.states.get(&id).map_or("", |s| s.machine.as_str());
                let taken = anim
                    .file
                    .states
                    .iter()
                    .any(|(k, s)| *k != id && s.machine == machine && s.name == trimmed);
                if taken {
                    return Err(format!("Another state of its machine is named {trimmed}"));
                }
                Ok(self.edit_anim(|f| {
                    if let Some(s) = f.states.get_mut(&id) {
                        trimmed.clone_into(&mut s.name);
                    }
                }))
            }
            AnimRow::Clip => Ok(self.edit_anim(|f| {
                if let Some(n) = state.as_deref().and_then(|s| Self::anim_pose_mut(f, s)) {
                    trimmed.clone_into(&mut n.clip);
                }
            })),
            AnimRow::PointClip(k) => Ok(self.edit_anim(|f| {
                if let Some(n) = state.as_deref().and_then(|s| Self::anim_pose_mut(f, s)) {
                    if trimmed.is_empty() {
                        if k < n.points.len() {
                            n.points.remove(k);
                        }
                    } else if let Some(p) = n.points.get_mut(k) {
                        trimmed.clone_into(&mut p.clip);
                    }
                }
            })),
            AnimRow::AddPoint => {
                if trimmed.is_empty() {
                    return Ok(None);
                }
                Ok(self.edit_anim(|f| {
                    if let Some(n) = state.as_deref().and_then(|s| Self::anim_pose_mut(f, s)) {
                        // One step on from the last along the parameter.
                        let at = n.points.last().map_or([0.0; 2], |p| {
                            if n.kind == NodeKind::Blend2d {
                                [p.at[0] + 1.0, p.at[1]]
                            } else {
                                [p.at[0] + 1.0, 0.0]
                            }
                        });
                        n.points.push(BlendPoint {
                            at,
                            clip: trimmed.to_owned(),
                        });
                    }
                }))
            }
            AnimRow::When => Ok(self.edit_anim(|f| {
                if let Some(t) = transition.as_deref().and_then(|t| f.transitions.get_mut(t)) {
                    trimmed.clone_into(&mut t.when);
                }
            })),
            AnimRow::ParamName(k) => {
                let Some((old, _)) = anim.param(k) else {
                    return Ok(None);
                };
                let old = old.to_owned();
                if trimmed == old {
                    return Ok(None);
                }
                if !is_param_name(trimmed) {
                    return Err(String::from(
                        "A parameter's name is a condition's word: letters, digits and `_`, \
                         not a digit first, nor `and`, `or`, `not`, `true` or `false`",
                    ));
                }
                if anim.file.params.contains_key(trimmed) {
                    return Err(format!("Another parameter is named {trimmed}"));
                }
                Ok(self.edit_anim(|f| {
                    if let Some(p) = f.params.remove(&old) {
                        f.params.insert(trimmed.to_owned(), p);
                    }
                    for t in f.transitions.values_mut() {
                        t.when = renamed(&t.when, &old, trimmed);
                    }
                    for n in f.nodes.values_mut() {
                        for name in [&mut n.param, &mut n.param_y] {
                            if *name == old {
                                trimmed.clone_into(name);
                            }
                        }
                    }
                    for l in f.layers.values_mut() {
                        if l.weight_param == old {
                            trimmed.clone_into(&mut l.weight_param);
                        }
                    }
                }))
            }
            _ => Ok(None),
        }
    }

    /// Row `row`'s switch flipped: the state its machine's entry, a transition's keep
    /// phase or inertialization, a flag's starting value.
    ///
    /// # Errors
    ///
    /// The entry switched off: a machine starts at one of its states.
    pub(crate) fn anim_toggle(&mut self, row: AnimRow) -> Result<Option<Outcome>, &'static str> {
        let Some(anim) = self.anim.as_deref() else {
            return Ok(None);
        };
        let transition = anim.inspected_transition().map(str::to_owned);
        match row {
            AnimRow::Entry => {
                let Some(id) = anim.inspected_state().map(str::to_owned) else {
                    return Ok(None);
                };
                if anim.look(row) == RowLook::Check(true) {
                    return Err("A machine starts at one of its states: make another its entry");
                }
                Ok(self.set_entry(&id))
            }
            AnimRow::KeepPhase | AnimRow::Inertialize => {
                let name = if row == AnimRow::KeepPhase {
                    "Toggle Keep Phase"
                } else {
                    "Toggle Inertialize"
                };
                let edit = self.edit_anim(|f| {
                    if let Some(t) = transition.as_deref().and_then(|t| f.transitions.get_mut(t)) {
                        if row == AnimRow::KeepPhase {
                            t.keep_phase = !t.keep_phase;
                        } else {
                            t.inertialize = !t.inertialize;
                        }
                    }
                });
                Ok(edit.map(|(before, after)| Outcome::Edited {
                    name,
                    before,
                    after,
                }))
            }
            AnimRow::ParamValue(k) => {
                let Some((name, _)) = anim.param(k) else {
                    return Ok(None);
                };
                let name = name.to_owned();
                let edit = self.edit_anim(|f| {
                    if let Some(p) = f.params.get_mut(&name) {
                        p.value = if p.value == 0.0 { 1.0 } else { 0.0 };
                    }
                });
                Ok(edit.map(|(before, after)| Outcome::Edited {
                    name: "Toggle Starting Value",
                    before,
                    after,
                }))
            }
            _ => Ok(None),
        }
    }

    /// Row `row`'s choice `i` taken: what the state's pose is (its clips kept, a blend's
    /// parameters the graph's first numbers where it had none), a blend's parameter, what
    /// a parameter holds (its value made one it can hold), a parameter of a kind added (its
    /// name from its kind, `number`).
    pub(crate) fn anim_choose(&mut self, row: AnimRow, i: usize) -> Option<(String, String)> {
        let anim = self.anim.as_deref()?;
        let state = anim.inspected_state().map(str::to_owned);
        let layer = anim.layer.clone();
        let mut options = Vec::new();
        anim.options(row, &mut options);
        let picked = options.get(i).map(|s| (*s).to_owned())?;
        let numbers: Vec<String> = {
            let mut v = Vec::new();
            anim.options(AnimRow::BlendParam, &mut v);
            v.into_iter().map(str::to_owned).collect()
        };
        let param = match row {
            AnimRow::ParamKind(k) => anim.param(k).map(|(n, _)| n.to_owned()),
            _ => None,
        };
        let kind_of = |name: &str| match name {
            "Bool" => ParamKind::Bool,
            "Int" => ParamKind::Int,
            "Trigger" => ParamKind::Trigger,
            _ => ParamKind::Float,
        };
        self.edit_anim(|f| match row {
            AnimRow::Pose => {
                let Some(pose) = state
                    .as_deref()
                    .and_then(|s| f.states.get(s))
                    .map(|s| s.pose.clone())
                else {
                    return;
                };
                let Some(n) = f.nodes.get_mut(&pose) else {
                    return;
                };
                let was_machine = n.kind == NodeKind::Machine;
                n.kind = match picked.as_str() {
                    "Blend1d" => NodeKind::Blend1d,
                    "Blend2d" => NodeKind::Blend2d,
                    "Machine" => NodeKind::Machine,
                    _ => NodeKind::Clip,
                };
                if matches!(n.kind, NodeKind::Blend1d | NodeKind::Blend2d) && n.param.is_empty() {
                    n.param = numbers.first().cloned().unwrap_or_default();
                }
                if n.kind == NodeKind::Blend2d && n.param_y.is_empty() {
                    n.param_y = numbers.get(1).cloned().unwrap_or_default();
                }
                let machine = n.kind == NodeKind::Machine;
                // A machine made (E12-5d-2c): its own Idle; one no longer: its states go.
                if machine && !was_machine {
                    fill_machine(f, &pose);
                } else if was_machine && !machine {
                    if let Some(n) = f.nodes.get_mut(&pose) {
                        n.entry.clear();
                    }
                    let inner: BTreeSet<String> = f
                        .states
                        .iter()
                        .filter(|(_, s)| s.machine == pose)
                        .map(|(k, _)| k.clone())
                        .collect();
                    remove(f, &inner, &BTreeSet::new());
                }
            }
            AnimRow::BlendParam | AnimRow::BlendParamY => {
                if let Some(n) = state.as_deref().and_then(|s| Self::anim_pose_mut(f, s)) {
                    if row == AnimRow::BlendParam {
                        n.param = picked;
                    } else {
                        n.param_y = picked;
                    }
                }
            }
            AnimRow::ParamKind(_) => {
                if let Some(p) = param.as_deref().and_then(|n| f.params.get_mut(n)) {
                    p.kind = kind_of(&picked);
                    p.value = match p.kind {
                        ParamKind::Float => p.value,
                        ParamKind::Int => p.value.round(),
                        ParamKind::Bool => f32::from(u8::from(p.value != 0.0)),
                        ParamKind::Trigger => 0.0,
                    };
                }
            }
            AnimRow::AddParam => {
                let kind = kind_of(&picked);
                let name = fresh_param(f, kind);
                f.params.insert(name, ParamFile { kind, value: 0.0 });
            }
            AnimRow::LayerBlend => {
                if let Some(l) = f.layers.get_mut(&layer) {
                    l.blend = if picked == "Additive" {
                        LayerBlend::Additive
                    } else {
                        LayerBlend::Normal
                    };
                }
            }
            AnimRow::LayerWeightParam => {
                if let Some(l) = f.layers.get_mut(&layer) {
                    if i == 0 {
                        l.weight_param.clear();
                    } else {
                        l.weight_param = picked;
                    }
                }
            }
            _ => {}
        })
    }

    /// Row `row` back to its default: no clip, a pace of 1, no blend parameter, a clip at
    /// the start of its blend, no condition, a quarter second's blend, priority 0, no exit
    /// time nor offset, both switches off, a number, a starting value of 0.
    pub(crate) fn anim_reset(&mut self, row: AnimRow) -> Option<(String, String)> {
        let anim = self.anim.as_deref()?;
        let state = anim.inspected_state().map(str::to_owned);
        let transition = anim.inspected_transition().map(str::to_owned);
        let param = match row {
            AnimRow::ParamKind(k) | AnimRow::ParamValue(k) => {
                anim.param(k).map(|(n, _)| n.to_owned())
            }
            _ => None,
        };
        let defaults = TransitionFile::default();
        let layer = anim.layer.clone();
        let joint = anim
            .mask_joint()
            .map(|j| (anim.joints.clone(), anim.joint_parents.clone(), j));
        self.edit_anim(|f| {
            if let Some(l) = f.layers.get_mut(&layer) {
                match row {
                    AnimRow::LayerBlend => l.blend = LayerBlend::Normal,
                    AnimRow::LayerWeight => l.weight = 1.0,
                    AnimRow::LayerWeightParam => l.weight_param.clear(),
                    AnimRow::MaskWeight => {
                        if let Some((names, parents, j)) = &joint {
                            reset_mask_weight(&mut l.mask, names, parents, *j);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(n) = state.as_deref().and_then(|s| Self::anim_pose_mut(f, s)) {
                match row {
                    AnimRow::Clip => n.clip.clear(),
                    AnimRow::Speed => n.speed = 1.0,
                    AnimRow::BlendParam => n.param.clear(),
                    AnimRow::BlendParamY => n.param_y.clear(),
                    AnimRow::PointAt(k) => {
                        if let Some(p) = n.points.get_mut(k) {
                            p.at = [0.0; 2];
                        }
                    }
                    _ => {}
                }
            }
            if let Some(t) = transition.as_deref().and_then(|t| f.transitions.get_mut(t)) {
                match row {
                    AnimRow::When => t.when.clear(),
                    AnimRow::Blend => t.blend = defaults.blend,
                    AnimRow::Priority => t.priority = defaults.priority,
                    AnimRow::Exit => t.exit = defaults.exit,
                    AnimRow::Offset => t.offset = defaults.offset,
                    AnimRow::KeepPhase => t.keep_phase = false,
                    AnimRow::Inertialize => t.inertialize = false,
                    _ => {}
                }
            }
            if let Some(p) = param.as_deref().and_then(|n| f.params.get_mut(n)) {
                match row {
                    AnimRow::ParamKind(_) => p.kind = ParamKind::Float,
                    AnimRow::ParamValue(_) => p.value = 0.0,
                    _ => {}
                }
            }
        })
    }

    /// Joint `j`'s check box in the shown layer's mask clicked (ADR 0124's E12-5d-2b, the
    /// team's а): its subtree turned in or out ([`toggle_mask`]). Its step's texts, when it
    /// changed.
    pub(crate) fn toggle_mask_joint(&mut self, j: usize) -> Option<(String, String)> {
        let anim = self.anim.as_deref()?;
        let (layer, names, parents) = (
            anim.layer.clone(),
            anim.joints.clone(),
            anim.joint_parents.clone(),
        );
        if j >= names.len() {
            return None;
        }
        self.edit_anim(|f| {
            if let Some(l) = f.layers.get_mut(&layer) {
                toggle_mask(&mut l.mask, &names, &parents, j);
            }
        })
    }

    /// State `state`'s machine entered (Tab or a double-click on a state playing a machine;
    /// ADR 0124's E12-5d-2c): the canvas shows its states, nothing selected, framed anew.
    /// False for a state playing no machine. Not a step: what the tab shows.
    pub(crate) fn enter_machine(&mut self, state: &str) -> bool {
        let Some(anim) = self.anim.as_deref_mut() else {
            return false;
        };
        let Some(pose) = anim.file.states.get(state).map(|s| s.pose.clone()) else {
            return false;
        };
        if !is_machine(&anim.file, &pose) {
            return false;
        }
        anim.path.push(pose);
        anim.clear_selection();
        anim.refresh();
        anim.laid_out = None;
        self.framed = false;
        true
    }

    /// Back out to the machine `level` entries down (0: the shown layer's own; the bar's
    /// crumbs, Tab with nothing selected one level; ADR 0124's E12-5d-2c), the state that
    /// plays the machine left selected. False where it is that deep already.
    pub(crate) fn leave_machine(&mut self, level: usize) -> bool {
        let Some(anim) = self.anim.as_deref_mut() else {
            return false;
        };
        if level >= anim.path.len() {
            return false;
        }
        let left = anim.path[level].clone();
        anim.path.truncate(level);
        anim.clear_selection();
        anim.refresh();
        let machine = anim.machine.clone();
        if let Some(id) = anim
            .file
            .states
            .iter()
            .find(|(_, s)| s.machine == machine && s.pose == left)
            .map(|(id, _)| id.clone())
        {
            anim.selected.insert(id);
        }
        anim.laid_out = None;
        self.framed = false;
        true
    }

    /// Tab on an animation graph's canvas (ADR 0124's E12-5d-2c): into the selected state's
    /// machine, or out one level with no machine state selected. What it did, to say.
    pub(crate) fn anim_tab(&mut self) -> Result<(), &'static str> {
        let Some(anim) = self.anim.as_deref() else {
            return Err("");
        };
        let one = (anim.selected.len() == 1)
            .then(|| anim.selected.iter().next().cloned())
            .flatten();
        if let Some(id) = one
            && self.enter_machine(&id)
        {
            return Ok(());
        }
        let depth = self.anim.as_deref().map_or(0, |a| a.path.len());
        if depth > 0 && self.leave_machine(depth - 1) {
            return Ok(());
        }
        Err("Tab enters a state playing a machine (its pose a Machine), and comes back out")
    }

    /// Layer `k` (in their order) shown on the canvas (ADR 0124's E12-5d-2b): its machine,
    /// nothing selected, framed anew. Not a step: what the tab shows.
    pub(crate) fn show_layer(&mut self, k: usize) {
        let Some(anim) = self.anim.as_deref_mut() else {
            return;
        };
        let Some(id) = anim.layer_ids.get(k).cloned() else {
            return;
        };
        // The shown layer's chip again: out of its machines (E12-5d-2c).
        if anim.layer == id && anim.path.is_empty() {
            return;
        }
        anim.path.clear();
        anim.layer = id;
        anim.clear_selection();
        anim.refresh();
        anim.laid_out = None;
        self.framed = false;
    }

    /// A layer added over the others (the bar's +; ADR 0124's E12-5d-2b): `Layer` (numbered
    /// when taken), playing a machine of its own whose entry state, Idle, plays a clip not
    /// named yet; shown. A step.
    pub(crate) fn add_layer(&mut self) -> Option<Outcome> {
        let mut made = String::new();
        let (before, after) = self.edit_anim(|f| made = insert_layer(f))?;
        let anim = self.anim.as_deref_mut()?;
        anim.layer = made;
        anim.clear_selection();
        anim.refresh();
        anim.laid_out = None;
        self.framed = false;
        Some(Outcome::Edited {
            name: "Add Layer",
            before,
            after,
        })
    }

    /// The layer shown deleted, its section's × (ADR 0124's E12-5d-2b): with what only it
    /// plays (its machine's states, their poses and transitions); the first left shown.
    pub(crate) fn delete_layer(&mut self) -> Option<(String, String)> {
        let id = self.anim.as_deref()?.layer.clone();
        let edit = self.edit_anim(|f| {
            if let Some(layer) = f.layers.remove(&id) {
                remove_tree(f, &layer.root);
            }
        });
        if let Some(anim) = self.anim.as_deref_mut() {
            anim.clear_selection();
            anim.laid_out = None;
        }
        self.framed = false;
        edit
    }

    /// The layer shown moved one place earlier (`true`: under the one before it) or later
    /// among the layers, its section's ↑ or ↓ (ADR 0124's E12-5d-2b): every layer's order
    /// numbered again from 0. A step; nothing at either end.
    pub(crate) fn move_layer(&mut self, earlier: bool) -> Option<(String, String)> {
        let anim = self.anim.as_deref()?;
        let k = anim.layer_index()?;
        let j = if earlier { k.checked_sub(1)? } else { k + 1 };
        if j >= anim.layer_ids.len() {
            return None;
        }
        let mut ids = anim.layer_ids.clone();
        ids.swap(k, j);
        self.edit_anim(|f| {
            for (n, id) in ids.iter().enumerate() {
                if let Some(l) = f.layers.get_mut(id) {
                    l.order = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
        })
    }

    /// Parameter `k` (in their names' order) deleted, its section's ×: the conditions naming
    /// it say so.
    pub(crate) fn delete_param(&mut self, k: usize) -> Option<(String, String)> {
        let name = self.anim.as_deref()?.param(k)?.0.to_owned();
        self.edit_anim(|f| {
            f.params.remove(&name);
        })
    }
}

/// An arrow from `a` to `b` (panel pixels): its line and its head at `b`, filled.
fn arrow(canvas: &mut Canvas, a: Vec2, b: Vec2, width: f32, z: f32, color: Vec4, order: Order) {
    canvas.draw(&Quad::line(a, b, width).color(color).order(order));
    head(canvas, a, b, width, z, color, order);
}

/// An arrow's head at `tip`, pointing away from `back`: its two edges and strokes across
/// between them, so it reads filled.
fn head(canvas: &mut Canvas, back: Vec2, tip: Vec2, width: f32, z: f32, color: Vec4, order: Order) {
    let dir = (tip - back).normalize_or_zero();
    if dir == Vec2::ZERO {
        return;
    }
    let side = dir.perp();
    let (length, half) = (HEAD[0] * z, HEAD[1] * z);
    let base = tip - dir * length;
    for s in [-1.0, 1.0] {
        canvas.draw(
            &Quad::line(tip, base + side * half * s, width)
                .color(color)
                .order(order),
        );
    }
    for f in [0.2, 0.45, 0.7] {
        let middle = base + dir * length * f;
        let across = side * half * (1.0 - f);
        canvas.draw(
            &Quad::line(middle - across, middle + across, length * 0.3)
                .color(color)
                .order(order),
        );
    }
}

/// Draws `doc`'s animation graph `anim` under the panels, as a material graph's canvas is
/// drawn (layer −1): the grid, the transitions' arrows (each one's condition or problem by
/// it while lit), the entry's tag, the Any State box, the states' boxes (a problem's why
/// under its box), then the arrow or the box being dragged.
pub(crate) fn draw(
    doc: &GraphDoc,
    anim: &AnimDoc,
    drag: Option<&Drag>,
    look: &Look<'_>,
    text: &mut UiText,
    canvas: &mut Canvas,
) {
    let [rx, ry, rw, rh] = look.rect.map(|v| v as f32);
    graph::draw_back(&doc.view, look, canvas);
    let order = graph::CANVAS_ORDER;
    let z = doc.view.zoom() * look.scale;
    let origin = [rx / look.scale, ry / look.scale];
    let to = |c: [f32; 2]| {
        let q = doc.view.panel_point(c, origin);
        Vec2::new(q[0] * look.scale, q[1] * look.scale)
    };
    let p = look.palette;
    let (title_size, label_size) = (TITLE_TEXT * z, PORT_TEXT * z);
    let error = Vec4::from_array(p.error);
    let text_color = Vec4::from_array(p.text);
    let dim = Vec4::from_array(p.text_dim);
    let shown = |lo: Vec2, hi: Vec2| !(hi.x < rx || hi.y < ry || lo.x > rx + rw || lo.y > ry + rh);
    let solid = |canvas: &mut Canvas, lo: Vec2, hi: Vec2, color: Vec4| {
        canvas.draw(
            &Quad::solid((lo + hi) * 0.5, hi - lo)
                .color(color)
                .order(order),
        );
    };
    // In Play (ADR 0124's E12-5e), the transition the watched character fades through: the
    // arrow from the state it fades from, else from Any State, into the state it is in.
    let live = &anim.live;
    let playing = Vec4::from_array(p.playing);
    let live_arrow = if live.on && !live.from.is_empty() {
        let into = |a: &Arrow, from: Option<&str>| {
            anim.file.transitions.get(&a.id).is_some_and(|t| {
                state_name(&anim.file, &t.to) == live.state
                    && match from {
                        Some(from) => !t.from.is_empty() && state_name(&anim.file, &t.from) == from,
                        None => t.from.is_empty(),
                    }
            })
        };
        let from = Some(live.from.as_str());
        (anim.arrows.iter().position(|a| into(a, from)))
            .or_else(|| anim.arrows.iter().position(|a| into(a, None)))
    } else {
        None
    };
    // The transitions, under the boxes: red with why where they do not hold together, the
    // selected orange, those of the selected states and under the pointer brighter.
    for (k, a) in anim.arrows.iter().enumerate() {
        let t = anim.file.transitions.get(&a.id);
        let selected = anim.transitions.contains(&a.id);
        let hovered = anim.hover == Hit::Arrow(k);
        let touched =
            t.is_some_and(|t| anim.selected.contains(&t.to) || anim.selected.contains(&t.from));
        let why = anim.transition_mark(&a.id);
        let lit = selected || hovered || touched;
        let lived = live_arrow == Some(k);
        let color = if why.is_some() {
            error
        } else if selected {
            SELECTED
        } else if lived {
            playing
        } else {
            Vec4::new(1.0, 1.0, 1.0, if lit { 0.9 } else { 0.55 })
        };
        let width = if lit || lived { 2.5 } else { 1.75 } * look.scale;
        // How far the fade is: a dot along the arrow.
        if lived && !a.looped {
            let (from, to_) = (to(a.from), to(a.to));
            let dot = from + (to_ - from) * live.faded.clamp(0.0, 1.0);
            let d = 4.0 * look.scale;
            canvas.draw(
                &Quad::rounded_rect(dot, Vec2::splat(d * 2.0), d)
                    .color(playing)
                    .order(order),
            );
        }
        if a.looped {
            let [q0, q1, q2, q3] = a.loop_points().map(to);
            for (u, v) in [(q0, q1), (q1, q2)] {
                canvas.draw(&Quad::line(u, v, width).color(color).order(order));
            }
            arrow(canvas, q2, q3, width, z, color, order);
        } else {
            arrow(canvas, to(a.from), to(a.to), width, z, color, order);
        }
    }
    // The entry: a tag left of its state, an arrow from it into the box.
    if let Some(b) = anim.boxes.iter().find(|b| b.entry) {
        let middle = b.at[1] + b.size[1] * 0.5;
        let right = b.at[0] - ENTRY_GAP;
        let (lo, hi) = (
            to([right - ENTRY_TAG[0], middle - ENTRY_TAG[1] * 0.5]),
            to([right, middle + ENTRY_TAG[1] * 0.5]),
        );
        if shown(lo, to([b.at[0], middle])) {
            canvas.draw(
                &Quad::rounded_rect((lo + hi) * 0.5, hi - lo, (hi.y - lo.y) * 0.5)
                    .color(ENTRY_COLOR)
                    .order(order),
            );
            if label_size >= 6.0 {
                text.draw_centered(
                    canvas,
                    look.glyphs,
                    "Entry",
                    label_size,
                    (lo + hi) * 0.5,
                    text_color,
                    order,
                );
            }
            arrow(
                canvas,
                to([right, middle]),
                to([b.at[0], middle]),
                1.75 * look.scale,
                z,
                Vec4::new(1.0, 1.0, 1.0, 0.7),
                order,
            );
        }
    }
    // The Any State box.
    if let Some(r) = anim.any_rect() {
        let (lo, hi) = (to(r[0]), to(r[1]));
        if shown(lo, hi) {
            canvas.draw(
                &Quad::rounded_rect((lo + hi) * 0.5, hi - lo, RADIUS * z)
                    .color(ANY_COLOR)
                    .order(order),
            );
            if title_size >= 5.0 {
                text.draw_centered(
                    canvas,
                    look.glyphs,
                    "Any State",
                    title_size,
                    (lo + hi) * 0.5,
                    text_color,
                    order,
                );
            }
            let frame = match anim.hover {
                _ if anim.any => Some((SELECTED, 1.5)),
                Hit::AnyEdge => Some((EDGE_COLOR, 2.5)),
                Hit::Any => Some((HOVERED, 1.5)),
                _ => None,
            };
            if let Some((color, thickness)) = frame {
                canvas.draw(
                    &Quad::rounded_rect((lo + hi) * 0.5, hi - lo, RADIUS * z)
                        .outline(thickness * look.scale)
                        .color(color)
                        .order(order),
                );
            }
        }
    }
    // The states: a body, a title bar (amber for the entry), the name, what the pose plays.
    for k in anim.order() {
        let b = &anim.boxes[k];
        let (lo, hi) = (to(b.at), to([b.at[0] + b.size[0], b.at[1] + b.size[1]]));
        if !shown(lo, hi) {
            continue;
        }
        let size = hi - lo;
        let radius = RADIUS * z;
        canvas.draw(
            &Quad::rounded_rect((lo + hi) * 0.5, size, radius)
                .color(BODY)
                .order(order),
        );
        let bar = if b.entry {
            ENTRY_TITLE
        } else if b.machine {
            MACHINE_TITLE
        } else {
            STATE_TITLE
        };
        let bar_hi = Vec2::new(hi.x, lo.y + TITLE * z);
        canvas.draw(
            &Quad::rounded_rect((lo + bar_hi) * 0.5, bar_hi - lo, radius)
                .color(bar)
                .order(order),
        );
        solid(canvas, Vec2::new(lo.x, bar_hi.y - radius), bar_hi, bar);
        if title_size >= 5.0 {
            text.draw_left(
                canvas,
                look.glyphs,
                &b.title,
                title_size,
                Vec2::new(lo.x + PAD * z, lo.y + TITLE * 0.5 * z),
                text_color,
                order,
            );
        }
        if label_size >= 6.0 {
            text.draw_left(
                canvas,
                look.glyphs,
                &b.summary,
                label_size,
                Vec2::new(lo.x + PAD * z, lo.y + (TITLE + ROW * 0.5) * z),
                dim,
                order,
            );
        }
        let why = anim.state_mark(&b.id);
        let frame = if why.is_some() {
            Some((error, 1.5))
        } else if anim.hover == Hit::Edge(k) {
            Some((EDGE_COLOR, 2.5))
        } else if anim.selected.contains(&b.id) {
            Some((SELECTED, 1.5))
        } else if anim.hover == Hit::State(k) {
            Some((HOVERED, 1.5))
        } else {
            None
        };
        if let Some((color, thickness)) = frame {
            canvas.draw(
                &Quad::rounded_rect((lo + hi) * 0.5, size, radius)
                    .outline(thickness * look.scale)
                    .color(color)
                    .order(order),
            );
        }
        // In Play (ADR 0124's E12-5e): the watched character's state ringed in Play's
        // colour, a line under its title for how far its pose has played; the state it
        // fades from ringed fainter as the fade goes on.
        if live.on {
            let now = b.title == live.state;
            let alpha = if now {
                1.0
            } else if !live.from.is_empty() && b.title == live.from {
                (1.0 - live.faded).max(0.25)
            } else {
                0.0
            };
            if alpha > 0.0 {
                let grow = Vec2::splat(4.0 * look.scale);
                canvas.draw(
                    &Quad::rounded_rect((lo + hi) * 0.5, size + grow, radius + grow.x * 0.5)
                        .outline(2.0 * look.scale)
                        .color(playing.with_w(playing.w * alpha))
                        .order(order),
                );
            }
            let played = (hi.x - lo.x) * live.progress.clamp(0.0, 1.0);
            if now && played >= 0.5 {
                let y = lo.y + TITLE * z;
                solid(
                    canvas,
                    Vec2::new(lo.x, y - 3.0 * look.scale),
                    Vec2::new(lo.x + played, y),
                    playing,
                );
            }
        }
        // Selected with a problem: its orange inside the red.
        if why.is_some() && anim.selected.contains(&b.id) {
            let inset = Vec2::splat(2.5 * look.scale);
            canvas.draw(
                &Quad::rounded_rect((lo + hi) * 0.5, size - inset * 2.0, radius)
                    .outline(1.5 * look.scale)
                    .color(SELECTED)
                    .order(order),
            );
        }
        if let Some(why) = why
            && label_size >= 5.0
        {
            text.draw_left(
                canvas,
                look.glyphs,
                why,
                label_size,
                Vec2::new(lo.x, hi.y + ROW * 0.5 * z),
                error,
                order,
            );
        }
    }
    // Over the boxes, each arrow's label: its problem always, its condition (or what makes
    // it go) while it is selected or under the pointer.
    for (k, a) in anim.arrows.iter().enumerate() {
        if label_size < 6.0 {
            break;
        }
        let why = anim.transition_mark(&a.id);
        let lit = anim.transitions.contains(&a.id) || anim.hover == Hit::Arrow(k);
        let label = why.or_else(|| {
            let t = anim.file.transitions.get(&a.id)?;
            lit.then_some(if !t.when.is_empty() {
                t.when.as_str()
            } else if t.exit > 0.0 {
                "at its exit time"
            } else {
                "at once"
            })
        });
        if let Some(label) = label {
            let at = to(a.middle()) + Vec2::new(6.0 * z, -8.0 * z);
            let color = if why.is_some() { error } else { text_color };
            text.draw_left(canvas, look.glyphs, label, label_size, at, color, order);
        }
    }
    // The transition being drawn: from its box's edge to the pointer, or to the edge of the
    // state it would go to.
    if let Some(Drag::Link { from }) = drag
        && let Some(cursor) = doc.cursor
    {
        let start = match from {
            Source::Any => anim.any_rect(),
            Source::State(id) => anim.box_of(id).map(|k| anim.boxes[k].rect()),
        };
        let end = match anim.hover {
            Hit::State(k) | Hit::Edge(k) => anim.boxes[k].rect(),
            _ => [cursor, cursor],
        };
        if let Some(start) = start
            && let Some((a, b)) = edge_points(start, end, 0.0)
        {
            arrow(
                canvas,
                to(a),
                to(b),
                2.0 * look.scale,
                z,
                Vec4::new(1.0, 1.0, 1.0, 0.85),
                order,
            );
        }
    }
    if let (Some(Drag::Select { from, .. }), Some(cursor)) = (drag, doc.cursor) {
        graph::draw_select_box(&doc.view, look, *from, cursor, canvas);
    }
    canvas.set_clip(None);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use engine_scene::AssetPaths;

    /// Seven pixels a character, as `FixedMeasure` does at 14 pixels.
    fn measure(s: &str, size: f32) -> f32 {
        s.chars().count() as f32 * size * 0.5
    }

    /// Idle (the entry) and Walk on machine `m`, a walk transition and back, a speed and a
    /// jump trigger.
    fn file() -> AnimGraphFile {
        let mut f = AnimGraphFile::default();
        f.params.insert(String::from("speed"), ParamFile::default());
        f.params.insert(
            String::from("jump"),
            ParamFile {
                kind: ParamKind::Trigger,
                value: 0.0,
            },
        );
        f.layers.insert(
            String::from("base"),
            LayerFile {
                name: String::from("Base"),
                root: String::from("m"),
                ..LayerFile::default()
            },
        );
        f.nodes.insert(
            String::from("m"),
            NodeFile {
                kind: NodeKind::Machine,
                entry: String::from("idle"),
                at: [-200.0, -100.0],
                ..NodeFile::default()
            },
        );
        for (id, clip) in [("ci", "Idle"), ("cw", "Walk")] {
            f.nodes.insert(
                String::from(id),
                NodeFile {
                    clip: String::from(clip),
                    ..NodeFile::default()
                },
            );
        }
        for (id, name, pose, at) in [
            ("idle", "Idle", "ci", [0.0, 0.0]),
            ("walk", "Walk", "cw", [300.0, 0.0]),
        ] {
            f.states.insert(
                String::from(id),
                StateFile {
                    name: String::from(name),
                    machine: String::from("m"),
                    pose: String::from(pose),
                    at,
                },
            );
        }
        for (id, from, to, when) in [
            ("t1", "idle", "walk", "speed > 0.1"),
            ("t2", "walk", "idle", "speed <= 0.1"),
        ] {
            f.transitions.insert(
                String::from(id),
                TransitionFile {
                    from: String::from(from),
                    to: String::from(to),
                    when: String::from(when),
                    ..TransitionFile::default()
                },
            );
        }
        f
    }

    fn doc() -> GraphDoc {
        let mut d = GraphDoc::new_anim(
            AssetId::new(),
            "anim/hero.agraph",
            file(),
            AssetPaths::new(),
        );
        d.relayout(&mut measure);
        d
    }

    fn anim(d: &GraphDoc) -> &AnimDoc {
        d.anim.as_deref().unwrap()
    }

    fn anim_mut(d: &mut GraphDoc) -> &mut AnimDoc {
        d.anim.as_deref_mut().unwrap()
    }

    /// A frame of the pointer at canvas point `c` (the panel at the origin, zoom 1).
    fn at(
        d: &mut GraphDoc,
        drag: &mut Option<Drag>,
        c: [f32; 2],
        press: bool,
        held: bool,
        ctrl: bool,
    ) -> Option<Outcome> {
        let q = d.view.panel_point(c, [0.0; 2]);
        let p = Pointer {
            at: Some(q),
            over: true,
            press,
            held,
            ctrl,
            ..Pointer::default()
        };
        let outcome = d.anim_pointer(&p, [0.0; 2], drag);
        d.relayout(&mut measure);
        outcome
    }

    /// The edit's texts, or a panic with what came instead.
    fn edited(outcome: Option<Outcome>) -> (&'static str, String, String) {
        match outcome {
            Some(Outcome::Edited {
                name,
                before,
                after,
            }) => (name, before, after),
            other => panic!("not an edit: {other:?}"),
        }
    }

    /// The text written back and read again is the file the document holds.
    fn round_trip(d: &GraphDoc) {
        let (back, report) = AnimGraphFile::parse(&d.text).unwrap();
        assert_eq!(report.id, Some(d.id));
        assert_eq!(back, anim(d).file, "{}", d.text);
        assert_eq!(back.to_text(d.id, None), d.text);
    }

    #[test]
    fn a_graph_file_opens_as_an_animation_graph_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("agraph_doc_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("anim")).unwrap();
        let d = doc();
        std::fs::write(dir.join("anim/hero.agraph"), &d.text).unwrap();
        let opened = GraphDoc::open(&dir, "anim/hero.agraph").unwrap();
        assert_eq!(opened.id, d.id);
        assert_eq!(anim(&opened).file, file());
        assert!(!opened.dirty() && opened.graph.nodes.is_empty());
        assert!(is_anim_graph("anim/hero.agraph") && !is_anim_graph("glow.mgraph"));
        round_trip(&opened);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The machine's states as boxes with their poses, the entry marked, Any State at the
    /// machine's place, arrows edge to edge, the pair side by side.
    #[test]
    fn states_are_laid_out_with_their_arrows_side_by_side() {
        let d = doc();
        let a = anim(&d);
        assert_eq!(a.machine, "m");
        assert_eq!(a.boxes.len(), 2);
        let (idle, walk) = (&a.boxes[0], &a.boxes[1]);
        assert_eq!(
            (idle.title.as_str(), idle.summary.as_str()),
            ("Idle", "Idle")
        );
        assert!(idle.entry && !walk.entry);
        assert_eq!(idle.size, [STATE_WIDTH, STATE_HEIGHT]);
        assert_eq!(a.any_rect().unwrap()[0], [-200.0, -100.0]);
        assert_eq!(a.arrows.len(), 2);
        let (t1, t2) = (&a.arrows[0], &a.arrows[1]);
        // Edge to edge: Idle's right edge to Walk's left one.
        let near = |a: f32, b: f32| (a - b).abs() < 1e-3;
        assert!(near(t1.from[0], idle.at[0] + idle.size[0]), "{t1:?}");
        assert!(near(t1.to[0], walk.at[0]), "{t1:?}");
        assert!(near(t2.from[0], walk.at[0]), "{t2:?}");
        // Side by side: half a lane each side of the middles' line.
        let middle = STATE_HEIGHT * 0.5;
        assert!((t1.from[1] - (middle + LANE * 0.5)).abs() < 1e-3, "{t1:?}");
        assert!((t2.from[1] - (middle - LANE * 0.5)).abs() < 1e-3, "{t2:?}");
        // A blend says its parameter and clips; a long one is cut.
        let mut f = file();
        f.nodes.insert(
            String::from("b"),
            NodeFile {
                kind: NodeKind::Blend1d,
                param: String::from("speed"),
                points: vec![
                    BlendPoint {
                        at: [0.0, 0.0],
                        clip: String::from("Walk"),
                    },
                    BlendPoint {
                        at: [3.0, 0.0],
                        clip: String::from("Run"),
                    },
                ],
                ..NodeFile::default()
            },
        );
        let mut s = String::new();
        pose_summary(&f, "b", &mut s);
        assert_eq!(s, "Blend1d speed: Walk \u{b7} Run");
        f.nodes.get_mut("b").unwrap().points[1].clip = "R".repeat(60);
        pose_summary(&f, "b", &mut s);
        assert_eq!(s.chars().count(), SUMMARY_MAX + 1);
        assert!(s.ends_with('\u{2026}'));
    }

    /// A press on a box's inside selects and moves it (whole units, one step; Escape takes a
    /// move back); on its edge, or with Ctrl, it draws a transition; a click on an arrow
    /// selects it; a box on the canvas selects what it meets.
    #[test]
    fn the_pointer_moves_connects_selects_and_takes_back() {
        let mut d = doc();
        let mut drag = None;
        // A move of Walk.
        let grab = [350.0, 30.0];
        assert_eq!(at(&mut d, &mut drag, grab, true, true, false), None);
        assert_eq!(anim(&d).selected.iter().collect::<Vec<_>>(), ["walk"]);
        at(&mut d, &mut drag, [390.4, 60.6], false, true, false);
        let (name, before, after) =
            edited(at(&mut d, &mut drag, [390.4, 60.6], false, false, false));
        assert_eq!(name, "Move State");
        assert_eq!(anim(&d).file.states["walk"].at, [340.0, 31.0]);
        assert!(after.contains("states[\"walk\"].at = [340, 31]"), "{after}");
        d.put_text(&before).unwrap();
        assert_eq!(anim(&d).file.states["walk"].at, [300.0, 0.0]);
        d.put_text(&after).unwrap();
        d.relayout(&mut measure);
        // Escape takes a move back.
        at(&mut d, &mut drag, [370.0, 40.0], true, true, false);
        at(&mut d, &mut drag, [470.0, 40.0], false, true, false);
        assert_eq!(anim(&d).file.states["walk"].at, [440.0, 31.0]);
        let escape = Pointer {
            escape: true,
            ..Pointer::default()
        };
        assert_eq!(d.anim_pointer(&escape, [0.0; 2], &mut drag), None);
        assert_eq!(anim(&d).file.states["walk"].at, [340.0, 31.0]);
        d.relayout(&mut measure);
        // From Idle's edge onto Walk: a transition, selected, with the file's defaults.
        let edge = [STATE_WIDTH - 1.0, 40.0];
        at(&mut d, &mut drag, edge, true, true, false);
        assert!(matches!(&drag, Some(Drag::Link { from: Source::State(s) }) if s == "idle"));
        at(&mut d, &mut drag, [380.0, 50.0], false, true, false);
        let (name, ..) = edited(at(&mut d, &mut drag, [380.0, 50.0], false, false, false));
        assert_eq!(name, "Add Transition");
        let t = anim(&d).transitions.iter().next().unwrap().clone();
        let made = &anim(&d).file.transitions[&t];
        assert_eq!((made.from.as_str(), made.to.as_str()), ("idle", "walk"));
        assert_eq!(made.blend, 0.25);
        assert!(made.inertialize, "drawn transitions inertialize (team, 2a)");
        assert!(
            t.len() == 4 && t.chars().all(|c| c.is_ascii_hexdigit()),
            "{t}"
        );
        // It has no condition yet: a problem on it, said.
        assert!(anim(&d).transition_mark(&t).unwrap().contains("at once"));
        // Ctrl+drag from Walk's inside onto Idle; into itself is refused.
        at(&mut d, &mut drag, [380.0, 60.0], true, true, true);
        assert!(matches!(&drag, Some(Drag::Link { .. })));
        assert_eq!(
            at(&mut d, &mut drag, [390.0, 60.0], false, false, false),
            Some(Outcome::Refused("A state does not go into itself"))
        );
        at(&mut d, &mut drag, [380.0, 60.0], true, true, true);
        let (name, ..) = edited(at(&mut d, &mut drag, [60.0, 30.0], false, false, false));
        assert_eq!(name, "Add Transition");
        assert_eq!(anim(&d).file.transitions.len(), 4);
        // Dropped on the empty canvas: a state there, the transition into it.
        at(&mut d, &mut drag, edge, true, true, false);
        let (name, ..) = edited(at(&mut d, &mut drag, [100.0, 300.0], false, false, false));
        assert_eq!(name, "Add State");
        let new = anim(&d).selected.iter().next().unwrap().clone();
        let state = &anim(&d).file.states[&new];
        assert_eq!(state.name, "New State");
        assert_eq!(state.at, [30.0, 273.0], "its middle where it was dropped");
        assert!(
            anim(&d)
                .file
                .transitions
                .values()
                .any(|t| t.from == "idle" && t.to == new && t.inertialize)
        );
        // A click on an arrow selects it alone.
        d.relayout(&mut measure);
        let middle = anim(&d).arrows[0].middle();
        assert_eq!(at(&mut d, &mut drag, middle, true, true, false), None);
        assert!(drag.is_none());
        assert_eq!(anim(&d).transitions.len(), 1);
        assert!(anim(&d).selected.is_empty());
        // A box on the canvas over both states selects them.
        at(&mut d, &mut drag, [-60.0, -20.0], true, true, false);
        at(&mut d, &mut drag, [600.0, 120.0], false, true, false);
        at(&mut d, &mut drag, [600.0, 120.0], false, false, false);
        assert_eq!(anim(&d).selected.len(), 2);
        assert!(anim(&d).transitions.is_empty());
        round_trip(&d);
    }

    /// Delete takes the selected states with every transition from or into them and their
    /// poses; the entry moves to a state left; undone, all comes back. Any State stays.
    #[test]
    fn deleting_takes_transitions_and_poses_and_undo_puts_them_back() {
        let mut d = doc();
        anim_mut(&mut d).any = true;
        assert_eq!(
            d.delete_anim_selection(),
            Some(Outcome::Refused(
                "Any State stays: the transitions from every state start there"
            ))
        );
        anim_mut(&mut d).clear_selection();
        anim_mut(&mut d).selected.insert(String::from("idle"));
        let (name, before, after) = edited(d.delete_anim_selection());
        assert_eq!(name, "Delete State");
        let f = &anim(&d).file;
        assert!(!f.states.contains_key("idle") && !f.nodes.contains_key("ci"));
        assert!(f.transitions.is_empty(), "both went with it");
        assert_eq!(
            f.nodes["m"].entry, "walk",
            "the machine starts at what is left"
        );
        d.put_text(&before).unwrap();
        assert_eq!(anim(&d).file, file());
        d.put_text(&after).unwrap();
        d.put_text(&before).unwrap();
        // A transition alone.
        anim_mut(&mut d).transitions.insert(String::from("t2"));
        let (name, ..) = edited(d.delete_anim_selection());
        assert_eq!(name, "Delete Transition");
        assert!(!anim(&d).file.transitions.contains_key("t2"));
        assert!(anim(&d).file.states.contains_key("walk"));
        round_trip(&d);
    }

    /// The entry set by the inspector's switch (refused switched off), added states, the
    /// search's kinds, a graph with no machine given one.
    #[test]
    fn the_entry_is_set_and_states_are_added() {
        let mut d = doc();
        anim_mut(&mut d).selected.insert(String::from("walk"));
        assert_eq!(anim(&d).look(AnimRow::Entry), RowLook::Check(false));
        let Ok(Some(Outcome::Edited { name, before, .. })) = d.anim_toggle(AnimRow::Entry) else {
            panic!("the entry set")
        };
        assert_eq!(name, "Set Entry State");
        assert_eq!(anim(&d).file.nodes["m"].entry, "walk");
        assert_eq!(
            d.anim_toggle(AnimRow::Entry),
            Err("A machine starts at one of its states: make another its entry")
        );
        d.put_text(&before).unwrap();
        assert_eq!(anim(&d).file.nodes["m"].entry, "idle");
        // A blend state: the graph's first number its parameter; named apart.
        let (name, ..) = edited(d.add_state(StateKind::Blend1d, [100.0, 200.0], None));
        assert_eq!(name, "Add State");
        let id = anim(&d).selected.iter().next().unwrap().clone();
        let s = anim(&d).file.states[&id].clone();
        let pose = &anim(&d).file.nodes[&s.pose];
        assert_eq!(
            (pose.kind, pose.param.as_str()),
            (NodeKind::Blend1d, "speed")
        );
        edited(d.add_state(StateKind::Clip, [400.0, 200.0], None));
        let id = anim(&d).selected.iter().next().unwrap().clone();
        assert_eq!(anim(&d).file.states[&id].name, "New State 2");
        assert_eq!(anim(&d).file.nodes["m"].entry, "idle", "the entry stays");
        // A graph with no machine: a base layer and a machine made, the state its entry.
        let mut empty = GraphDoc::new_anim(
            AssetId::new(),
            "empty.agraph",
            AnimGraphFile::default(),
            AssetPaths::new(),
        );
        assert!(anim(&empty).machine.is_empty());
        edited(empty.add_state(StateKind::Clip, [0.0, 0.0], None));
        let a = anim(&empty);
        let id = a.selected.iter().next().unwrap();
        assert!(!a.machine.is_empty());
        assert_eq!(a.file.nodes[&a.machine].entry, *id);
        assert_eq!(a.file.layers.values().next().unwrap().name, "Base");
        round_trip(&empty);
        // A new graph: Idle, the entry, plays no clip yet (a problem on it).
        let start = GraphDoc::new_anim(AssetId::new(), "new.agraph", starter(), AssetPaths::new());
        let a = anim(&start);
        let idle = a.file.states.iter().next().unwrap();
        assert_eq!(idle.1.name, "Idle");
        assert_eq!(a.file.nodes[&a.machine].entry, *idle.0);
        assert_eq!(a.state_mark(idle.0), Some("no clip"));
        assert_eq!(a.marks.len(), 1, "{:?}", a.marks);
        round_trip(&start);
    }

    /// ADR 0124's E12-5d-2b: layers one at a time on the canvas. A layer added (a machine
    /// of its own whose Idle is its entry) is shown and is a step; its section's rows set
    /// its name (refused empty or taken), blend, weight (held within 0 to 1) and weight's
    /// parameter, each a step; it moves under the base and back; the base shown again shows
    /// its machine; a state added goes into the layer shown; the layer deleted takes what
    /// only it plays, and undo brings it back.
    #[test]
    fn layers_show_one_at_a_time_and_edit_as_steps() {
        let mut d = doc();
        assert_eq!(anim(&d).layer, "base");
        assert_eq!(anim(&d).layer_ids, ["base"]);
        let base_text = d.text.clone();
        let Some(Outcome::Edited { before, after, .. }) = d.add_layer() else {
            panic!("a step");
        };
        assert_eq!(before, base_text);
        assert_ne!(after, before);
        let a = anim(&d);
        assert_eq!(a.layer_ids.len(), 2);
        assert_eq!(a.layer_index(), Some(1), "the new layer shown");
        let layer = a.shown_layer().unwrap();
        assert_eq!(layer.name, "Layer");
        assert_eq!(layer.order, 1);
        assert_eq!(a.machine, layer.root);
        assert_eq!(a.file.states_of(&a.machine).count(), 1, "its Idle");
        assert!(!d.framed, "framed anew");
        // Its section, with nothing selected, before the parameters'.
        let (mut rows, mut sections) = (Vec::new(), Vec::new());
        a.rows(&mut rows, &mut sections);
        assert_eq!(
            rows[..4],
            [
                AnimRow::LayerName,
                AnimRow::LayerBlend,
                AnimRow::LayerWeight,
                AnimRow::LayerWeightParam
            ]
        );
        assert_eq!(sections[0], (AnimSection::Layer, 0, 4));
        let mut title = String::new();
        a.layer_title(&mut title);
        assert_eq!(title, "Layer \u{b7} Layer");
        assert!(d.anim_set_text(AnimRow::LayerName, " ").is_err());
        assert!(
            d.anim_set_text(AnimRow::LayerName, "Base").is_err(),
            "taken"
        );
        assert!(
            d.anim_set_text(AnimRow::LayerName, "Upper")
                .unwrap()
                .is_some()
        );
        assert!(
            d.anim_set_numbers(AnimRow::LayerWeight, [0.5, 0.0, 0.0, 0.0])
                .is_some()
        );
        assert!(
            d.anim_set_numbers(AnimRow::LayerWeight, [1.5, 0.0, 0.0, 0.0])
                .is_some()
        );
        assert_eq!(
            anim(&d).shown_layer().unwrap().weight,
            1.0,
            "held within 0 to 1"
        );
        assert!(
            d.anim_set_numbers(AnimRow::LayerWeight, [0.25, 0.0, 0.0, 0.0])
                .is_some()
        );
        let options = |d: &GraphDoc, row| {
            let mut out = Vec::new();
            anim(d).options(row, &mut out);
            out.into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        assert_eq!(options(&d, AnimRow::LayerBlend), ["Normal", "Additive"]);
        assert!(d.anim_choose(AnimRow::LayerBlend, 1).is_some());
        assert_eq!(
            options(&d, AnimRow::LayerWeightParam),
            ["none", "speed"],
            "a trigger sets no weight"
        );
        assert!(d.anim_choose(AnimRow::LayerWeightParam, 1).is_some());
        let l = anim(&d).shown_layer().unwrap();
        assert_eq!(
            (l.name.as_str(), l.weight, l.blend, l.weight_param.as_str()),
            ("Upper", 0.25, LayerBlend::Additive, "speed")
        );
        let mut texts = Vec::new();
        for (k, row) in [
            AnimRow::LayerBlend,
            AnimRow::LayerWeight,
            AnimRow::LayerWeightParam,
        ]
        .into_iter()
        .enumerate()
        {
            anim(&d).row_texts(row, &mut texts, k);
            assert!(anim(&d).changed(row), "{row:?}");
        }
        assert_eq!(texts, ["Additive", "0.25", "speed"]);
        assert!(d.anim_reset(AnimRow::LayerWeightParam).is_some());
        assert!(anim(&d).shown_layer().unwrap().weight_param.is_empty());
        // Under the base and back; nothing at the end.
        let upper = anim(&d).layer.clone();
        assert!(d.move_layer(true).is_some());
        assert_eq!(anim(&d).layer_ids, [upper.clone(), String::from("base")]);
        assert_eq!(anim(&d).layer, upper, "still shown");
        assert!(d.move_layer(true).is_none(), "first already");
        assert!(d.move_layer(false).is_some());
        assert_eq!(anim(&d).layer_ids, [String::from("base"), upper.clone()]);
        // The base shown again: its machine; a state added goes into the layer shown.
        d.show_layer(0);
        assert_eq!(anim(&d).layer, "base");
        assert_eq!(anim(&d).machine, "m");
        d.show_layer(1);
        let machine = anim(&d).machine.clone();
        assert!(d.add_state(StateKind::Clip, [400.0, 0.0], None).is_some());
        assert_eq!(anim(&d).file.states_of(&machine).count(), 2);
        assert_eq!(
            anim(&d).file.states_of("m").count(),
            2,
            "the base's untouched"
        );
        // Deleted: its machine, its states and their poses go; the base is shown.
        let before = d.text.clone();
        let (nodes, states) = (anim(&d).file.nodes.len(), anim(&d).file.states.len());
        let (_, after) = d.delete_layer().unwrap();
        let a = anim(&d);
        assert_eq!(a.layer_ids, ["base"]);
        assert_eq!(a.layer, "base");
        assert_eq!(a.machine, "m");
        assert!(!a.file.nodes.contains_key(&machine));
        assert_eq!(a.file.states.len(), states - 2);
        assert_eq!(a.file.nodes.len(), nodes - 3, "the machine and two poses");
        assert_ne!(after, before);
        // Undone: back.
        d.put_text(&before).unwrap();
        assert_eq!(anim(&d).layer_ids.len(), 2);
        assert!(anim(&d).file.nodes.contains_key(&machine));
    }

    /// ADR 0124's E12-5d-2b, the team's а: a layer's mask as check boxes over the joints.
    /// A box turns its joint's subtree in or out, the mask written its shortest way: every
    /// joint in is the empty mask, a mask putting every joint out keeps its roots at 0 (an
    /// empty one would put them in), entries repeating what they inherit go. The picked
    /// joint's weight row sets its own weight (the others kept in) and ↺ gives it back.
    #[test]
    fn a_layers_mask_turns_subtrees_in_and_out_by_check_boxes() {
        // hips ─┬─ spine ── arm
        //       └─ leg ── foot
        let names: Vec<String> = ["hips", "spine", "arm", "leg", "foot"]
            .map(String::from)
            .to_vec();
        let parents = [None, Some(0), Some(1), Some(0), Some(3)];
        let weights = |m: &BTreeMap<String, f32>| {
            (0..names.len())
                .map(|j| mask_weight_of(m, &names, &parents, j))
                .collect::<Vec<_>>()
        };
        let entries =
            |m: &BTreeMap<String, f32>| m.iter().map(|(k, v)| (k.clone(), *v)).collect::<Vec<_>>();
        let mut mask = BTreeMap::new();
        assert_eq!(weights(&mask), [1.0; 5], "empty: every joint in");
        // The leg out: the hips spelt out, the leg's subtree at 0.
        toggle_mask(&mut mask, &names, &parents, 3);
        assert_eq!(
            entries(&mask),
            [(String::from("hips"), 1.0), (String::from("leg"), 0.0)]
        );
        assert_eq!(weights(&mask), [1.0, 1.0, 1.0, 0.0, 0.0]);
        // Back in: every joint in again, the empty mask.
        toggle_mask(&mut mask, &names, &parents, 3);
        assert!(mask.is_empty(), "{mask:?}");
        // The hips out: every joint out, the root kept at 0.
        toggle_mask(&mut mask, &names, &parents, 0);
        assert_eq!(entries(&mask), [(String::from("hips"), 0.0)]);
        assert_eq!(weights(&mask), [0.0; 5]);
        // The spine in: the upper body alone.
        toggle_mask(&mut mask, &names, &parents, 1);
        assert_eq!(weights(&mask), [0.0, 1.0, 1.0, 0.0, 0.0]);
        // A hand-written mask of the spine alone, the spine turned out: the root at 0.
        let mut alone = BTreeMap::from([(String::from("spine"), 1.0)]);
        toggle_mask(&mut alone, &names, &parents, 1);
        assert_eq!(entries(&alone), [(String::from("hips"), 0.0)]);
        // A weight on the arm from the empty mask keeps the others in.
        let mut half = BTreeMap::new();
        set_mask_weight(&mut half, &names, &parents, 2, 0.5);
        assert_eq!(weights(&half), [1.0, 1.0, 0.5, 1.0, 1.0]);
        reset_mask_weight(&mut half, &names, &parents, 2);
        assert!(half.is_empty(), "back to every joint: {half:?}");
        // Through the document: the picked joint's row, a box, each a step.
        let mut d = doc();
        anim_mut(&mut d).set_joints(&names, parents.into_iter());
        assert_eq!(anim(&d).joint_depths, [0, 1, 2, 1, 2]);
        let (mut rows, mut sections) = (Vec::new(), Vec::new());
        anim(&d).rows(&mut rows, &mut sections);
        assert_eq!(
            sections[1],
            (AnimSection::Mask, 4, 4),
            "no joint picked: no row"
        );
        let mut title = String::new();
        anim(&d).mask_title(&mut title);
        assert_eq!(title, "Mask \u{b7} all joints");
        assert!(d.toggle_mask_joint(3).is_some());
        assert_eq!(anim(&d).mask_weight(4), 0.0, "the foot with its leg");
        anim(&d).mask_title(&mut title);
        assert_eq!(title, "Mask \u{b7} 2 set");
        anim_mut(&mut d).mask_joint = String::from("arm");
        anim(&d).rows(&mut rows, &mut sections);
        assert_eq!(sections[1], (AnimSection::Mask, 4, 5));
        assert_eq!(rows[4], AnimRow::MaskWeight);
        assert_eq!(anim(&d).numbers(AnimRow::MaskWeight)[0], 1.0, "inherited");
        assert!(!anim(&d).changed(AnimRow::MaskWeight));
        assert!(
            d.anim_set_numbers(AnimRow::MaskWeight, [0.25, 0.0, 0.0, 0.0])
                .is_some()
        );
        assert_eq!(anim(&d).mask_weight(2), 0.25);
        assert!(anim(&d).changed(AnimRow::MaskWeight));
        assert!(d.anim_reset(AnimRow::MaskWeight).is_some());
        assert_eq!(anim(&d).mask_weight(2), 1.0);
        assert!(!anim(&d).changed(AnimRow::MaskWeight));
    }

    /// ADR 0124's E12-5d-2c: a state's pose made a machine gets its own Idle as its entry;
    /// Tab (or a double-click) enters it, the bar's crumbs name it, a state added goes inside,
    /// Tab with nothing selected comes back out with the state selected; the machine gone
    /// (its state's pose a clip again) takes its states with it and the canvas comes out.
    #[test]
    fn a_state_playing_a_machine_is_entered_and_left() {
        let mut d = doc();
        anim_mut(&mut d).selected.insert(String::from("idle"));
        assert_eq!(POSE_KINDS[3], "Machine");
        assert!(d.anim_choose(AnimRow::Pose, 3).is_some(), "a step");
        let inner = anim(&d).file.states["idle"].pose.clone();
        let a = anim(&d);
        assert_eq!(a.file.nodes[&inner].kind, NodeKind::Machine);
        let entry = a.file.nodes[&inner].entry.clone();
        assert_eq!(a.file.states[&entry].name, "Idle");
        assert_eq!(a.file.states[&entry].machine, inner);
        // Tab: in.
        assert!(d.anim_tab().is_ok());
        assert_eq!(anim(&d).path, std::slice::from_ref(&inner));
        assert_eq!(anim(&d).machine, inner);
        assert!(anim(&d).selected.is_empty());
        assert!(!d.framed, "framed anew");
        let mut crumbs = Vec::new();
        anim(&d).crumbs(&mut crumbs);
        assert_eq!(crumbs, ["Idle"]);
        let nodes: Vec<&String> = anim(&d).file.nodes.keys().collect();
        assert_eq!(
            anim(&d).machine_node_index(),
            nodes.iter().position(|k| **k == inner)
        );
        // A state added goes inside.
        assert!(d.add_state(StateKind::Clip, [300.0, 0.0], None).is_some());
        assert_eq!(anim(&d).file.states_of(&inner).count(), 2);
        assert_eq!(
            anim(&d).file.states_of("m").count(),
            2,
            "the outer one's untouched"
        );
        // Tab with nothing selected: out, the state selected.
        anim_mut(&mut d).clear_selection();
        assert!(d.anim_tab().is_ok());
        assert!(anim(&d).path.is_empty());
        assert_eq!(anim(&d).machine, "m");
        assert!(anim(&d).selected.contains("idle"));
        // A double-click on the state enters it too.
        d.relayout(&mut measure);
        let k = anim(&d).boxes.iter().position(|b| b.id == "idle").unwrap();
        assert!(anim(&d).boxes[k].machine);
        let [lo, hi] = anim(&d).boxes[k].rect();
        let c = [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5];
        let mut drag = None;
        for _ in 0..2 {
            at(&mut d, &mut drag, c, true, true, false);
            at(&mut d, &mut drag, c, false, false, false);
        }
        assert_eq!(
            anim(&d).path,
            std::slice::from_ref(&inner),
            "entered by the double-click"
        );
        // Its pose a clip again: its states go, the canvas comes out.
        d.leave_machine(0);
        anim_mut(&mut d).clear_selection();
        anim_mut(&mut d).selected.insert(String::from("idle"));
        assert!(d.anim_choose(AnimRow::Pose, 0).is_some());
        assert_eq!(anim(&d).file.states_of(&inner).count(), 0);
        assert_eq!(anim(&d).file.nodes[&inner].kind, NodeKind::Clip);
        // A machine state added from the search: its own Idle inside.
        anim_mut(&mut d).clear_selection();
        assert!(
            d.add_state(StateKind::Machine, [0.0, 300.0], None)
                .is_some()
        );
        let made = anim(&d).selected.iter().next().unwrap().clone();
        let pose = anim(&d).file.states[&made].pose.clone();
        assert_eq!(anim(&d).file.states_of(&pose).count(), 1);
        // Entered, then the state deleted (by its text): the canvas comes out.
        assert!(d.enter_machine(&made));
        let mut text = d.text.clone();
        let gone = text
            .lines()
            .filter(|l| !l.contains(&format!("[\"{made}\"]")))
            .collect::<Vec<_>>()
            .join("\n");
        text.clone_from(&gone);
        d.put_text(&text).unwrap();
        assert!(anim(&d).path.is_empty(), "{:?}", anim(&d).path);
        assert_eq!(anim(&d).machine, "m");
    }

    /// ADR 0124's E12-5d-2c: the shown machine's live state is read at its node's place among
    /// the file's nodes (in their ids' order, as the player's graph has them), a layer
    /// playing nothing before it or not.
    #[test]
    fn the_shown_machine_is_found_by_its_nodes_place() {
        let mut f = file();
        f.layers.insert(
            String::from("aaa"),
            LayerFile {
                name: String::from("Ghost"),
                root: String::from("nothing"),
                ..LayerFile::default()
            },
        );
        let d = GraphDoc::new_anim(AssetId::new(), "anim/hero.agraph", f, AssetPaths::new());
        assert_eq!(anim(&d).layer, "base", "the first playing a machine");
        // Nodes in their ids' order: ci, cw, m.
        assert_eq!(anim(&d).machine_node_index(), Some(2));
    }

    /// A state's rows: its name (refused empty or taken), its clip, its pace, what it plays
    /// (a blend's parameters and clips, one added, one emptied away); each undone.
    #[test]
    fn a_states_rows_edit_its_name_clip_pace_and_blend() {
        let mut d = doc();
        anim_mut(&mut d).selected.insert(String::from("idle"));
        let mut rows = Vec::new();
        let mut sections = Vec::new();
        anim(&d).rows(&mut rows, &mut sections);
        assert_eq!(
            rows,
            [
                AnimRow::Name,
                AnimRow::Entry,
                AnimRow::Pose,
                AnimRow::Clip,
                AnimRow::Speed
            ]
        );
        assert_eq!(sections, [(AnimSection::Selected, 0, 5)]);
        let mut title = String::new();
        anim(&d).title(&mut title);
        assert_eq!(title, "State \u{b7} Idle");
        assert!(d.anim_set_text(AnimRow::Name, " ").is_err());
        assert_eq!(
            d.anim_set_text(AnimRow::Name, "Walk"),
            Err(String::from("Another state of its machine is named Walk"))
        );
        let (before, _) = d.anim_set_text(AnimRow::Name, "Rest").unwrap().unwrap();
        assert_eq!(anim(&d).file.states["idle"].name, "Rest");
        d.anim_set_text(AnimRow::Clip, "Breathe").unwrap().unwrap();
        assert_eq!(anim(&d).file.nodes["ci"].clip, "Breathe");
        d.anim_set_numbers(AnimRow::Speed, [1.5, 0.0, 0.0, 0.0])
            .unwrap();
        assert_eq!(anim(&d).file.nodes["ci"].speed, 1.5);
        assert!(anim(&d).changed(AnimRow::Speed));
        d.anim_reset(AnimRow::Speed).unwrap();
        assert!(!anim(&d).changed(AnimRow::Speed));
        // A 2D blend: two parameters, clips added and placed, one emptied away.
        d.anim_choose(AnimRow::AddParam, 0).unwrap();
        let mut options = Vec::new();
        anim(&d).options(AnimRow::Pose, &mut options);
        let blend = options.iter().position(|o| *o == "Blend2d").unwrap();
        d.anim_choose(AnimRow::Pose, blend).unwrap();
        let pose = &anim(&d).file.nodes["ci"];
        assert_eq!(pose.kind, NodeKind::Blend2d);
        assert_eq!(
            (pose.param.as_str(), pose.param_y.as_str()),
            ("number", "speed")
        );
        d.anim_set_text(AnimRow::AddPoint, "Walk").unwrap().unwrap();
        d.anim_set_text(AnimRow::AddPoint, "Run").unwrap().unwrap();
        d.anim_set_numbers(AnimRow::PointAt(1), [3.0, 1.0, 0.0, 0.0])
            .unwrap();
        anim(&d).rows(&mut rows, &mut sections);
        assert!(rows.contains(&AnimRow::PointClip(1)) && rows.contains(&AnimRow::BlendParamY));
        assert_eq!(
            anim(&d).look(AnimRow::PointAt(0)),
            RowLook::Numbers(2, false)
        );
        let points = &anim(&d).file.nodes["ci"].points;
        assert_eq!(points[1].at, [3.0, 1.0]);
        assert_eq!(points[1].clip, "Run");
        d.anim_set_text(AnimRow::PointClip(0), "").unwrap().unwrap();
        assert_eq!(anim(&d).file.nodes["ci"].points.len(), 1);
        round_trip(&d);
        d.put_text(&before).unwrap();
        assert_eq!(anim(&d).file, file());
    }

    /// A transition's rows: its condition compiled as typed (its problem at its place), its
    /// numbers kept in their ranges, its switches; the parameters with nothing selected:
    /// added, renamed (the conditions follow), retyped, deleted.
    #[test]
    fn a_transitions_condition_is_checked_and_the_parameters_edit() {
        let mut d = doc();
        anim_mut(&mut d).transitions.insert(String::from("t1"));
        let mut rows = Vec::new();
        let mut sections = Vec::new();
        anim(&d).rows(&mut rows, &mut sections);
        assert_eq!(rows[0], AnimRow::When);
        assert!(!rows.contains(&AnimRow::Problem));
        let mut title = String::new();
        anim(&d).title(&mut title);
        assert_eq!(title, "Transition \u{b7} Idle \u{2192} Walk");
        d.anim_set_text(AnimRow::When, "speeed > 0.1")
            .unwrap()
            .unwrap();
        assert_eq!(
            anim(&d).transition_mark("t1"),
            Some("no parameter `speeed` (at 1)")
        );
        anim(&d).rows(&mut rows, &mut sections);
        assert_eq!(rows[1], AnimRow::Problem);
        let mut texts = Vec::new();
        anim(&d).row_texts(AnimRow::Problem, &mut texts, 0);
        assert_eq!(texts[0], "no parameter `speeed` (at 1)");
        d.anim_set_text(AnimRow::When, "speed > 0.1 and (jump")
            .unwrap();
        assert_eq!(anim(&d).transition_mark("t1"), Some("expected `)` (at 22)"));
        d.anim_set_text(AnimRow::When, "jump").unwrap();
        assert_eq!(anim(&d).transition_mark("t1"), None);
        // Numbers in their ranges; switches.
        d.anim_set_numbers(AnimRow::Blend, [-1.0, 0.0, 0.0, 0.0])
            .unwrap();
        d.anim_set_numbers(AnimRow::Priority, [2.6, 0.0, 0.0, 0.0])
            .unwrap();
        d.anim_set_numbers(AnimRow::Exit, [0.9, 0.0, 0.0, 0.0])
            .unwrap();
        let t = &anim(&d).file.transitions["t1"];
        assert_eq!((t.blend, t.priority, t.exit), (0.0, 3, 0.9));
        let Ok(Some(Outcome::Edited { name, .. })) = d.anim_toggle(AnimRow::Inertialize) else {
            panic!("toggled")
        };
        assert_eq!(name, "Toggle Inertialize");
        assert!(anim(&d).file.transitions["t1"].inertialize);
        assert_eq!(anim(&d).look(AnimRow::Inertialize), RowLook::Check(true));
        d.anim_reset(AnimRow::Inertialize).unwrap();
        assert!(!anim(&d).file.transitions["t1"].inertialize);
        // With nothing selected: the layer shown (E12-5d-2b), the parameters a section
        // each, then Add.
        anim_mut(&mut d).clear_selection();
        anim(&d).rows(&mut rows, &mut sections);
        assert_eq!(
            sections,
            [
                (AnimSection::Layer, 0, 4),
                (AnimSection::Mask, 4, 4),
                (AnimSection::Param(0), 4, 6),
                (AnimSection::Param(1), 6, 9),
                (AnimSection::Add, 9, 10)
            ]
        );
        assert_eq!(anim(&d).param(0).unwrap().0, "jump");
        // Renamed: the conditions naming it follow, a word inside another stays.
        d.anim_set_text(AnimRow::When, "speed > 0.1").unwrap();
        anim_mut(&mut d).transitions.insert(String::from("t1"));
        d.anim_set_text(AnimRow::When, "speed > 1e-3 and speedy")
            .unwrap();
        anim_mut(&mut d).clear_selection();
        assert!(d.anim_set_text(AnimRow::ParamName(1), "and").is_err());
        assert!(d.anim_set_text(AnimRow::ParamName(1), "2fast").is_err());
        assert_eq!(
            d.anim_set_text(AnimRow::ParamName(1), "jump"),
            Err(String::from("Another parameter is named jump"))
        );
        let (before, _) = d
            .anim_set_text(AnimRow::ParamName(1), "pace")
            .unwrap()
            .unwrap();
        let f = &anim(&d).file;
        assert!(f.params.contains_key("pace") && !f.params.contains_key("speed"));
        assert_eq!(f.transitions["t1"].when, "pace > 1e-3 and speedy");
        assert_eq!(f.transitions["t2"].when, "pace <= 0.1");
        d.put_text(&before).unwrap();
        // A whole number, then a flag: its value made one it can hold.
        d.anim_set_numbers(AnimRow::ParamValue(1), [2.4, 0.0, 0.0, 0.0])
            .unwrap();
        d.anim_choose(AnimRow::ParamKind(1), 2).unwrap();
        assert_eq!(anim(&d).file.params["speed"].value, 2.0);
        d.anim_choose(AnimRow::ParamKind(1), 1).unwrap();
        assert_eq!(anim(&d).file.params["speed"].value, 1.0);
        assert_eq!(anim(&d).look(AnimRow::ParamValue(1)), RowLook::Check(true));
        d.anim_toggle(AnimRow::ParamValue(1)).unwrap();
        assert_eq!(anim(&d).file.params["speed"].value, 0.0);
        // Added by kind, named by it; deleted: the conditions naming it say so.
        d.anim_choose(AnimRow::AddParam, 3).unwrap();
        assert_eq!(anim(&d).file.params["trigger"].kind, ParamKind::Trigger);
        d.delete_param(1).unwrap();
        assert!(!anim(&d).file.params.contains_key("speed"));
        assert!(anim(&d).transition_mark("t2").unwrap().contains("speed"));
        round_trip(&d);
    }

    /// The bar says the states, transitions, selection, zoom and the first problem; the
    /// renamer leaves numbers and other words alone; problems name what they are on.
    #[test]
    fn the_status_and_problems_say_what_is_wrong() {
        let mut d = doc();
        let mut s = String::new();
        anim(&d).status(1.0, &mut s);
        assert_eq!(s, "2 states \u{b7} 2 transitions \u{b7} 100 %");
        anim_mut(&mut d).selected.insert(String::from("walk"));
        d.anim_set_text(AnimRow::Clip, "").unwrap().unwrap();
        anim(&d).status(0.5, &mut s);
        assert_eq!(
            s,
            "2 states \u{b7} 2 transitions \u{b7} 1 selected \u{b7} 50 % \u{b7} \u{26a0} 1 \
             problem: Walk: no clip"
        );
        assert_eq!(
            renamed("a+speed*2e+1-speed2", "speed", "v"),
            "a+v*2e+1-speed2"
        );
        // As the conditions read it: `2speed` is a number, then a word.
        assert_eq!(renamed("2speed ≥ speed", "speed", "v"), "2v ≥ v");
        let mut broken = file();
        broken.layers.get_mut("base").unwrap().weight_param = String::from("lean");
        let a = AnimDoc::new(broken);
        assert!(
            a.marks
                .iter()
                .any(|m| m.on == On::Graph(String::from("layer Base")))
        );
    }

    /// A state into itself is a loop over its box; Any State's transitions come from its
    /// box; a box's edge and inside are told apart.
    #[test]
    fn loops_and_any_state_are_drawn_and_hit() {
        let mut f = file();
        f.transitions.insert(
            String::from("t3"),
            TransitionFile {
                from: String::from("walk"),
                to: String::from("walk"),
                exit: 1.0,
                ..TransitionFile::default()
            },
        );
        f.transitions.insert(
            String::from("t4"),
            TransitionFile {
                to: String::from("idle"),
                when: String::from("jump"),
                ..TransitionFile::default()
            },
        );
        let mut d = GraphDoc::new_anim(AssetId::new(), "a.agraph", f, AssetPaths::new());
        d.relayout(&mut measure);
        let a = anim(&d);
        let looped = a.arrows.iter().find(|r| r.id == "t3").unwrap();
        assert!(looped.looped);
        let walk = &a.boxes[1];
        assert_eq!(looped.to, [walk.at[0] + walk.size[0] - LOOP[2], walk.at[1]]);
        assert_eq!(a.hit(looped.middle(), 4.0), Hit::Arrow(2));
        let any = a.arrows.iter().find(|r| r.id == "t4").unwrap();
        let r = a.any_rect().unwrap();
        assert!(
            any.from[1] >= r[0][1] && any.from[1] <= r[1][1] + 0.01,
            "{any:?}"
        );
        assert_eq!(a.hit([-150.0, -80.0], 6.0), Hit::Any);
        assert_eq!(a.hit([-200.0, -80.0], 6.0), Hit::AnyEdge);
        assert_eq!(a.hit([70.0, 30.0], 6.0), Hit::State(0));
        assert_eq!(a.hit([1.0, 30.0], 6.0), Hit::Edge(0));
        assert_eq!(
            a.hit([-3.0, 30.0], 6.0),
            Hit::Edge(0),
            "a little outside too"
        );
        assert_eq!(a.hit([-30.0, 300.0], 6.0), Hit::Canvas);
        // Any State moves (its machine's place), as a step.
        let mut drag = None;
        at(&mut d, &mut drag, [-150.0, -80.0], true, true, false);
        at(&mut d, &mut drag, [-100.0, -60.0], false, true, false);
        let (name, ..) = edited(at(&mut d, &mut drag, [-100.0, -60.0], false, false, false));
        assert_eq!(name, "Move Any State");
        assert_eq!(anim(&d).file.nodes["m"].at, [-150.0, -80.0]);
        // F frames every box and Any State.
        let area = anim(&d).extent().unwrap();
        assert_eq!(area[0], [-150.0, -80.0]);
    }
}
