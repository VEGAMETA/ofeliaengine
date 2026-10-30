//! A material graph as the editor holds it (ADR 0123 E10-3d): its `.mgraph` file open in a
//! tab of its own, the canvas's view of it, its nodes laid out as boxes with their ports,
//! what lies under the pointer, and the canvas's edits, each a file step of the one history
//! (as material files' and the mixer's).
//!
//! - **The canvas** is in canvas units, a node's `at` its box's top left; the view maps them
//!   to the panel by its pan and its zoom. The zoom goes by steps, so the text is drawn at a
//!   few sizes only (the glyph atlas keeps every size it draws).
//! - **A box** has the node's title (its parameter's name after its kind's, where it is
//!   one), its inputs down its left edge and its outputs down its right, side by side; a
//!   constant shows its value where inputs would be. The surface shows the inputs its
//!   shading model reads, and any other that is linked.
//! - **Links** run from an output on a box's right edge to an input on another's left, as
//!   curves leaving and arriving level (Blender's, Unreal's).
//! - **The pointer:** a press on an output draws a link from it, on an input draws one to it
//!   (or lifts its link off, carried to another input or dropped); on a box selects it
//!   (Shift adds or takes it out) and drags the selection; on the empty canvas draws a box
//!   that selects what it meets. The middle or the right button pans, the wheel zooms about
//!   the pointer, Escape takes a drag back.
//! - **Edits** keep the graph whole: a link that would loop back is refused, the surface
//!   stays, a link dropped on an input replaces the one it had; moves end on whole units.
//! - **More** (E10-3d-2): Ctrl+D copies the selection with the links into it; Ctrl+drag
//!   from an output carries all its links to another output (egui-snarl's yank) or takes
//!   them out; [`starter`] is a new graph drawing a glTF material's own look.
//! - **Adding** (E10-3e): the node search offers [`addable`] kinds (the editor's palette at
//!   the pointer); a link dropped on the empty canvas asks for it, the node it adds linked
//!   to the link's port by the port that fits best; an image dropped from the dock is a
//!   texture node.
//! - **Values** (E10-3f-1): the inspector shows the node selected alone ([`NodeRow`]s: its
//!   parameter's name, a constant's value, its unlinked inputs' own values, a texture's
//!   image, its note), or the graph's shading model with none.
//! - **The preview** (E10-3f-2, the team's «а»): a sphere the player draws with the graph's
//!   editing build ([`preview_material`]), in the canvas's corner; every number is a
//!   parameter there, so a value's change reaches it without compiling anything.
//! - **Errors** (E10-3f-3): what stops the graph from generating, at its node (a loop, a
//!   kind or a port a merge left), framed in red on the canvas with why under the box;
//!   Slang's own errors in the preview's module go to their nodes by the generator's line
//!   map ([`slang_error`], E10-3g-1: an Expression node's code).

use crate::paint::UiText;
use crate::shell::Palette;
use engine_assets::OutlineMaps;
use engine_assets::graph_module::{GRAPH_TYPE, encoding};
use engine_assets::material_file::{Look as MaterialLook, MaterialFile, MaterialTexture};
use engine_core::math::{Vec2, Vec4};
use engine_material_graph::graph::OUTPUT;
use engine_material_graph::library::{self, Body, Fill, OUTPUT_KIND, REROUTE_KIND};
use engine_material_graph::{Build, CALL_KIND, FUNCTION_KIND, Generated, GraphError, ParamType};
use engine_material_graph::{Frame, Graph, GraphFile, Kind, Model, Node, PortRef, PortType};
use engine_render2d::{Canvas, ClipRect, GlyphTexture, Order, Quad};
use engine_scene::{AssetId, AssetPaths, AssetRef};
use engine_ui::TextMeasure;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The zoom's steps: each draws the text at one size.
pub(crate) const ZOOMS: [f32; 11] = [0.25, 0.33, 0.5, 0.67, 0.8, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0];
/// Zoom 1.
const ZOOM_ONE: usize = 5;

/// A box's title bar, in canvas units.
pub(crate) const TITLE: f32 = 26.0;
/// A row of ports.
pub(crate) const ROW: f32 = 22.0;
/// Room inside a box's edges.
pub(crate) const PAD: f32 = 10.0;
/// Between the inputs' names and the outputs'.
const GAP: f32 = 20.0;
/// The narrowest box.
const MIN_WIDTH: f32 = 110.0;
/// A port's dot.
const PORT_RADIUS: f32 = 4.5;
/// How far from a port a press or a drop still takes it, in panel pixels.
pub(crate) const PORT_REACH: f32 = 9.0;
/// The titles' text at zoom 1.
pub(crate) const TITLE_TEXT: f32 = 13.0;
/// The ports' text.
pub(crate) const PORT_TEXT: f32 = 12.0;
/// Around the nodes framed (F), in panel pixels.
const FRAME_MARGIN: f32 = 40.0;
/// A node's preview's tile (E10-4g), in canvas units: the atlas's pixels at zoom 1.
pub(crate) const TILE: f32 = 64.0;
/// Room above and below a tile.
const TILE_ROOM: f32 = 8.0;
/// A box's corners.
pub(crate) const RADIUS: f32 = 5.0;
/// The grid's small and large squares.
const GRID: f32 = 20.0;
const GRID_MAJOR: f32 = 100.0;
/// Pieces a link is drawn in.
const LINK_SEGMENTS: usize = 24;
/// A reroute's capsule (E10-3e-2a): its pins at its ends, its middle grabbed to move it.
const REROUTE: [f32; 2] = [28.0, 14.0];
/// A frame's title bar, the room left round what C frames, its least size and its corner's
/// grip (E10-3e-2b), in canvas units.
const FRAME_TITLE: f32 = 26.0;
const FRAME_ROOM: f32 = 20.0;
const FRAME_MIN: [f32; 2] = [80.0, 50.0];
const FRAME_GRIP: f32 = 12.0;
/// A frame's area and title bar (linear), the grid showing through.
const FRAME_FILL: Vec4 = Vec4::new(0.03, 0.035, 0.045, 0.55);
const FRAME_BAR: Vec4 = Vec4::new(0.05, 0.055, 0.07, 0.9);
/// Two presses this close in time (and in place, [`CLICK_SLOP`]) are a double-click.
pub(crate) const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);
/// Under this many panel pixels a box's drag is a click.
pub(crate) const CLICK_SLOP: f32 = 3.0;

/// How the canvas shows: the canvas point at the panel's top left, and the zoom's step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct View {
    /// The canvas point at the panel's top left.
    pub(crate) pan: [f32; 2],
    /// The zoom's step in [`ZOOMS`].
    pub(crate) step: usize,
}

impl Default for View {
    fn default() -> Self {
        Self {
            pan: [0.0; 2],
            step: ZOOM_ONE,
        }
    }
}

impl View {
    /// Panel pixels a canvas unit.
    pub(crate) fn zoom(&self) -> f32 {
        ZOOMS[self.step.min(ZOOMS.len() - 1)]
    }

    /// Canvas point `p` in the panel whose top left is `origin`.
    pub(crate) fn panel_point(&self, p: [f32; 2], origin: [f32; 2]) -> [f32; 2] {
        let z = self.zoom();
        [
            origin[0] + (p[0] - self.pan[0]) * z,
            origin[1] + (p[1] - self.pan[1]) * z,
        ]
    }

    /// The canvas point under panel point `q`.
    pub(crate) fn canvas_point(&self, q: [f32; 2], origin: [f32; 2]) -> [f32; 2] {
        let z = self.zoom();
        [
            self.pan[0] + (q[0] - origin[0]) / z,
            self.pan[1] + (q[1] - origin[1]) / z,
        ]
    }

    /// Moved with a drag of `d` panel pixels: the canvas follows the pointer.
    pub(crate) fn pan_by(&mut self, d: [f32; 2]) {
        let z = self.zoom();
        self.pan[0] -= d[0] / z;
        self.pan[1] -= d[1] / z;
    }

    /// `steps` zoom steps in (out where negative) about panel point `q`: the canvas point
    /// under it stays there.
    pub(crate) fn zoom_by(&mut self, steps: i32, q: [f32; 2], origin: [f32; 2]) {
        let at = self.canvas_point(q, origin);
        let last = ZOOMS.len() as i32 - 1;
        self.step = (self.step as i32 + steps).clamp(0, last) as usize;
        let z = self.zoom();
        self.pan = [
            at[0] - (q[0] - origin[0]) / z,
            at[1] - (q[1] - origin[1]) / z,
        ];
    }

    /// Canvas rectangle `area` (its corners) whole in a panel of `size`, centered, at the
    /// largest step that fits up to zoom 1.
    pub(crate) fn frame(&mut self, area: [[f32; 2]; 2], size: [f32; 2]) {
        let w = (area[1][0] - area[0][0]).max(1.0);
        let h = (area[1][1] - area[0][1]).max(1.0);
        let room = [
            (size[0] - 2.0 * FRAME_MARGIN).max(1.0),
            (size[1] - 2.0 * FRAME_MARGIN).max(1.0),
        ];
        self.step = (0..=ZOOM_ONE)
            .rev()
            .find(|&s| w * ZOOMS[s] <= room[0] && h * ZOOMS[s] <= room[1])
            .unwrap_or(0);
        let z = self.zoom();
        let center = [
            (area[0][0] + area[1][0]) * 0.5,
            (area[0][1] + area[1][1]) * 0.5,
        ];
        self.pan = [center[0] - size[0] * 0.5 / z, center[1] - size[1] * 0.5 / z];
    }
}

/// A node as the canvas draws it, in canvas units.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeBox {
    /// Its node's id.
    pub(crate) id: String,
    /// Its kind; `None` for one the library lacks (drawn with no ports).
    pub(crate) kind: Option<&'static Kind>,
    /// Its title: its kind's, and its parameter's name where it is one.
    pub(crate) title: String,
    /// Its top left.
    pub(crate) at: [f32; 2],
    /// Its width and height.
    pub(crate) size: [f32; 2],
    /// Its inputs shown, top to bottom.
    pub(crate) inputs: Vec<&'static str>,
    /// Its outputs, top to bottom.
    pub(crate) outputs: Vec<&'static str>,
    /// Its node's preview shows under its ports (E10-4g).
    pub(crate) tile: bool,
}

impl NodeBox {
    /// Its preview's tile (E10-4g): its low and high corners in canvas units, centred
    /// under its ports.
    pub(crate) fn tile_rect(&self) -> Option<[[f32; 2]; 2]> {
        self.tile.then(|| {
            let x = self.at[0] + (self.size[0] - TILE) * 0.5;
            let y = self.at[1] + self.size[1] - TILE - TILE_ROOM;
            [[x, y], [x + TILE, y + TILE]]
        })
    }

    /// Where input `k`'s dot is.
    pub(crate) fn input_at(&self, k: usize) -> [f32; 2] {
        [self.at[0], self.row(k)]
    }

    /// Where output `k`'s dot is.
    pub(crate) fn output_at(&self, k: usize) -> [f32; 2] {
        [self.at[0] + self.size[0], self.row(k)]
    }

    /// Row `k`'s middle (a reroute's middle).
    fn row(&self, k: usize) -> f32 {
        if self.is_reroute() {
            return self.at[1] + self.size[1] * 0.5;
        }
        self.at[1] + TITLE + ROW * (k as f32 + 0.5)
    }

    /// Whether it is a reroute (E10-3e-2a): a capsule with a pin at each end, no title.
    pub(crate) fn is_reroute(&self) -> bool {
        self.kind.is_some_and(|k| k.name == REROUTE_KIND)
    }

    /// Its input `name`'s row.
    pub(crate) fn input(&self, name: &str) -> Option<usize> {
        self.inputs.iter().position(|&n| n == name)
    }

    /// Its output `name`'s row.
    pub(crate) fn output(&self, name: &str) -> Option<usize> {
        self.outputs.iter().position(|&n| n == name)
    }

    /// Whether canvas point `p` is on it.
    pub(crate) fn contains(&self, p: [f32; 2]) -> bool {
        p[0] >= self.at[0]
            && p[1] >= self.at[1]
            && p[0] <= self.at[0] + self.size[0]
            && p[1] <= self.at[1] + self.size[1]
    }

    /// What input `k` carries.
    fn input_type(&self, k: usize) -> PortType {
        self.kind
            .and_then(|kind| kind.input(self.inputs[k]))
            .map_or(PortType::Dynamic, |i| i.ty)
    }

    /// What output `k` gives.
    fn output_type(&self, k: usize) -> PortType {
        self.kind
            .and_then(|kind| kind.output(self.outputs[k]))
            .map_or(PortType::Dynamic, |o| o.ty)
    }
}

/// Whether the surface's input `name` shows under `model`: those every model reads, and the
/// model's own.
fn surface_reads(model: Model, name: &str) -> bool {
    model.terms().contains(&name) || !Model::ALL.iter().any(|m| m.terms().contains(&name))
}

/// The room a constant's value takes where its inputs would be, at zoom 1: a colour's
/// swatch, a number per component.
fn value_width(kind: &Kind) -> f32 {
    match kind.body {
        Body::Constant(_) if kind.name == "color" => 40.0,
        Body::Constant(w) => 12.0 + 34.0 * f32::from(w),
        _ => 0.0,
    }
}

/// Whether a node of `kind` has a preview (E10-4g), as the editing build numbers them: one
/// with an output (a function call by its first) but a reroute and the surface, and a
/// function's output (what it is given, E10-4g-3).
fn previewed(kind: &Kind) -> bool {
    kind.name != REROUTE_KIND
        && kind.name != OUTPUT_KIND
        && (!kind.outputs.is_empty() || matches!(kind.body, Body::FunctionOutput(_)))
}

/// `graph`'s nodes laid out into `out` in their ids' order, each as wide as its texts need
/// (`measure`: a text's width at a size, at zoom 1); a call titled by its function's mark
/// where `titles` knows it (module, function, title; E10-3g-3b); with `tiles`, room under
/// the ports of each node with a preview for its tile (E10-4g).
pub(crate) fn layout(
    graph: &Graph,
    titles: &[(AssetRef, String, String)],
    tiles: bool,
    measure: &mut dyn FnMut(&str, f32) -> f32,
    out: &mut Vec<NodeBox>,
) {
    out.clear();
    for (id, node) in &graph.nodes {
        let kind = engine_material_graph::node_kind(node).ok();
        if kind.is_some_and(|k| k.name == REROUTE_KIND) {
            out.push(NodeBox {
                id: id.clone(),
                kind,
                title: String::new(),
                at: node.at,
                size: REROUTE,
                inputs: vec!["in"],
                outputs: vec!["out"],
                tile: false,
            });
            continue;
        }
        let title = match kind {
            Some(k) if k.body == Body::Expression => {
                format!("{} · {}", k.title, short_code(&node.code))
            }
            Some(k) if matches!(k.body, Body::Call(_)) => titles
                .iter()
                .find(|(m, f, _)| *m == node.module && *f == node.function)
                .map_or_else(|| k.title.to_owned(), |(.., t)| t.clone()),
            Some(k) if !node.param.is_empty() => format!("{} · {}", k.title, node.param),
            Some(k) => k.title.to_owned(),
            None => node.kind.clone(),
        };
        let (inputs, outputs): (Vec<&'static str>, Vec<&'static str>) = match kind {
            Some(k) => (
                k.inputs
                    .iter()
                    .map(|i| i.name)
                    .filter(|&name| {
                        k.name != OUTPUT_KIND
                            || surface_reads(graph.model, name)
                            || graph
                                .links
                                .keys()
                                .any(|to| to.node == *id && to.port == name)
                    })
                    .collect(),
                k.outputs.iter().map(|o| o.name).collect(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let mut left = kind.map_or(0.0, value_width);
        for name in &inputs {
            left = left.max(measure(name, PORT_TEXT));
        }
        let mut right = 0.0f32;
        for name in &outputs {
            right = right.max(measure(name, PORT_TEXT));
        }
        let tile = tiles && kind.is_some_and(previewed);
        let width = (left + right + 2.0 * PAD + GAP)
            .max(measure(&title, TITLE_TEXT) + 2.0 * PAD)
            .max(MIN_WIDTH)
            .max(if tile { TILE + 2.0 * PAD } else { 0.0 })
            .ceil();
        let valued = kind.is_some_and(|k| value_width(k) > 0.0);
        let rows = inputs.len().max(outputs.len()).max(usize::from(valued));
        let room = if tile { TILE + 2.0 * TILE_ROOM } else { 0.0 };
        out.push(NodeBox {
            id: id.clone(),
            kind,
            title,
            at: node.at,
            size: [width, TITLE + ROW * rows as f32 + PAD * 0.5 + room],
            inputs,
            outputs,
            tile,
        });
    }
}

/// What lies under a canvas point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    /// The empty canvas.
    Canvas,
    /// Box `k`'s title or body.
    Node(usize),
    /// Box `k`'s input `i`.
    Input(usize, usize),
    /// Box `k`'s output `o`.
    Output(usize, usize),
    /// Frame `k`'s title bar (frames in their ids' order, E10-3e-2b).
    Frame(usize),
    /// Frame `k`'s corner grip.
    FrameCorner(usize),
}

/// Point `t` (0 to 1) along the link from output point `a` to input point `b`: a cubic
/// curve leaving `a` and arriving at `b` level, its handles half the run out (at least 40
/// units, so a link going back loops around its boxes).
pub(crate) fn link_point(a: [f32; 2], b: [f32; 2], t: f32) -> [f32; 2] {
    let handle = ((b[0] - a[0]).abs() * 0.5).max(40.0);
    let (c1, c2) = ([a[0] + handle, a[1]], [b[0] - handle, b[1]]);
    let u = 1.0 - t;
    let w = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
    [
        w[0] * a[0] + w[1] * c1[0] + w[2] * c2[0] + w[3] * b[0],
        w[0] * a[1] + w[1] * c1[1] + w[2] * c2[1] + w[3] * b[1],
    ]
}

/// How far point `p` is from the segment `a`–`b`.
pub(crate) fn segment_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let length = dx * dx + dy * dy;
    let t = if length > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p[0] - (a[0] + t * dx)).hypot(p[1] - (a[1] + t * dy))
}

/// Whether node `node`'s value comes from `target`: `target` is `node`, or one of the nodes
/// its inputs are linked from, or theirs, up the graph.
pub(crate) fn depends_on(graph: &Graph, node: &str, target: &str) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n == target {
            return true;
        }
        if !seen.insert(n) {
            continue;
        }
        for (to, from) in &graph.links {
            if to.node == n {
                stack.push(from.node.as_str());
            }
        }
    }
    false
}

/// Output `from` linked into input `to` (replacing the link `to` had); refused, with why,
/// where it would loop back.
///
/// # Errors
///
/// The link would feed a node from itself.
pub(crate) fn connect(graph: &mut Graph, from: PortRef, to: PortRef) -> Result<(), &'static str> {
    if from.node == to.node {
        return Err("A node cannot feed itself");
    }
    if depends_on(graph, &from.node, &to.node) {
        return Err("That link would loop back into its own node");
    }
    graph.links.insert(to, from);
    Ok(())
}

/// A new node's id: its kind's name and the first number free (`multiply2`).
pub(crate) fn fresh_id(graph: &Graph, kind: &str) -> String {
    (1..)
        .map(|n| format!("{kind}{n}"))
        .find(|id| !graph.nodes.contains_key(id))
        .unwrap_or_default()
}

/// A parameter's name no node of `graph` has: `name`, else `name_2`, `name_3`…
fn fresh_param(graph: &Graph, name: &str) -> String {
    let taken = |p: &str| graph.nodes.values().any(|n| n.param == p);
    if !taken(name) {
        return name.to_owned();
    }
    (2..)
        .map(|n| format!("{name}_{n}"))
        .find(|p| !taken(p))
        .unwrap_or_default()
}

/// Whether `v` is `one` in every component, to within rounding.
fn is(v: &[f32], one: f32) -> bool {
    v.iter().all(|x| (x - one).abs() < 1e-4)
}

/// A new graph drawing `maps`, a glTF material's own look (ADR 0123 E10-3d-2): its maps
/// where `image` finds their files in the project (an image's path from the glTF's folder
/// to its reference and its path in the project), each times its factor where that is not
/// 1, the factors alone where there is no map; the standard material's grey, roughness
/// 0.5 and no metal without `maps`. The images found come back with their paths (the
/// file's references); the maps of data (metallic and roughness, occlusion) are named
/// too, for the material file to say they are not colours.
pub(crate) fn starter(
    maps: Option<&OutlineMaps>,
    image: &mut dyn FnMut(&str) -> Option<(AssetRef, String)>,
) -> (Graph, Vec<(AssetRef, String)>, Vec<&'static str>) {
    let standard = OutlineMaps {
        base_color: [0.8, 0.8, 0.8, 1.0],
        metallic: 0.0,
        roughness: 0.5,
        ..OutlineMaps::default()
    };
    let m = maps.unwrap_or(&standard);
    let mut g = Graph::new(Model::Lit);
    let mut images = Vec::new();
    let mut data = Vec::new();
    // Map `uri` as texture node `id` (parameter `param`) at `at`, if its file is found.
    let mut map = |g: &mut Graph, id: &str, param: &str, uri: &Option<String>, at: [f32; 2]| {
        let (texture, path) = uri.as_deref().and_then(&mut *image)?;
        g.add(
            id,
            Node::new("texture", at).named(param).with_texture(texture),
        );
        images.push((texture, path));
        Some(PortRef::new(id, "rgba"))
    };
    let out = |port: &str| PortRef::new(OUTPUT, port);
    let (maps_x, mid_x) = (-560.0, -300.0);
    // The base colour: its map times its factor, or the factor.
    match map(
        &mut g,
        "base_map",
        "base_color_map",
        &m.base_color_image,
        [maps_x, 0.0],
    ) {
        Some(rgba) if is(&m.base_color, 1.0) => {
            g.link(rgba, out("base"));
        }
        Some(rgba) => {
            g.add(
                "tint",
                Node::new("color", [maps_x, 130.0])
                    .with_value(m.base_color)
                    .named("tint"),
            );
            g.add("base", Node::new("multiply", [mid_x, 0.0]));
            g.link(rgba, PortRef::new("base", "a"))
                .link(PortRef::new("tint", "rgba"), PortRef::new("base", "b"))
                .link(PortRef::new("base", "out"), out("base"));
        }
        None => {
            g.add(
                "base_color",
                Node::new("color", [mid_x, 0.0])
                    .with_value(m.base_color)
                    .named("base_color"),
            );
            g.link(PortRef::new("base_color", "rgba"), out("base"));
        }
    }
    // Roughness (green) and metal (blue): the map's channels times their factors.
    let mr = map(
        &mut g,
        "mr_map",
        "metallic_roughness_map",
        &m.metallic_roughness_image,
        [maps_x, 260.0],
    );
    if mr.is_some() {
        data.push("metallic_roughness_map");
    }
    for (term, channel, factor, y) in [
        ("roughness", "g", m.roughness, 260.0),
        ("metallic", "b", m.metallic, 390.0),
    ] {
        let number = |g: &mut Graph, at: [f32; 2]| {
            g.add(
                term,
                Node::new("number", at)
                    .with_value([factor, 0.0, 0.0, 0.0])
                    .named(term),
            );
            PortRef::new(term, "value")
        };
        let value = match (&mr, (factor - 1.0).abs() < 1e-4) {
            (Some(_), true) => PortRef::new("mr_map", channel),
            (Some(_), false) => {
                let k = number(&mut g, [maps_x, y + 130.0]);
                let times = format!("{term}_times");
                g.add(&times, Node::new("multiply", [mid_x, y]));
                g.link(PortRef::new("mr_map", channel), PortRef::new(&times, "a"))
                    .link(k, PortRef::new(&times, "b"));
                PortRef::new(&times, "out")
            }
            (None, _) => number(&mut g, [mid_x, y]),
        };
        g.link(value, out(term));
    }
    // The normal map through a Normal Map node.
    if let Some(rgba) = map(
        &mut g,
        "normal_tex",
        "normal_map",
        &m.normal_image,
        [maps_x, 650.0],
    ) {
        g.add("normal", Node::new("normal_map", [mid_x, 650.0]));
        g.link(rgba, PortRef::new("normal", "color"))
            .link(PortRef::new("normal", "normal"), out("normal"));
    }
    // Emission: its map times its colour, or the colour where it glows.
    let glow = [m.emissive[0], m.emissive[1], m.emissive[2], 1.0];
    match map(
        &mut g,
        "emissive_map",
        "emissive_map",
        &m.emissive_image,
        [maps_x, 780.0],
    ) {
        Some(_) if is(&m.emissive, 1.0) => {
            g.link(PortRef::new("emissive_map", "rgb"), out("emission"));
        }
        Some(_) => {
            g.add(
                "glow",
                Node::new("color", [maps_x, 910.0])
                    .with_value(glow)
                    .named("emission"),
            );
            g.add("emission", Node::new("multiply", [mid_x, 780.0]));
            g.link(
                PortRef::new("emissive_map", "rgb"),
                PortRef::new("emission", "a"),
            )
            .link(PortRef::new("glow", "rgba"), PortRef::new("emission", "b"))
            .link(PortRef::new("emission", "out"), out("emission"));
        }
        None if !is(&m.emissive, 0.0) => {
            g.add(
                "glow",
                Node::new("color", [mid_x, 780.0])
                    .with_value(glow)
                    .named("emission"),
            );
            g.link(PortRef::new("glow", "rgba"), out("emission"));
        }
        None => {}
    }
    // Ambient occlusion (red).
    if map(
        &mut g,
        "occlusion_map",
        "occlusion_map",
        &m.occlusion_image,
        [maps_x, 1040.0],
    )
    .is_some()
    {
        data.push("occlusion_map");
        g.link(PortRef::new("occlusion_map", "r"), out("occlusion"));
    }
    (g, images, data)
}

/// The kinds the node search offers, in the library's order (its groups together): all
/// but the surface (a graph has one); for a link drawn out of an output (`Some(true)`)
/// those with an input, into an input (`Some(false)`) those with an output.
pub(crate) fn addable(from: Option<bool>, function: bool) -> impl Iterator<Item = &'static Kind> {
    library::KINDS.iter().filter(move |k| {
        // A function's inputs and outputs in a function only (E10-3g-2b).
        let port = matches!(k.body, Body::FunctionInput(_) | Body::FunctionOutput(_));
        k.name != OUTPUT_KIND
            && (!port || function)
            && match from {
                Some(true) => !k.inputs.is_empty(),
                Some(false) => !k.outputs.is_empty(),
                None => true,
            }
    })
}

/// What Make Function makes of a graph's selection (E10-3g-2b, [`GraphDoc::function_of_selection`]).
#[derive(Debug, Clone)]
pub(crate) struct MadeFunction {
    /// The function's graph, and the paths of the references its nodes make.
    pub(crate) function: Graph,
    pub(crate) paths: AssetPaths,
    /// Its ports, as its calls keep them.
    pub(crate) signature: String,
    /// The nodes it was made of.
    pub(crate) nodes: BTreeSet<String>,
    /// Its inputs: what each reads outside, and its name; its outputs: the port inside
    /// each gives, and its name.
    pub(crate) inputs: Vec<(PortRef, String)>,
    pub(crate) outputs: Vec<(PortRef, String)>,
    /// Where its call goes.
    pub(crate) at: [f32; 2],
}

/// A node id's base from a function's name (`New Function`: `new_function`).
fn id_base(name: &str) -> String {
    let base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let base = base.trim_matches('_');
    if base.is_empty() || base.starts_with(|c: char| c.is_ascii_digit()) {
        String::from("function")
    } else {
        base.to_owned()
    }
}

/// What `[Node]` functions `functions` (by module) make call `node`: its function's name
/// and signature (E10-3g-3b), where they know it and a node can call it.
pub(crate) fn node_function_call<'f>(
    functions: &'f BTreeMap<AssetId, Vec<crate::node_functions::NodeFunction>>,
    node: &Node,
) -> Option<(&'f str, &'f str)> {
    let f = functions
        .get(&node.module.file)?
        .iter()
        .find(|f| f.name == node.function)?;
    Some((f.name.as_str(), f.signature.as_ref().ok()?.as_str()))
}

/// Whether the file at `path` is a function's (`.mfunc`, E10-3g-2b).
pub(crate) fn is_function(path: &str) -> bool {
    path.rsplit('.')
        .next()
        .is_some_and(|e| e.eq_ignore_ascii_case("mfunc"))
}

/// The text of `graph`'s file of id `id`, a function's where `function`.
fn file_text(function: bool, graph: &Graph, id: AssetId, paths: &AssetPaths) -> String {
    let file = GraphFile::of(graph);
    if function {
        file.to_function_text(id, Some(paths))
    } else {
        file.to_text(id, Some(paths))
    }
}

/// A graph's file's text read (a function's where `function`): its graph and what reading
/// it reported.
fn read_text(function: bool, text: &str) -> Result<(Graph, engine_scene::ValueReport), String> {
    if function {
        let (file, report) = GraphFile::parse_function(text).map_err(|e| e.to_string())?;
        Ok((file.function()?, report))
    } else {
        let (file, report) = GraphFile::parse(text).map_err(|e| e.to_string())?;
        Ok((file.graph()?, report))
    }
}

/// A kind's ports as the node search shows them beside its name: `a, b → out`.
pub(crate) fn ports_text(kind: &Kind) -> String {
    let mut out = String::new();
    for (i, input) in kind.inputs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(input.name);
    }
    out.push_str(if kind.inputs.is_empty() {
        "→ "
    } else {
        " → "
    });
    for (i, output) in kind.outputs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(output.name);
    }
    out
}

/// The port among `ports` a link carrying `ty` best meets: one as wide, else one as wide as
/// what is linked (dynamic), else the first.
fn best_port(ports: &[(&'static str, PortType)], ty: PortType) -> Option<&'static str> {
    ports
        .iter()
        .find(|(_, t)| *t == ty)
        .or_else(|| ports.iter().find(|(_, t)| *t == PortType::Dynamic))
        .or_else(|| ports.first())
        .map(|(name, _)| *name)
}

/// A texture parameter's name from its image's file (`textures/Wood Grain.png`:
/// `wood_grain`): letters and digits, `_` between words.
fn param_of(path: &str) -> String {
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .split('.')
        .next()
        .unwrap_or("");
    let mut out = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert_str(0, "texture_");
    }
    out
}

/// A row of the inspector while a graph's tab is shown (ADR 0123 E10-3f-1): of the node
/// selected alone, or of the graph with none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeRow {
    /// The graph's shading model: which of the surface's terms it reads.
    Model,
    /// The node's parameter: a row of its materials by that name (none: compiled in).
    Param,
    /// A constant's components, as many as it is wide.
    Value(u8),
    /// An input's own value while nothing is linked to it (an input its kind fills with a
    /// value): its name, and how many numbers it shows (one for a dynamic input, which
    /// fills every component with it).
    Input(&'static str, u8),
    /// A texture node's own image.
    Texture,
    /// The node's note.
    Comment,
    /// A frame's title (E10-3e-2b).
    Title,
    /// An Expression node's Slang (E10-3g-1).
    Code,
    /// A function's input's or output's name, its port's (E10-3g-2b).
    Name,
}

impl NodeRow {
    /// Its name in the inspector (`colour`: the node is a colour's).
    pub(crate) fn name(self, colour: bool) -> &'static str {
        match self {
            Self::Model => "Shading model",
            Self::Param => "Parameter",
            Self::Value(_) if colour => "Color",
            Self::Value(_) => "Value",
            Self::Input(name, _) => name,
            Self::Texture => "Image",
            Self::Comment => "Note",
            Self::Title => "Title",
            Self::Code => "Code",
            Self::Name => "Name",
        }
    }

    /// Its numbers' labels: a colour's channels, a vector's axes, none for one number.
    pub(crate) fn labels(self, colour: bool) -> &'static [&'static str] {
        match self {
            Self::Value(_) | Self::Input(..) if colour => &["R", "G", "B", "A"],
            Self::Value(w) | Self::Input(_, w) => match w {
                2 => &["X", "Y"],
                3 => &["X", "Y", "Z"],
                4 => &["X", "Y", "Z", "W"],
                _ => &[],
            },
            _ => &[],
        }
    }
}

/// The shading models' names in [`Model::ALL`]'s order: the model's choice.
pub(crate) const MODEL_NAMES: [&str; 8] = [
    "lit",
    "unlit",
    "clearcoat",
    "cloth",
    "hair",
    "toon",
    "sheen",
    "sheen_clearcoat",
];

/// How many numbers an input of type `ty` shows.
fn shown_width(ty: PortType) -> u8 {
    match ty {
        PortType::Float(w) => w,
        PortType::Dynamic => 1,
    }
}

/// Whether `name` can name a parameter: letters, digits and `_`, not a digit first (a
/// Slang field's name).
fn is_param_name(name: &str) -> bool {
    name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// The editing build of `doc`'s graph for its preview (ADR 0123 E10-3f-2): its module's
/// text (every number a parameter, so a value's change changes no code), and the material
/// file drawing module `module` (at `module_path` in the project) with every parameter's
/// value and every texture node's image (a normal map's where it feeds a Normal Map node),
/// with the paths of its references.
///
/// # Errors
///
/// The graph cannot be generated (at its node).
pub(crate) fn preview_material(
    doc: &GraphDoc,
    root: &std::path::Path,
    open: &dyn Fn(AssetRef) -> Option<(Graph, AssetPaths)>,
    module: AssetRef,
    module_path: &str,
) -> Result<(Generated, MaterialFile, AssetPaths), GraphError> {
    let (generated, functions) = generated(doc, root, open, Build::Editing)?;
    let mut file = MaterialFile {
        shader: module,
        type_name: String::from(GRAPH_TYPE),
        look: MaterialLook::Own,
        ..MaterialFile::default()
    };
    let mut paths = AssetPaths::new();
    paths.insert(module, module_path);
    // The graph written: a function's nodes among its own, their images in the functions'
    // files.
    let written = &generated.expanded;
    for p in &generated.params {
        let Some(node) = written.nodes.get(&p.node) else {
            continue;
        };
        match p.ty {
            ParamType::Texture => {
                let Some(path) = functions.paths.get(node.texture) else {
                    continue;
                };
                paths.insert(node.texture, path);
                file.textures.insert(
                    p.name.clone(),
                    MaterialTexture {
                        image: node.texture,
                        encoding: encoding(written, &p.node),
                    },
                );
            }
            ParamType::Float(1) => {
                file.numbers.insert(p.name.clone(), p.value[0]);
            }
            ParamType::Float(w) => {
                file.vectors
                    .insert(p.name.clone(), p.value[..usize::from(w)].to_vec());
            }
        }
    }
    Ok((generated, file, paths))
}

/// `doc`'s graph generated in build `build` (the preview's, Show Slang's), and the functions
/// it calls with every path their files gave: those functions from their tabs (`open`, an
/// unsaved edit and all) or their files under `root` (E10-3g-2), the modules its and their
/// `[Node]` calls name from those paths (E10-3g-3). A function's own graph is the unlit
/// graph that previews it ([`function_preview`], E10-3g-2b).
///
/// # Errors
///
/// A function's file that cannot be read; the graph cannot be generated (at its node).
pub(crate) fn generated(
    doc: &GraphDoc,
    root: &std::path::Path,
    open: &dyn Fn(AssetRef) -> Option<(Graph, AssetPaths)>,
    build: Build,
) -> Result<(Generated, engine_assets::graph_module::Functions), GraphError> {
    let (wrapper, own);
    let (graph, paths) = if doc.function {
        wrapper = function_preview(doc)?;
        let mut with = doc.paths.clone();
        with.insert(AssetRef::file(doc.id), &doc.path);
        own = with;
        (&wrapper, &own)
    } else {
        (&doc.graph, &doc.paths)
    };
    // A function's tab previews what it holds, saved or not.
    let open = |r: AssetRef| {
        if doc.function && r == AssetRef::file(doc.id) {
            Some((doc.graph.clone(), doc.paths.clone()))
        } else {
            open(r)
        }
    };
    let functions = engine_assets::graph_module::read_functions_with(graph, paths, root, &open)
        .map_err(|e| {
            let at = e.path.as_ref().map(|p| format!("{}: ", p.display()));
            GraphError {
                node: None,
                message: format!("{}{}", at.unwrap_or_default(), e.message),
            }
        })?;
    let modules = |m| {
        functions
            .paths
            .get(m)
            .filter(|p| !p.is_empty())
            .map(engine_assets::graph_module::include_path)
    };
    let found = |f| functions.graphs.get(&f).cloned();
    let sources = engine_material_graph::Sources {
        modules: &modules,
        functions: &found,
    };
    // A function's tab previews its own nodes, as its preview's call makes them (E10-4g-3).
    let shown = if doc.function {
        engine_material_graph::Shown::Function(PREVIEW_CALL)
    } else {
        engine_material_graph::Shown::Graph
    };
    let generated =
        engine_material_graph::generate_shown(graph, GRAPH_TYPE, build, &sources, shown)?;
    Ok((generated, functions))
}

/// The ports of the function in the file at `path` as its calls keep them, or why it has
/// none (E10-3g-2b).
pub(crate) fn read_signature(path: &std::path::Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (file, _) = GraphFile::parse_function(&text).map_err(|e| e.to_string())?;
    let function = file.function()?;
    Ok(engine_material_graph::signature_of(&function)?.to_string())
}

/// A function's name: its file's (`shaders/tint.mfunc`: `tint`).
pub(crate) fn function_name(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rsplit_once('.').map_or(file, |(stem, _)| stem)
}

/// The call of the function in the graph its tab previews.
const PREVIEW_CALL: &str = "function";

/// The graph a function's tab previews (E10-3g-2b): unlit, a call of the function (its own
/// file's, as its tab holds it) whose first output is the surface.
fn function_preview(doc: &GraphDoc) -> Result<Graph, GraphError> {
    let unsaid = |message: String| GraphError {
        node: None,
        message,
    };
    let signature = engine_material_graph::signature_of(&doc.graph).map_err(unsaid)?;
    let first = signature
        .parameters
        .iter()
        .find(|p| p.direction == engine_material_graph::call::Direction::Out)
        .ok_or_else(|| unsaid(String::from("the function gives nothing back")))?;
    let mut call = Node::new(FUNCTION_KIND, [0.0, 0.0])
        .with_call(function_name(&doc.path), &signature.to_string());
    call.module = AssetRef::file(doc.id);
    let mut g = Graph::new(Model::Unlit);
    g.add(PREVIEW_CALL, call).link(
        PortRef::new(PREVIEW_CALL, &first.name),
        PortRef::new(OUTPUT, "base"),
    );
    Ok(g)
}

/// An Expression node's code as its title shows it: its first line, at most 24 characters.
fn short_code(code: &str) -> String {
    let line = code.lines().next().unwrap_or("").trim();
    let line = if line.is_empty() { "a" } else { line };
    match line.char_indices().nth(24) {
        Some((at, _)) => format!("{}…", line[..at].trim_end()),
        None => line.to_owned(),
    }
}

/// Slang's first error in `diagnostics` placed in the module whose file is named `file`
/// (E10-3g-1): the node the generator's line map `lines` gives its line (none for a line no
/// node made), and what it says: the detail at its place when Slang gives one (`undefined
/// identifier 'oops'.`), otherwise its title. Slang writes `error[E30015]: title`, then
/// `--> path:line:column`, the line's source and carets under the place, the detail after
/// them; warnings, and errors placed in other files, are passed over, but for an error in
/// a module a call names (`calls`: its file's name and the call, E10-3g-3b), which is that
/// call's.
pub(crate) fn slang_error<'a>(
    diagnostics: &'a str,
    file: &str,
    lines: &'a [Option<String>],
    calls: &'a [(String, String)],
) -> Option<(Option<&'a str>, &'a str)> {
    // The error being read (its title), whether its place was read, and its node once
    // that place is in the module.
    let mut title = None;
    let mut placed = false;
    let mut node: Option<Option<&str>> = None;
    for line in diagnostics.lines().map(str::trim) {
        let header = line.split_once('[').filter(|(kind, rest)| {
            !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_lowercase()) && rest.contains("]:")
        });
        if let Some((kind, rest)) = header {
            if let Some(at) = node {
                return Some((at, title.unwrap_or("")));
            }
            title = (kind == "error").then(|| rest.split_once("]:").map_or("", |(_, t)| t.trim()));
            placed = false;
        } else if let Some(place) = line.strip_prefix("--> ")
            && title.is_some()
            && !placed
        {
            placed = true;
            let mut parts = place.rsplitn(3, ':');
            let (_, number, path) = (parts.next(), parts.next(), parts.next());
            if let (Some(Ok(number)), Some(path)) = (number.map(str::parse::<usize>), path) {
                let name = path.rsplit(['/', '\\']).next();
                if name == Some(file) {
                    node = Some(lines.get(number.wrapping_sub(1)).and_then(Option::as_deref));
                } else if let Some((_, call)) = calls.iter().find(|(f, _)| Some(f.as_str()) == name)
                {
                    node = Some(Some(call.as_str()));
                }
            }
        } else if let Some(at) = node
            && let Some(rest) = line.strip_prefix('|').map(str::trim_start)
            && rest.starts_with('^')
        {
            let detail = rest.trim_start_matches('^').trim();
            return Some((
                at,
                if detail.is_empty() {
                    title.unwrap_or("")
                } else {
                    detail
                },
            ));
        }
    }
    node.map(|at| (at, title.unwrap_or("")))
}

/// Nodes `ids` deleted with their links, the surface kept: how many went.
pub(crate) fn delete(graph: &mut Graph, ids: &BTreeSet<String>) -> usize {
    let before = graph.nodes.len();
    graph
        .nodes
        .retain(|id, _| id == OUTPUT || !ids.contains(id));
    let nodes = &graph.nodes;
    graph
        .links
        .retain(|to, from| nodes.contains_key(&to.node) && nodes.contains_key(&from.node));
    before - graph.nodes.len()
}

/// The pointer over the canvas this frame, in panel pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Pointer {
    /// Where it is.
    pub(crate) at: Option<[f32; 2]>,
    /// It is over the canvas, and nothing over the canvas takes it: presses and the wheel
    /// are the canvas's.
    pub(crate) over: bool,
    /// The left button pressed this frame, and held.
    pub(crate) press: bool,
    pub(crate) held: bool,
    /// The middle or the right button pressed this frame, and held.
    pub(crate) pan_press: bool,
    pub(crate) pan_held: bool,
    /// Its move with the middle or the right button held, since the last frame.
    pub(crate) moved: [f32; 2],
    /// Wheel notches, up (in) positive.
    pub(crate) wheel: f32,
    /// Shift held.
    pub(crate) shift: bool,
    /// Ctrl held: a press on an output carries its links (a yank).
    pub(crate) ctrl: bool,
    /// Escape pressed.
    pub(crate) escape: bool,
}

/// A drag on the canvas under way.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Drag {
    /// The view moved by the middle or the right button.
    Pan,
    /// A frame's corner dragged: the frame, the canvas point pressed, its size then, and
    /// whether the pointer has left the press's spot (E10-3e-2b).
    Size {
        frame: String,
        from: [f32; 2],
        start: [f32; 2],
        moved: bool,
    },
    /// The selected nodes moved: the canvas point pressed, each node's place then, and
    /// whether the pointer has left the press's spot.
    Move {
        /// The canvas point pressed.
        from: [f32; 2],
        /// Each node's place then.
        starts: Vec<(String, [f32; 2])>,
        /// The frames moved with them, each's place then (E10-3e-2b).
        frames: Vec<(String, [f32; 2])>,
        /// The pointer has moved past [`CLICK_SLOP`].
        moved: bool,
    },
    /// A link drawn from port `from`: an output's, or an input's to be linked from an
    /// output; and the input it was lifted off, where it was one's.
    Link {
        /// The port it starts from.
        from: PortRef,
        /// `from` is an output.
        output: bool,
        /// The input it was lifted off.
        lifted: Option<PortRef>,
    },
    /// An output's links lifted off it (Ctrl+drag, egui-snarl's yank): to be carried to
    /// another output, or taken out.
    Yank {
        /// The output they came from.
        from: PortRef,
        /// The inputs it fed.
        inputs: Vec<PortRef>,
    },
    /// A box drawn from canvas point `from`; Shift adds what it meets to the selection.
    Select {
        /// Where it started.
        from: [f32; 2],
        /// It adds to the selection.
        add: bool,
    },
}

/// What a frame of the canvas did to the graph.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    /// An edit made: the step's name, the text before and after it.
    Edited {
        /// The step's name.
        name: &'static str,
        /// The text before it.
        before: String,
        /// The text after it.
        after: String,
    },
    /// An edit refused, and why.
    Refused(&'static str),
    /// A link dropped on the empty canvas: the node search asked for at canvas point `at`,
    /// the node it adds linked to `from` (a port, and whether it is an output).
    Search {
        /// Where the link was dropped.
        at: [f32; 2],
        /// The port it was drawn from.
        from: (PortRef, bool),
    },
}

/// A material graph's file as the editor holds it.
#[derive(Debug)]
pub(crate) struct GraphDoc {
    /// Its file's id.
    pub(crate) id: AssetId,
    /// Its path under the project, folders by `/`.
    pub(crate) path: String,
    /// The graph.
    pub(crate) graph: Graph,
    /// The paths its references were written with (its textures').
    pub(crate) paths: AssetPaths,
    /// Its text as the graph now is.
    pub(crate) text: String,
    /// Its text as its file holds it.
    pub(crate) saved: String,
    /// How many times it changed: its boxes are laid out again when it did.
    pub(crate) revision: u64,
    /// The canvas's view of it.
    pub(crate) view: View,
    /// The selected nodes, by id.
    pub(crate) selected: BTreeSet<String>,
    /// Its nodes' boxes as last laid out, in their ids' order.
    pub(crate) boxes: Vec<NodeBox>,
    /// Its calls' titles, from their functions' marks (module, function, title; E10-3g-3b).
    pub(crate) call_titles: Vec<(AssetRef, String, String)>,
    /// The revision they were laid out at, and whether with tiles.
    laid_out: Option<(u64, bool)>,
    /// Its nodes' previews show on the canvas (E10-4g): the editor's choice.
    pub(crate) tiles: bool,
    /// What the pointer is over, and the canvas point under it.
    pub(crate) hover: Hit,
    pub(crate) cursor: Option<[f32; 2]>,
    /// Closing was asked once with unsaved changes: asked again, it closes.
    pub(crate) closing: bool,
    /// It has been framed once (it opens with its nodes in view).
    pub(crate) framed: bool,
    /// A function's file (`.mfunc`, E10-3g-2b), not a material graph's: its inputs and
    /// outputs are its ports, it has no shading model nor surface.
    pub(crate) function: bool,
    /// The graph it was entered from by Tab on a call of it (E10-3g-2b): Tab goes back.
    pub(crate) entered_from: Option<AssetId>,
    /// Its tab's label, in its own room: its file's name, `*` after it while unsaved
    /// ([`Self::refresh_tab`]).
    pub(crate) tab: String,
    /// The revision its unsaved text was autosaved at, and when an autosave is due (as
    /// material files').
    pub(crate) autosaved: Option<u64>,
    pub(crate) due: Option<std::time::Instant>,
    /// What stopped it from generating when it was last generated (E10-3f-3), at its node
    /// when it has one.
    pub(crate) error: Option<GraphError>,
    /// The canvas's last press: when and where (a double-click on a link, E10-3e-2a).
    pub(crate) last_press: Option<(std::time::Instant, [f32; 2])>,
    /// The selected frames, by id (E10-3e-2b): apart from the nodes, as a file's ids for
    /// both could meet.
    pub(crate) selected_frames: BTreeSet<String>,
    /// An animation graph's file (`.agraph`, ADR 0124 E12-5d-1) in place of a material
    /// graph, with what its canvas shows ([`crate::agraph`]); `graph` stays empty then. Its
    /// tab, history, saves and autosaves are a material graph's.
    pub(crate) anim: Option<Box<crate::agraph::AnimDoc>>,
}

impl GraphDoc {
    /// The graph file at `path` under the project's `root`.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or is not a graph.
    pub(crate) fn open(root: &Path, path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
        // An animation graph's (ADR 0124 E12-5d-1).
        if crate::agraph::is_anim_graph(path) {
            let (file, report) =
                engine_scene::AnimGraphFile::parse(&text).map_err(|e| format!("{path}:{e}"))?;
            return Ok(Self::new_anim(
                report.id.unwrap_or_else(AssetId::new),
                path,
                file,
                report.assets,
            ));
        }
        let (graph, report) =
            read_text(is_function(path), &text).map_err(|e| format!("{path}: {e}"))?;
        Ok(Self::new(
            report.id.unwrap_or_else(AssetId::new),
            path,
            graph,
            report.assets,
        ))
    }

    /// Graph `graph` of file `id` at `path`, its references written with `paths`, as
    /// saved: unsaved only once edited.
    pub(crate) fn new(id: AssetId, path: &str, graph: Graph, paths: AssetPaths) -> Self {
        Self::with(id, path, graph, paths, None)
    }

    /// Animation graph `file` of file `id` at `path`, its references written with `paths`,
    /// as saved (ADR 0124 E12-5d-1).
    pub(crate) fn new_anim(
        id: AssetId,
        path: &str,
        file: engine_scene::AnimGraphFile,
        paths: AssetPaths,
    ) -> Self {
        let mut graph = Graph::new(Model::Lit);
        graph.nodes.clear();
        let anim = crate::agraph::AnimDoc::new(file);
        Self::with(id, path, graph, paths, Some(Box::new(anim)))
    }

    /// A graph's document, as saved: its text made from what it holds.
    fn with(
        id: AssetId,
        path: &str,
        graph: Graph,
        paths: AssetPaths,
        anim: Option<Box<crate::agraph::AnimDoc>>,
    ) -> Self {
        let mut doc = Self {
            id,
            path: path.to_owned(),
            graph,
            paths,
            saved: String::new(),
            text: String::new(),
            revision: 0,
            view: View::default(),
            selected: BTreeSet::new(),
            boxes: Vec::new(),
            call_titles: Vec::new(),
            laid_out: None,
            tiles: false,
            hover: Hit::Canvas,
            cursor: None,
            closing: false,
            framed: false,
            function: is_function(path),
            entered_from: None,
            tab: String::new(),
            autosaved: None,
            due: None,
            error: None,
            last_press: None,
            selected_frames: BTreeSet::new(),
            anim,
        };
        doc.text = doc.current_text();
        doc.saved.clone_from(&doc.text);
        doc
    }

    /// Its text as it now is: its graph's (a function's), or its animation graph's.
    fn current_text(&self) -> String {
        match &self.anim {
            Some(anim) => anim.file.to_text(self.id, Some(&self.paths)),
            None => file_text(self.function, &self.graph, self.id, &self.paths),
        }
    }

    /// Its tab's label written again where it changed (saved or unsaved): a steady frame
    /// writes nothing.
    pub(crate) fn refresh_tab(&mut self) {
        let dirty = self.dirty();
        if !self.tab.is_empty() && self.tab.ends_with(" *") == dirty {
            return;
        }
        let name = self.path.rsplit('/').next().unwrap_or(&self.path);
        self.tab.clear();
        self.tab.push_str(name);
        if dirty {
            self.tab.push_str(" *");
        }
    }

    /// Edited since it was saved.
    pub(crate) fn dirty(&self) -> bool {
        self.text != self.saved
    }

    /// `edit` applied to the graph: its text before and after, when it changed.
    pub(crate) fn edit(&mut self, edit: impl FnOnce(&mut Graph)) -> Option<(String, String)> {
        edit(&mut self.graph);
        self.settle()
    }

    /// After the graph changed in place (a drag's moves): laid out again, its text again,
    /// and whether that changed (its text before and after).
    pub(crate) fn settle(&mut self) -> Option<(String, String)> {
        self.revision += 1;
        let after = self.current_text();
        if after == self.text {
            return None;
        }
        if let Some(anim) = self.anim.as_deref_mut() {
            anim.refresh();
        }
        let before = core::mem::replace(&mut self.text, after.clone());
        Some((before, after))
    }

    /// Its text put back (undo, redo).
    ///
    /// # Errors
    ///
    /// A text that is not a graph.
    pub(crate) fn put_text(&mut self, text: &str) -> Result<(), String> {
        if let Some(anim) = self.anim.as_deref_mut() {
            let (file, report) = engine_scene::AnimGraphFile::parse(text)
                .map_err(|e| format!("{}:{e}", self.path))?;
            anim.put(file);
            self.paths = report.assets;
            self.text = text.to_owned();
            self.revision += 1;
            return Ok(());
        }
        let (graph, report) =
            read_text(self.function, text).map_err(|e| format!("{}: {e}", self.path))?;
        self.graph = graph;
        self.paths = report.assets;
        self.text = text.to_owned();
        self.revision += 1;
        let nodes = &self.graph.nodes;
        self.selected.retain(|id| nodes.contains_key(id));
        Ok(())
    }

    /// Saved: as its file now holds it.
    pub(crate) fn saved_now(&mut self) {
        self.saved.clone_from(&self.text);
        self.closing = false;
    }

    /// Its boxes laid out again where the graph changed since (`measure`: a text's width
    /// at a size).
    pub(crate) fn relayout(&mut self, measure: &mut dyn FnMut(&str, f32) -> f32) {
        if let Some(anim) = self.anim.as_deref_mut() {
            anim.relayout(self.revision, measure);
            return;
        }
        let key = (self.revision, self.tiles);
        if self.laid_out != Some(key) {
            layout(
                &self.graph,
                &self.call_titles,
                self.tiles,
                measure,
                &mut self.boxes,
            );
            self.laid_out = Some(key);
        }
    }

    /// Node `id`'s box.
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

    /// What lies under canvas point `p`, the top box first, a port within `reach` canvas
    /// units of its dot before the box it is on.
    pub(crate) fn hit(&self, p: [f32; 2], reach: f32) -> Hit {
        for k in self.order().rev() {
            let b = &self.boxes[k];
            // A reroute's pins reach half as far: its middle is grabbed to move it.
            let reach = if b.is_reroute() { reach * 0.5 } else { reach };
            let near = |q: [f32; 2]| (q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2) <= reach * reach;
            if let Some(i) = (0..b.inputs.len()).find(|&i| near(b.input_at(i))) {
                return Hit::Input(k, i);
            }
            if let Some(o) = (0..b.outputs.len()).find(|&o| near(b.output_at(o))) {
                return Hit::Output(k, o);
            }
            if b.contains(p) {
                return Hit::Node(k);
            }
        }
        // Under the boxes, the frames (the last on top): a title bar and a corner grip; their
        // insides are the canvas's (a box selection or a pan starts there).
        for (k, f) in self.graph.frames.values().enumerate().rev() {
            let hi = [f.at[0] + f.size[0], f.at[1] + f.size[1]];
            if p[0] < f.at[0] || p[1] < f.at[1] || p[0] > hi[0] || p[1] > hi[1] {
                continue;
            }
            if p[0] >= hi[0] - FRAME_GRIP && p[1] >= hi[1] - FRAME_GRIP {
                return Hit::FrameCorner(k);
            }
            if p[1] <= f.at[1] + FRAME_TITLE {
                return Hit::Frame(k);
            }
        }
        Hit::Canvas
    }

    /// A frame round the selected nodes' boxes (C, Unreal's comment box): room round them
    /// and a title bar, titled "Frame", and it selected alone; a step.
    pub(crate) fn frame_selection(&mut self) -> Option<Outcome> {
        let mut area: Option<[[f32; 2]; 2]> = None;
        for b in self.boxes.iter().filter(|b| self.selected.contains(&b.id)) {
            let (lo, hi) = (b.at, [b.at[0] + b.size[0], b.at[1] + b.size[1]]);
            area = Some(area.map_or([lo, hi], |[a, z]| {
                [
                    [a[0].min(lo[0]), a[1].min(lo[1])],
                    [z[0].max(hi[0]), z[1].max(hi[1])],
                ]
            }));
        }
        let Some([lo, hi]) = area else {
            return Some(Outcome::Refused("Select the nodes to put in a frame"));
        };
        let id = (1..)
            .map(|n| format!("frame{n}"))
            .find(|id| !self.graph.frames.contains_key(id))?;
        let frame = Frame {
            at: [
                (lo[0] - FRAME_ROOM).round(),
                (lo[1] - FRAME_ROOM - FRAME_TITLE).round(),
            ],
            size: [
                (hi[0] - lo[0] + 2.0 * FRAME_ROOM).round(),
                (hi[1] - lo[1] + 2.0 * FRAME_ROOM + FRAME_TITLE).round(),
            ],
            title: String::from("Frame"),
        };
        let (before, after) = self.edit(|g| {
            g.frames.insert(id.clone(), frame);
        })?;
        self.selected.clear();
        self.selected_frames = [id].into_iter().collect();
        Some(Outcome::Edited {
            name: "Frame Selection",
            before,
            after,
        })
    }

    /// The frame the inspector shows: the one selected alone, with no node.
    pub(crate) fn inspected_frame(&self) -> Option<&str> {
        if !self.selected.is_empty() {
            return None;
        }
        let mut ids = self.selected_frames.iter();
        match (ids.next(), ids.next()) {
            (Some(id), None) if self.graph.frames.contains_key(id) => Some(id),
            _ => None,
        }
    }

    /// The inspected frame's title: `text`.
    pub(crate) fn set_title(&mut self, text: &str) -> Option<(String, String)> {
        let id = self.inspected_frame()?.to_owned();
        self.edit(|g| {
            if let Some(f) = g.frames.get_mut(&id) {
                text.clone_into(&mut f.title);
            }
        })
    }

    /// The link under canvas point `p` (within `reach` of its curve), by the input it
    /// feeds.
    pub(crate) fn link_at(&self, p: [f32; 2], reach: f32) -> Option<PortRef> {
        self.graph.links.iter().find_map(|(into, from)| {
            let (a, b) = (self.port_point(from, true)?, self.port_point(into, false)?);
            let mut last = a;
            let near = (1..=LINK_SEGMENTS).any(|s| {
                let next = link_point(a, b, s as f32 / LINK_SEGMENTS as f32);
                let d = segment_distance(p, last, next);
                last = next;
                d <= reach
            });
            near.then(|| into.clone())
        })
    }

    /// What output `port` of node `node` carries: its kind's type, or through reroutes
    /// (each passing its input on) what feeds the first node that is not one; nothing
    /// linked, any.
    pub(crate) fn carried_by(&self, node: &str, port: &str) -> PortType {
        let (mut node, mut port) = (node, port);
        // A chain of reroutes is followed back; a loop of them stops.
        for _ in 0..64 {
            let Some(k) = self.box_of(node) else {
                break;
            };
            let b = &self.boxes[k];
            if !b.is_reroute() {
                return b
                    .output(port)
                    .map_or(PortType::Dynamic, |o| b.output_type(o));
            }
            let Some((_, from)) = self
                .graph
                .links
                .iter()
                .find(|(to, _)| to.node == node && to.port == "in")
            else {
                break;
            };
            (node, port) = (&from.node, &from.port);
        }
        PortType::Dynamic
    }

    /// A reroute put on the link into `to`, its middle at canvas point `at`, the link
    /// passing through it, and it selected alone (a double-click on the link, Unreal's).
    pub(crate) fn add_reroute(&mut self, to: &PortRef, at: [f32; 2]) -> Option<Outcome> {
        let from = self.graph.links.get(to)?.clone();
        let id = fresh_id(&self.graph, REROUTE_KIND);
        let place = [
            (at[0] - REROUTE[0] * 0.5).round(),
            (at[1] - REROUTE[1] * 0.5).round(),
        ];
        let (before, after) = self.edit(|g| {
            g.add(&id, Node::new(REROUTE_KIND, place));
            g.links.insert(PortRef::new(&id, "in"), from);
            g.links.insert(to.clone(), PortRef::new(&id, "out"));
        })?;
        self.selected = [id].into_iter().collect();
        Some(Outcome::Edited {
            name: "Add Reroute",
            before,
            after,
        })
    }

    /// Where port `port` is (an output's where `output`), if its node shows it.
    pub(crate) fn port_point(&self, port: &PortRef, output: bool) -> Option<[f32; 2]> {
        let b = &self.boxes[self.box_of(&port.node)?];
        if output {
            b.output(&port.port).map(|o| b.output_at(o))
        } else {
            b.input(&port.port).map(|i| b.input_at(i))
        }
    }

    /// The canvas rectangle around the selected nodes' boxes, or all of them with none
    /// selected.
    pub(crate) fn extent(&self) -> Option<[[f32; 2]; 2]> {
        let any = self.selected.iter().any(|id| self.box_of(id).is_some());
        let mut area: Option<[[f32; 2]; 2]> = None;
        for (k, b) in self.boxes.iter().enumerate() {
            if any && !self.is_selected(k) {
                continue;
            }
            let (lo, hi) = (b.at, [b.at[0] + b.size[0], b.at[1] + b.size[1]]);
            area = Some(match area {
                None => [lo, hi],
                Some([a, z]) => [
                    [a[0].min(lo[0]), a[1].min(lo[1])],
                    [z[0].max(hi[0]), z[1].max(hi[1])],
                ],
            });
        }
        area
    }

    /// The view framing the selected nodes (all of them, none selected) in a panel of
    /// `size`.
    pub(crate) fn frame(&mut self, size: [f32; 2]) {
        let area = match &self.anim {
            Some(anim) => anim.extent(),
            None => self.extent(),
        };
        if let Some(area) = area {
            self.view.frame(area, size);
        }
    }

    /// The selected nodes deleted, the surface kept, as a step.
    pub(crate) fn delete_selected(&mut self) -> Option<Outcome> {
        if self.selected.is_empty() && self.selected_frames.is_empty() {
            return None;
        }
        if self.selected_frames.is_empty() && self.selected.iter().all(|id| id == OUTPUT) {
            return Some(Outcome::Refused("The surface stays: every graph has one"));
        }
        let selected = core::mem::take(&mut self.selected);
        // Frames go alone: the nodes inside them stay.
        let frames = core::mem::take(&mut self.selected_frames);
        let mut gone = 0;
        let (before, after) = self.edit(|g| {
            gone = delete(g, &selected);
            g.frames.retain(|id, _| !frames.contains(id));
        })?;
        if selected.contains(OUTPUT) {
            self.selected.insert(OUTPUT.to_owned());
        }
        let name = match (gone, frames.len()) {
            (0, 1) => "Delete Frame",
            (0, _) => "Delete Frames",
            (1, 0) => "Delete Node",
            _ => "Delete Nodes",
        };
        Some(Outcome::Edited {
            name,
            before,
            after,
        })
    }

    /// The node the inspector shows: the one selected alone.
    pub(crate) fn inspected(&self) -> Option<&str> {
        let mut ids = self.selected.iter();
        match (ids.next(), ids.next()) {
            (Some(id), None) if self.graph.nodes.contains_key(id) => Some(id),
            _ => None,
        }
    }

    /// The inspected node's kind.
    pub(crate) fn inspected_kind(&self) -> Option<&'static Kind> {
        engine_material_graph::node_kind(self.graph.nodes.get(self.inspected()?)?).ok()
    }

    /// The inspector's rows into `out` (its room kept): the inspected node's, or the
    /// graph's own (its shading model) with none.
    pub(crate) fn rows(&self, out: &mut Vec<NodeRow>) {
        out.clear();
        let Some(id) = self.inspected() else {
            if self.inspected_frame().is_some() {
                out.push(NodeRow::Title);
            } else if !self.function {
                out.push(NodeRow::Model);
            }
            return;
        };
        let node = &self.graph.nodes[id];
        let Ok(kind) = engine_material_graph::node_kind(node) else {
            out.push(NodeRow::Comment);
            return;
        };
        match kind.body {
            Body::Constant(w) => {
                out.push(NodeRow::Param);
                out.push(NodeRow::Value(w));
            }
            Body::Texture => {
                out.push(NodeRow::Param);
                out.push(NodeRow::Texture);
            }
            Body::Expression => out.push(NodeRow::Code),
            // A function's port: its name, an input's value where a call leaves it unlinked
            // (E10-3g-2b).
            Body::FunctionInput(w) => {
                out.push(NodeRow::Name);
                out.push(NodeRow::Value(w));
            }
            Body::FunctionOutput(_) => out.push(NodeRow::Name),
            _ => {}
        }
        for input in kind.inputs {
            let linked = self
                .graph
                .links
                .keys()
                .any(|to| to.node == id && to.port == input.name);
            let shown = kind.name != OUTPUT_KIND || surface_reads(self.graph.model, input.name);
            if !linked && shown && matches!(input.fill, Fill::Value(_)) {
                out.push(NodeRow::Input(input.name, shown_width(input.ty)));
            }
        }
        out.push(NodeRow::Comment);
    }

    /// Row `row`'s numbers for the inspected node: a constant's components, an input's
    /// own value or its kind's.
    pub(crate) fn numbers(&self, row: NodeRow) -> [f32; 4] {
        let Some(node) = self.inspected().and_then(|id| self.graph.nodes.get(id)) else {
            return [0.0; 4];
        };
        match row {
            NodeRow::Value(_) => node.value,
            NodeRow::Input(name, _) => node.inputs.get(name).copied().unwrap_or_else(|| {
                match engine_material_graph::node_kind(node)
                    .ok()
                    .and_then(|k| k.input(name))
                    .map(|i| i.fill)
                {
                    Some(Fill::Value(v)) => v,
                    _ => [0.0; 4],
                }
            }),
            _ => [0.0; 4],
        }
    }

    /// Whether row `row` holds other than its default: its reset button.
    pub(crate) fn changed(&self, row: NodeRow) -> bool {
        if row == NodeRow::Model {
            return self.graph.model != Model::Lit;
        }
        if row == NodeRow::Title {
            return self
                .inspected_frame()
                .and_then(|id| self.graph.frames.get(id))
                .is_some_and(|f| f.title != "Frame");
        }
        let Some(node) = self.inspected().and_then(|id| self.graph.nodes.get(id)) else {
            return false;
        };
        match row {
            NodeRow::Model | NodeRow::Title => false,
            NodeRow::Param => !node.param.is_empty(),
            // A port keeps a name: no reset to none.
            NodeRow::Name => false,
            NodeRow::Value(w) => node.value[..usize::from(w)].iter().any(|v| *v != 0.0),
            NodeRow::Input(name, _) => node.inputs.contains_key(name),
            NodeRow::Texture => !node.texture.is_none(),
            NodeRow::Comment => !node.comment.is_empty(),
            NodeRow::Code => !node.code.is_empty(),
        }
    }

    /// Row `row`'s shown texts into `texts` from `at` on (each number's, a name, an image's
    /// file), their rooms reused: how many.
    pub(crate) fn row_texts(&self, row: NodeRow, texts: &mut Vec<String>, at: usize) -> usize {
        let node = self.inspected().and_then(|id| self.graph.nodes.get(id));
        let n = match row {
            NodeRow::Value(w) | NodeRow::Input(_, w) => usize::from(w),
            _ => 1,
        };
        while texts.len() < at + n {
            texts.push(String::new());
        }
        let numbers = self.numbers(row);
        for (k, text) in texts[at..at + n].iter_mut().enumerate() {
            match row {
                NodeRow::Value(_) | NodeRow::Input(..) => value_text(text, &numbers[k..=k]),
                NodeRow::Model => {
                    text.clear();
                    text.push_str(self.graph.model.name());
                }
                NodeRow::Param | NodeRow::Name => {
                    text.clear();
                    text.push_str(node.map_or("", |n| n.param.as_str()));
                }
                NodeRow::Comment => {
                    text.clear();
                    text.push_str(node.map_or("", |n| n.comment.as_str()));
                }
                NodeRow::Title => {
                    text.clear();
                    let frame = self
                        .inspected_frame()
                        .and_then(|id| self.graph.frames.get(id));
                    text.push_str(frame.map_or("", |f| f.title.as_str()));
                }
                NodeRow::Code => {
                    text.clear();
                    text.push_str(node.map_or("", |n| n.code.as_str()));
                }
                NodeRow::Texture => {
                    text.clear();
                    let path = node.and_then(|n| self.paths.get(n.texture));
                    text.push_str(path.map_or("none", |p| p.rsplit('/').next().unwrap_or(p)));
                }
            }
        }
        n
    }

    /// The inspected node changed by `edit`: its text before and after, when it changed.
    fn edit_inspected(&mut self, edit: impl FnOnce(&mut Node)) -> Option<(String, String)> {
        let id = self.inspected()?.to_owned();
        self.edit(|g| {
            if let Some(node) = g.nodes.get_mut(&id) {
                edit(node);
            }
        })
    }

    /// Row `row` of the inspected node set to `values` (a dynamic input's first number
    /// filling every component).
    pub(crate) fn set_numbers(
        &mut self,
        row: NodeRow,
        values: [f32; 4],
    ) -> Option<(String, String)> {
        let dynamic = match row {
            NodeRow::Input(name, _) => self
                .inspected_kind()
                .and_then(|k| k.input(name))
                .is_some_and(|i| i.ty == PortType::Dynamic),
            _ => false,
        };
        let values = if dynamic { [values[0]; 4] } else { values };
        self.edit_inspected(|node| match row {
            NodeRow::Value(_) => node.value = values,
            NodeRow::Input(name, _) => {
                node.inputs.insert(name.to_owned(), values);
            }
            _ => {}
        })
    }

    /// The inspected node's parameter named `name` (empty for none: compiled in).
    ///
    /// # Errors
    ///
    /// A name a Slang field cannot have, or one another node's parameter has.
    pub(crate) fn set_param(
        &mut self,
        name: &str,
    ) -> Result<Option<(String, String)>, &'static str> {
        if !is_param_name(name) {
            return Err("A parameter's name is letters, digits and `_`, not a digit first");
        }
        let id = self.inspected().unwrap_or_default();
        let taken = !name.is_empty()
            && self
                .graph
                .nodes
                .iter()
                .any(|(other, n)| other != id && n.param == name);
        if taken {
            return Err("Another node's parameter has that name");
        }
        Ok(self.edit_inspected(|node| name.clone_into(&mut node.param)))
    }

    /// The inspected node's note.
    pub(crate) fn set_comment(&mut self, text: &str) -> Option<(String, String)> {
        self.edit_inspected(|node| text.clone_into(&mut node.comment))
    }

    /// The inspected Expression node's Slang: `text`.
    pub(crate) fn set_code(&mut self, text: &str) -> Option<(String, String)> {
        self.edit_inspected(|node| text.clone_into(&mut node.code))
    }

    /// The inspected texture node's own image: `image`, at `path` in the project.
    pub(crate) fn set_texture(&mut self, image: AssetRef, path: &str) -> Option<(String, String)> {
        self.paths.insert(image, path);
        self.edit_inspected(|node| node.texture = image)
    }

    /// The graph's shading model: [`MODEL_NAMES`]'s `k`th.
    pub(crate) fn set_model(&mut self, k: usize) -> Option<(String, String)> {
        let model = *Model::ALL.get(k)?;
        self.edit(|g| g.model = model)
    }

    /// Row `row` back to its default: no parameter, a zero value, the input's kind's
    /// value, no image, no note, the lit model.
    pub(crate) fn reset(&mut self, row: NodeRow) -> Option<(String, String)> {
        if row == NodeRow::Model {
            return self.edit(|g| g.model = Model::Lit);
        }
        if row == NodeRow::Title {
            return self.set_title("Frame");
        }
        self.edit_inspected(|node| match row {
            NodeRow::Param => node.param.clear(),
            NodeRow::Name => {}
            NodeRow::Value(_) => node.value = [0.0; 4],
            NodeRow::Input(name, _) => {
                node.inputs.remove(name);
            }
            NodeRow::Texture => node.texture = AssetRef::NONE,
            NodeRow::Comment => node.comment.clear(),
            NodeRow::Code => node.code.clear(),
            NodeRow::Model | NodeRow::Title => {}
        })
    }

    /// What port `port` of its node carries (an output's where `output`).
    fn port_type(&self, port: &PortRef, output: bool) -> Option<PortType> {
        let kind = engine_material_graph::node_kind(self.graph.nodes.get(&port.node)?).ok()?;
        if output {
            kind.output(&port.port).map(|o| o.ty)
        } else {
            kind.input(&port.port).map(|i| i.ty)
        }
    }

    /// A node of kind `kind` added at canvas point `at` (on whole units), and selected;
    /// linked to `from` (a port, and whether it is an output) where the search came from a
    /// link drawn out of it: an output into the node's input that fits best, the node's
    /// output that fits best into an input.
    pub(crate) fn add_node(
        &mut self,
        kind: &str,
        at: [f32; 2],
        from: Option<&(PortRef, bool)>,
    ) -> Option<Outcome> {
        let k = library::kind(kind)?;
        if k.name == OUTPUT_KIND {
            return Some(Outcome::Refused("A graph has one surface"));
        }
        let node = Node::new(kind, at.map(f32::round));
        self.add_linked(k, node, kind, from, "Add Node")
    }

    /// A call (of kind `kind`: [`CALL_KIND`] a `[Node]` function's, E10-3g-3b;
    /// [`FUNCTION_KIND`] a function file's, E10-3g-2b) of function `function` of module
    /// `module` (at `path` in the project) of signature `signature` added at canvas point
    /// `at`, linked to `from` as [`Self::add_node`] does, and selected: the node search's
    /// functions.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_call(
        &mut self,
        kind: &'static str,
        module: AssetRef,
        path: &str,
        function: &str,
        signature: &str,
        at: [f32; 2],
        from: Option<&(PortRef, bool)>,
    ) -> Option<Outcome> {
        let mut node = Node::new(kind, at.map(f32::round)).with_call(function, signature);
        node.module = module;
        let k = engine_material_graph::node_kind(&node).ok()?;
        self.paths.insert(module, path);
        let base = if kind == CALL_KIND {
            function.to_owned()
        } else {
            id_base(function)
        };
        self.add_linked(k, node, &base, from, "Add Call")
    }

    /// Node `node` of kind `k` added under an id from `base`, linked to `from` (the port
    /// that fits best), and selected, as the step `name`.
    fn add_linked(
        &mut self,
        k: &'static Kind,
        node: Node,
        base: &str,
        from: Option<&(PortRef, bool)>,
        name: &'static str,
    ) -> Option<Outcome> {
        let id = fresh_id(&self.graph, base);
        let link = match from {
            Some((port, true)) => {
                let ty = self.port_type(port, true)?;
                let ports: Vec<_> = k.inputs.iter().map(|i| (i.name, i.ty)).collect();
                Some((port.clone(), PortRef::new(&id, best_port(&ports, ty)?)))
            }
            Some((port, false)) => {
                let ty = self.port_type(port, false)?;
                let ports: Vec<_> = k.outputs.iter().map(|o| (o.name, o.ty)).collect();
                Some((PortRef::new(&id, best_port(&ports, ty)?), port.clone()))
            }
            None => None,
        };
        let (before, after) = self.edit(|g| {
            g.add(&id, node);
            if let Some((source, into)) = link {
                g.links.insert(into, source);
            }
        })?;
        self.selected = [id].into_iter().collect();
        Some(Outcome::Edited {
            name,
            before,
            after,
        })
    }

    /// Its calls' titles from their functions' marks (module, function, title), laid out
    /// again where they changed (no code changes with them).
    pub(crate) fn set_call_titles(&mut self, titles: &[(AssetRef, String, String)]) {
        if self.call_titles != titles {
            titles.clone_into(&mut self.call_titles);
            self.laid_out = None;
        }
    }

    /// Its calls' names and signatures as their functions now have them, where `current`
    /// knows them (a `[Node]` function's, E10-3g-3b; a function file's, its name its
    /// file's, E10-3g-2b): a function's change follows into the graph, as Unreal's
    /// function calls follow their functions; no step of its own, as the signature is the
    /// function's, not the graph's. Whether any changed.
    pub(crate) fn refresh_calls<'s>(
        &mut self,
        current: &dyn Fn(&Node) -> Option<(&'s str, &'s str)>,
    ) -> bool {
        let stale = |node: &Node| {
            if node.kind != CALL_KIND && node.kind != FUNCTION_KIND {
                return None;
            }
            current(node)
                .filter(|&(name, signature)| name != node.function || signature != node.signature)
        };
        if !self.graph.nodes.values().any(|n| stale(n).is_some()) {
            return false;
        }
        let fresh: Vec<(String, &str, &str)> = self
            .graph
            .nodes
            .iter()
            .filter_map(|(id, n)| {
                let (name, signature) = stale(n)?;
                Some((id.clone(), name, signature))
            })
            .collect();
        self.edit(|g| {
            for (id, name, signature) in fresh {
                if let Some(node) = g.nodes.get_mut(&id) {
                    name.clone_into(&mut node.function);
                    signature.clone_into(&mut node.signature);
                }
            }
        })
        .is_some()
    }

    /// The selected nodes made a function (Ctrl+G, Blender's Make Group; E10-3g-2b), the
    /// surface and a function's own inputs and outputs not: the function's graph, its
    /// ports the links across the selection's edge (one input per output outside read
    /// inside, named after the port reading it, as wide as it carries; one output per
    /// output inside read outside, named after the port outside reading it), placed left
    /// and right of the nodes in their readers' order; and what the call taking their place
    /// needs ([`Self::call_made`]).
    ///
    /// # Errors
    ///
    /// Nothing selected that can go, or nothing outside reads it (a function gives
    /// something back).
    pub(crate) fn function_of_selection(&self) -> Result<MadeFunction, String> {
        let picked: BTreeSet<&str> = self
            .selected
            .iter()
            .map(String::as_str)
            .filter(|id| {
                self.graph.nodes.get(*id).is_some_and(|n| {
                    n.kind != OUTPUT_KIND
                        && library::kind(&n.kind).is_none_or(|k| {
                            !matches!(k.body, Body::FunctionInput(_) | Body::FunctionOutput(_))
                        })
                })
            })
            .collect();
        if picked.is_empty() {
            return Err(String::from(
                "Select the nodes to make a function of: the surface and a function's ports stay",
            ));
        }
        let at = |id: &str| self.graph.nodes.get(id).map_or([0.0; 2], |n| n.at);
        let width = |from: &PortRef| match self.carried_by(&from.node, &from.port) {
            PortType::Float(w) => w,
            PortType::Dynamic => 4,
        };
        // The links across the edge, in the order of the nodes reading them (top to bottom).
        let mut crossing: Vec<(&PortRef, &PortRef, bool)> = self
            .graph
            .links
            .iter()
            .filter_map(|(to, from)| {
                match (
                    picked.contains(to.node.as_str()),
                    picked.contains(from.node.as_str()),
                ) {
                    (true, false) => Some((to, from, true)),
                    (false, true) => Some((to, from, false)),
                    _ => None,
                }
            })
            .collect();
        crossing.sort_by(|a, b| {
            let (pa, pb) = (at(&a.0.node), at(&b.0.node));
            (pa[1], pa[0])
                .partial_cmp(&(pb[1], pb[0]))
                .unwrap_or(core::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(b.0))
        });
        let mut names: BTreeSet<String> = BTreeSet::new();
        let mut name = |base: &str, input: bool| {
            let base = if is_param_name(base) && !["in", "out", "inout"].contains(&base) {
                base
            } else if input {
                "input"
            } else {
                "result"
            };
            let mut n = base.to_owned();
            let mut k = 2;
            while names.contains(&n) {
                n = format!("{base}_{k}");
                k += 1;
            }
            names.insert(n.clone());
            n
        };
        let mut inputs: Vec<(PortRef, String, u8)> = Vec::new();
        let mut outputs: Vec<(PortRef, String, u8)> = Vec::new();
        for &(to, from, input) in &crossing {
            let list = if input { &mut inputs } else { &mut outputs };
            if !list.iter().any(|(source, ..)| source == from) {
                list.push((from.clone(), name(&to.port, input), width(from)));
            }
        }
        if outputs.is_empty() {
            return Err(String::from(
                "Nothing outside reads the selection: a function gives something back",
            ));
        }
        // The function: the nodes as they are, their links among them, its ports beside
        // them.
        let mut function = Graph::new(Model::Lit);
        function.nodes.clear();
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for &id in &picked {
            let node = &self.graph.nodes[id];
            lo = [lo[0].min(node.at[0]), lo[1].min(node.at[1])];
            hi = [hi[0].max(node.at[0]), hi[1].max(node.at[1])];
            function.nodes.insert(id.to_owned(), node.clone());
        }
        for (to, from) in &self.graph.links {
            if picked.contains(to.node.as_str()) && picked.contains(from.node.as_str()) {
                function.links.insert(to.clone(), from.clone());
            }
        }
        let place = |x: f32, k: usize| [x, (lo[1] + 80.0 * k as f32).round()];
        for (k, (source, param, w)) in inputs.iter().enumerate() {
            let id = fresh_id(&function, "input");
            let node = Node::new(
                engine_material_graph::function::input_kind(*w),
                place(lo[0] - 260.0, k),
            )
            .named(param);
            function.nodes.insert(id.clone(), node);
            for (to, from) in &self.graph.links {
                if from == source && picked.contains(to.node.as_str()) {
                    function
                        .links
                        .insert(to.clone(), PortRef::new(&id, "value"));
                }
            }
        }
        for (k, (source, param, w)) in outputs.iter().enumerate() {
            let id = fresh_id(&function, "output");
            let node = Node::new(
                engine_material_graph::function::output_kind(*w),
                place(hi[0] + 260.0, k),
            )
            .named(param);
            function.nodes.insert(id.clone(), node);
            function
                .links
                .insert(PortRef::new(&id, "value"), source.clone());
        }
        let signature = engine_material_graph::signature_of(&function)?.to_string();
        // The references its nodes make, with the paths this graph wrote them with.
        let mut paths = AssetPaths::new();
        for node in function.nodes.values() {
            for r in [node.texture, node.module] {
                if let Some(path) = self.paths.get(r) {
                    paths.insert(r, path);
                }
            }
        }
        Ok(MadeFunction {
            function,
            paths,
            signature,
            nodes: picked.into_iter().map(str::to_owned).collect(),
            inputs: inputs.into_iter().map(|(s, n, _)| (s, n)).collect(),
            outputs: outputs.into_iter().map(|(s, n, _)| (s, n)).collect(),
            at: [lo[0].round(), ((lo[1] + hi[1]) * 0.5).round()],
        })
    }

    /// The nodes of `made` taken out, a call of its function (file `module` at `path`,
    /// named `name`) in their place, linked as they were, and selected: Make Function's
    /// step.
    pub(crate) fn call_made(
        &mut self,
        made: &MadeFunction,
        module: AssetRef,
        path: &str,
        name: &str,
    ) -> Option<Outcome> {
        let id = fresh_id(&self.graph, &id_base(name));
        let mut call = Node::new(FUNCTION_KIND, made.at).with_call(name, &made.signature);
        call.module = module;
        self.paths.insert(module, path);
        let (before, after) = self.edit(|g| {
            let readers: Vec<(PortRef, PortRef)> = g
                .links
                .iter()
                .filter(|(to, from)| {
                    made.nodes.contains(&from.node) && !made.nodes.contains(&to.node)
                })
                .map(|(to, from)| (to.clone(), from.clone()))
                .collect();
            g.links.retain(|to, from| {
                !made.nodes.contains(&to.node) && !made.nodes.contains(&from.node)
            });
            g.nodes.retain(|n, _| !made.nodes.contains(n));
            g.add(&id, call);
            for (source, param) in &made.inputs {
                g.links.insert(PortRef::new(&id, param), source.clone());
            }
            for (to, from) in readers {
                if let Some((_, param)) = made.outputs.iter().find(|(s, _)| *s == from) {
                    g.links.insert(to, PortRef::new(&id, param));
                }
            }
        })?;
        self.selected = [id].into_iter().collect();
        self.selected_frames.clear();
        Some(Outcome::Edited {
            name: "Make Function",
            before,
            after,
        })
    }

    /// The selected calls of function files taken apart (Ctrl+Alt+G, Blender's Ungroup;
    /// E10-3g-2b): each one level, its function (from `functions`, its file's references
    /// at `paths`) made nodes where the call was, under their ids in the function where
    /// those are free here, the call's links read through them, a call inside the function
    /// kept a call; the nodes made selected, as one step.
    ///
    /// # Errors
    ///
    /// A call's function not found, or a link of the call to a port it has not.
    pub(crate) fn ungroup(
        &mut self,
        functions: &BTreeMap<AssetRef, Graph>,
        paths: &AssetPaths,
    ) -> Result<Option<Outcome>, String> {
        let calls: Vec<String> = self
            .selected
            .iter()
            .filter(|id| {
                self.graph
                    .nodes
                    .get(*id)
                    .is_some_and(|n| n.kind == FUNCTION_KIND)
            })
            .cloned()
            .collect();
        if calls.is_empty() {
            return Ok(Some(Outcome::Refused(
                "Select a function's call to take apart (Ctrl+G makes one)",
            )));
        }
        let mut graph = self.graph.clone();
        let mut made = Vec::new();
        for call_id in &calls {
            let call = graph.nodes[call_id].clone();
            let function = functions
                .get(&call.module)
                .ok_or_else(|| format!("{} is not found", call.function))?;
            // Its ids where they are free, else its kind's next.
            let mut taken: BTreeSet<String> = graph.nodes.keys().cloned().collect();
            let mut ids: BTreeMap<String, String> = BTreeMap::new();
            for id in function.nodes.keys() {
                let new = if taken.contains(id) {
                    let base = id.trim_end_matches(|c: char| c.is_ascii_digit());
                    let base = if base.is_empty() { "node" } else { base };
                    (1..)
                        .map(|n| format!("{base}{n}"))
                        .find(|n| !taken.contains(n))
                        .unwrap_or_default()
                } else {
                    id.clone()
                };
                taken.insert(new.clone());
                ids.insert(id.clone(), new);
            }
            let id_for = |id: &str| ids.get(id).cloned().unwrap_or_else(|| id.to_owned());
            let (out, added) = engine_material_graph::inline(&graph, call_id, function, &id_for)?;
            graph = out;
            // Where the call was: the function's nodes from its top left corner.
            let lo = function.nodes.values().fold([f32::MAX; 2], |lo, n| {
                [lo[0].min(n.at[0]), lo[1].min(n.at[1])]
            });
            for id in &added {
                if let Some(node) = graph.nodes.get_mut(id) {
                    node.at = [
                        (node.at[0] - lo[0] + call.at[0]).round(),
                        (node.at[1] - lo[1] + call.at[1]).round(),
                    ];
                    for r in [node.texture, node.module] {
                        if let Some(path) = paths.get(r) {
                            self.paths.insert(r, path);
                        }
                    }
                }
            }
            made.extend(added);
        }
        let Some((before, after)) = self.edit(|g| *g = graph) else {
            return Ok(None);
        };
        self.selected = made.into_iter().collect();
        self.selected_frames.clear();
        Ok(Some(Outcome::Edited {
            name: if calls.len() == 1 {
                "Ungroup Function"
            } else {
                "Ungroup Functions"
            },
            before,
            after,
        }))
    }

    /// A texture node of image `image` (at `path` in the project) added at canvas point
    /// `at`, its parameter named after the file (`wood_grain`), and selected: an image
    /// dropped from the dock.
    pub(crate) fn add_texture(
        &mut self,
        image: AssetRef,
        path: &str,
        at: [f32; 2],
    ) -> Option<Outcome> {
        let id = fresh_id(&self.graph, "texture");
        let param = fresh_param(&self.graph, &param_of(path));
        self.paths.insert(image, path);
        let node = Node::new("texture", at.map(f32::round))
            .named(&param)
            .with_texture(image);
        let (before, after) = self.edit(|g| {
            g.add(&id, node);
        })?;
        self.selected = [id].into_iter().collect();
        Some(Outcome::Edited {
            name: "Add Texture",
            before,
            after,
        })
    }

    /// The selected nodes copied (Ctrl+D), the surface not: each with an id of its own 30
    /// units down and right, a parameter with a name of its own; the links among them
    /// copied between the copies, and those into them from the others kept (Blender's);
    /// the copies selected.
    pub(crate) fn duplicate_selected(&mut self) -> Option<Outcome> {
        let picked: Vec<String> = self
            .selected
            .iter()
            .filter(|id| id.as_str() != OUTPUT && self.graph.nodes.contains_key(id.as_str()))
            .cloned()
            .collect();
        if picked.is_empty() {
            return (!self.selected.is_empty()).then_some(Outcome::Refused(
                "The surface is not copied: a graph has one",
            ));
        }
        let mut copies = BTreeSet::new();
        let (before, after) = self.edit(|g| {
            let mut renamed = BTreeMap::new();
            for id in &picked {
                let mut node = g.nodes[id].clone();
                node.at = [node.at[0] + 30.0, node.at[1] + 30.0];
                if !node.param.is_empty() {
                    node.param = fresh_param(g, &node.param);
                }
                let new = fresh_id(g, &node.kind);
                g.nodes.insert(new.clone(), node);
                renamed.insert(id.clone(), new.clone());
                copies.insert(new);
            }
            let into: Vec<(PortRef, PortRef)> = g
                .links
                .iter()
                .filter(|(to, _)| renamed.contains_key(&to.node))
                .map(|(to, from)| (to.clone(), from.clone()))
                .collect();
            for (to, from) in into {
                let from = match renamed.get(&from.node) {
                    Some(n) => PortRef::new(n, &from.port),
                    None => from,
                };
                g.links
                    .insert(PortRef::new(&renamed[&to.node], &to.port), from);
            }
        })?;
        self.selected = copies;
        let name = if picked.len() == 1 {
            "Duplicate Node"
        } else {
            "Duplicate Nodes"
        };
        Some(Outcome::Edited {
            name,
            before,
            after,
        })
    }

    /// A frame of the pointer on the canvas whose panel has its top left at `origin`: the
    /// view panned and zoomed, the selection made, nodes moved and linked; `drag` is the
    /// drag under way, kept between frames.
    pub(crate) fn pointer(
        &mut self,
        p: &Pointer,
        origin: [f32; 2],
        drag: &mut Option<Drag>,
    ) -> Option<Outcome> {
        let at = p.at.map(|q| self.view.canvas_point(q, origin));
        self.cursor = at;
        let reach = PORT_REACH / self.view.zoom();
        self.hover = match at {
            Some(c) if p.over || drag.is_some() => self.hit(c, reach),
            _ => Hit::Canvas,
        };
        // Escape: the drag under way taken back, or else the selection let go.
        if p.escape {
            match drag.take() {
                Some(Drag::Move { starts, frames, .. }) => {
                    for (id, at) in starts {
                        if let Some(n) = self.graph.nodes.get_mut(&id) {
                            n.at = at;
                        }
                    }
                    for (id, at) in frames {
                        if let Some(f) = self.graph.frames.get_mut(&id) {
                            f.at = at;
                        }
                    }
                    self.revision += 1;
                }
                Some(Drag::Size { frame, start, .. }) => {
                    if let Some(f) = self.graph.frames.get_mut(&frame) {
                        f.size = start;
                    }
                    self.revision += 1;
                }
                Some(_) => {}
                None => self.selected.clear(),
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
                // A second press soon after the first, in its place, on a link: a reroute
                // on it (E10-3e-2a, Unreal's double-click).
                let now = std::time::Instant::now();
                let again = self.last_press.replace((now, c)).is_some_and(|(t, q)| {
                    now.duration_since(t) <= DOUBLE_CLICK
                        && (q[0] - c[0]).hypot(q[1] - c[1]) * self.view.zoom() <= CLICK_SLOP
                });
                if again
                    && self.hover == Hit::Canvas
                    && let Some(to) = self.link_at(c, reach * 0.6)
                {
                    self.last_press = None;
                    return self.add_reroute(&to, c);
                }
                *drag = Some(self.press(c, p.shift, p.ctrl));
            }
            return None;
        }
        let ended = !p.held;
        match drag.as_mut()? {
            Drag::Move {
                from,
                starts,
                frames,
                moved,
            } => {
                if let Some(c) = at {
                    let d = [c[0] - from[0], c[1] - from[1]];
                    if !*moved && (d[0].hypot(d[1]) * self.view.zoom()) > CLICK_SLOP {
                        *moved = true;
                    }
                    if *moved {
                        for (id, s) in starts.iter() {
                            if let Some(n) = self.graph.nodes.get_mut(id) {
                                n.at = [s[0] + d[0], s[1] + d[1]];
                            }
                        }
                        for (id, s) in frames.iter() {
                            if let Some(f) = self.graph.frames.get_mut(id) {
                                f.at = [s[0] + d[0], s[1] + d[1]];
                            }
                        }
                        self.revision += 1;
                    }
                }
                if !ended {
                    return None;
                }
                let Some(Drag::Move {
                    starts,
                    frames,
                    moved,
                    ..
                }) = drag.take()
                else {
                    return None;
                };
                if !moved {
                    return None;
                }
                // It ends on whole units: the file keeps short numbers.
                for (id, _) in &starts {
                    if let Some(n) = self.graph.nodes.get_mut(id) {
                        n.at = n.at.map(f32::round);
                    }
                }
                for (id, _) in &frames {
                    if let Some(f) = self.graph.frames.get_mut(id) {
                        f.at = f.at.map(f32::round);
                    }
                }
                let name = match (frames.len(), starts.len()) {
                    (1, _) => "Move Frame",
                    (0, 1) => "Move Node",
                    _ => "Move Nodes",
                };
                self.settle().map(|(before, after)| Outcome::Edited {
                    name,
                    before,
                    after,
                })
            }
            Drag::Size {
                frame,
                from,
                start,
                moved,
            } => {
                if let Some(c) = at {
                    let d = [c[0] - from[0], c[1] - from[1]];
                    if !*moved && (d[0].hypot(d[1]) * self.view.zoom()) > CLICK_SLOP {
                        *moved = true;
                    }
                    if *moved && let Some(f) = self.graph.frames.get_mut(frame.as_str()) {
                        f.size = [
                            (start[0] + d[0]).max(FRAME_MIN[0]),
                            (start[1] + d[1]).max(FRAME_MIN[1]),
                        ];
                        self.revision += 1;
                    }
                }
                if !ended {
                    return None;
                }
                let Some(Drag::Size { frame, moved, .. }) = drag.take() else {
                    return None;
                };
                if !moved {
                    return None;
                }
                if let Some(f) = self.graph.frames.get_mut(&frame) {
                    f.size = f.size.map(f32::round);
                }
                self.settle().map(|(before, after)| Outcome::Edited {
                    name: "Resize Frame",
                    before,
                    after,
                })
            }
            Drag::Link { .. } => {
                if !ended {
                    return None;
                }
                let Some(Drag::Link {
                    from,
                    output,
                    lifted,
                }) = drag.take()
                else {
                    return None;
                };
                self.drop_link(from, output, lifted, at.map(|c| self.hit(c, reach)))
            }
            Drag::Yank { .. } => {
                if !ended {
                    return None;
                }
                let Some(Drag::Yank { from, inputs }) = drag.take() else {
                    return None;
                };
                self.drop_yank(&from, &inputs, at.map(|c| self.hit(c, reach)))
            }
            Drag::Select { from, add } => {
                if !ended {
                    return None;
                }
                let (from, add) = (*from, *add);
                *drag = None;
                let to = at.unwrap_or(from);
                if !add {
                    self.selected.clear();
                    self.selected_frames.clear();
                }
                let size = [(to[0] - from[0]).abs(), (to[1] - from[1]).abs()];
                if size[0].max(size[1]) * self.view.zoom() > CLICK_SLOP {
                    let lo = [from[0].min(to[0]), from[1].min(to[1])];
                    let hi = [from[0].max(to[0]), from[1].max(to[1])];
                    for b in &self.boxes {
                        let meets = b.at[0] <= hi[0]
                            && b.at[1] <= hi[1]
                            && b.at[0] + b.size[0] >= lo[0]
                            && b.at[1] + b.size[1] >= lo[1];
                        if meets {
                            self.selected.insert(b.id.clone());
                        }
                    }
                }
                None
            }
            Drag::Pan => None,
        }
    }

    /// The drag a press at canvas point `c` starts: a link from a port (an input's link
    /// lifted off it), a move of the selection made by the press, or a box.
    fn press(&mut self, c: [f32; 2], shift: bool, ctrl: bool) -> Drag {
        let reach = PORT_REACH / self.view.zoom();
        match self.hit(c, reach) {
            Hit::Output(k, o) if ctrl => {
                let from = PortRef::new(&self.boxes[k].id, self.boxes[k].outputs[o]);
                let inputs: Vec<PortRef> = self
                    .graph
                    .links
                    .iter()
                    .filter(|(_, f)| **f == from)
                    .map(|(to, _)| to.clone())
                    .collect();
                if inputs.is_empty() {
                    Drag::Link {
                        from,
                        output: true,
                        lifted: None,
                    }
                } else {
                    Drag::Yank { from, inputs }
                }
            }
            Hit::Output(k, o) => Drag::Link {
                from: PortRef::new(&self.boxes[k].id, self.boxes[k].outputs[o]),
                output: true,
                lifted: None,
            },
            Hit::Input(k, i) => {
                let to = PortRef::new(&self.boxes[k].id, self.boxes[k].inputs[i]);
                match self.graph.links.get(&to) {
                    Some(from) => Drag::Link {
                        from: from.clone(),
                        output: true,
                        lifted: Some(to),
                    },
                    None => Drag::Link {
                        from: to,
                        output: false,
                        lifted: None,
                    },
                }
            }
            Hit::Node(k) => {
                let id = self.boxes[k].id.clone();
                if shift {
                    if !self.selected.remove(&id) {
                        self.selected.insert(id);
                    }
                } else if !self.selected.contains(&id) {
                    self.selected.clear();
                    self.selected_frames.clear();
                    self.selected.insert(id);
                }
                let starts = self
                    .selected
                    .iter()
                    .filter_map(|id| Some((id.clone(), self.graph.nodes.get(id)?.at)))
                    .collect();
                Drag::Move {
                    from: c,
                    starts,
                    frames: Vec::new(),
                    moved: false,
                }
            }
            // A frame's title bar: it, and the nodes whose boxes it holds whole, moved.
            Hit::Frame(k) => {
                let Some((id, f)) = self.graph.frames.iter().nth(k) else {
                    return Drag::Select {
                        from: c,
                        add: shift,
                    };
                };
                let (id, at) = (id.clone(), f.at);
                let hi = [f.at[0] + f.size[0], f.at[1] + f.size[1]];
                let starts = self
                    .boxes
                    .iter()
                    .filter(|b| {
                        b.at[0] >= at[0]
                            && b.at[1] >= at[1]
                            && b.at[0] + b.size[0] <= hi[0]
                            && b.at[1] + b.size[1] <= hi[1]
                    })
                    .filter_map(|b| Some((b.id.clone(), self.graph.nodes.get(&b.id)?.at)))
                    .collect();
                if !shift {
                    self.selected.clear();
                    self.selected_frames.clear();
                }
                self.selected_frames.insert(id.clone());
                Drag::Move {
                    from: c,
                    starts,
                    frames: vec![(id, at)],
                    moved: false,
                }
            }
            Hit::FrameCorner(k) => match self.graph.frames.iter().nth(k) {
                Some((id, f)) => Drag::Size {
                    frame: id.clone(),
                    from: c,
                    start: f.size,
                    moved: false,
                },
                None => Drag::Select {
                    from: c,
                    add: shift,
                },
            },
            Hit::Canvas => Drag::Select {
                from: c,
                add: shift,
            },
        }
    }

    /// The links lifted off output `from` (into `inputs`) dropped on `on`: carried to the
    /// output there (refused, all of them, where one would loop back), or taken out.
    fn drop_yank(
        &mut self,
        from: &PortRef,
        inputs: &[PortRef],
        on: Option<Hit>,
    ) -> Option<Outcome> {
        match on {
            Some(Hit::Output(k, o)) => {
                let to = PortRef::new(&self.boxes[k].id, self.boxes[k].outputs[o]);
                if to == *from {
                    return None;
                }
                let mut trial = self.graph.clone();
                for input in inputs {
                    if let Err(why) = connect(&mut trial, to.clone(), input.clone()) {
                        return Some(Outcome::Refused(why));
                    }
                }
                let (before, after) = self.edit(|g| *g = trial)?;
                let name = if inputs.len() == 1 {
                    "Move Link"
                } else {
                    "Move Links"
                };
                Some(Outcome::Edited {
                    name,
                    before,
                    after,
                })
            }
            _ => {
                let (before, after) = self.edit(|g| {
                    for input in inputs {
                        g.links.remove(input);
                    }
                })?;
                let name = if inputs.len() == 1 {
                    "Remove Link"
                } else {
                    "Remove Links"
                };
                Some(Outcome::Edited {
                    name,
                    before,
                    after,
                })
            }
        }
    }

    /// A link drawn from `from` dropped on `on`: linked there (an input's old link
    /// replaced), moved (lifted off one input onto another), or taken out (lifted and
    /// dropped on nothing).
    fn drop_link(
        &mut self,
        from: PortRef,
        output: bool,
        lifted: Option<PortRef>,
        on: Option<Hit>,
    ) -> Option<Outcome> {
        let port = |k: usize, name: &str| PortRef::new(&self.boxes[k].id, name);
        let (source, into) = match (output, on) {
            (true, Some(Hit::Input(k, i))) => (from, port(k, self.boxes[k].inputs[i])),
            (false, Some(Hit::Output(k, o))) => (port(k, self.boxes[k].outputs[o]), from),
            _ => {
                // Dropped on the empty canvas: the node search, for a node to end it on.
                if lifted.is_none()
                    && matches!(on, Some(Hit::Canvas))
                    && let Some(at) = self.cursor
                {
                    return Some(Outcome::Search {
                        at,
                        from: (from, output),
                    });
                }
                // Dropped on nothing a link can end on: a lifted link goes.
                let lifted = lifted?;
                let (before, after) = self.edit(|g| {
                    g.links.remove(&lifted);
                })?;
                return Some(Outcome::Edited {
                    name: "Remove Link",
                    before,
                    after,
                });
            }
        };
        if lifted.as_ref() == Some(&into) {
            return None;
        }
        // Checked on the graph as it would be, the lifted link out.
        let mut trial = self.graph.clone();
        if let Some(l) = &lifted {
            trial.links.remove(l);
        }
        if let Err(why) = connect(&mut trial, source, into) {
            return Some(Outcome::Refused(why));
        }
        let (before, after) = self.edit(|g| *g = trial)?;
        let name = if lifted.is_some() {
            "Move Link"
        } else {
            "Link"
        };
        Some(Outcome::Edited {
            name,
            before,
            after,
        })
    }
}

/// A port's colour by what it carries (Unreal's and Shader Graph's habit: a scalar grey,
/// then green, yellow and pink as it widens; a dynamic one blue). The ports' names say it
/// too, not colour alone.
fn port_color(ty: PortType) -> Vec4 {
    match ty {
        PortType::Float(1) => Vec4::new(0.66, 0.66, 0.66, 1.0),
        PortType::Float(2) => Vec4::new(0.47, 0.80, 0.42, 1.0),
        PortType::Float(3) => Vec4::new(0.93, 0.80, 0.32, 1.0),
        PortType::Float(_) => Vec4::new(0.90, 0.47, 0.72, 1.0),
        PortType::Dynamic => Vec4::new(0.47, 0.72, 0.96, 1.0),
    }
}

/// A title bar's colour by its kind's group in the node search, dark enough for white
/// text (linear: about sRGB 40 95 90 for the inputs, 55 75 120 for the maths).
pub(crate) fn title_color(category: &str) -> Vec4 {
    match category {
        "Input" => Vec4::new(0.021, 0.115, 0.1, 1.0),
        "Math" => Vec4::new(0.038, 0.07, 0.188, 1.0),
        "Vector" => Vec4::new(0.09, 0.045, 0.205, 1.0),
        "Surface" => Vec4::new(0.188, 0.09, 0.021, 1.0),
        "Output" => Vec4::new(0.223, 0.026, 0.032, 1.0),
        _ => Vec4::new(0.061, 0.061, 0.069, 1.0),
    }
}

/// A box's body (linear: about sRGB 63 63 67).
pub(crate) const BODY: Vec4 = Vec4::new(0.05, 0.05, 0.058, 0.97);
/// A selected box's frame (the viewport's orange), and one under the pointer's.
pub(crate) const SELECTED: Vec4 = Vec4::new(1.0, 0.62, 0.18, 1.0);
pub(crate) const HOVERED: Vec4 = Vec4::new(1.0, 1.0, 1.0, 0.3);

/// A constant's components as the canvas shows them: up to three decimals, no trailing
/// zeros.
pub(crate) fn value_text(out: &mut String, v: &[f32]) {
    use std::fmt::Write as _;
    out.clear();
    for (k, x) in v.iter().enumerate() {
        if k > 0 {
            out.push_str(", ");
        }
        let start = out.len();
        let _ = write!(out, "{x:.3}");
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
        if &out[start..] == "-0" {
            out.truncate(start);
            out.push('0');
        }
    }
}

/// The box the selection is being dragged as, in canvas units.
fn select_box(drag: Option<&Drag>, cursor: Option<[f32; 2]>) -> Option<([f32; 2], [f32; 2])> {
    match (drag, cursor) {
        (Some(Drag::Select { from, .. }), Some(to)) => Some((*from, to)),
        _ => None,
    }
}

/// What the canvas draws with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Look<'a> {
    /// The panel's rectangle in pixels.
    pub(crate) rect: [u32; 4],
    /// Pixels a panel pixel.
    pub(crate) scale: f32,
    /// The editor's colours.
    pub(crate) palette: &'a Palette,
    /// The glyphs' texture.
    pub(crate) glyphs: &'a GlyphTexture,
    /// The node previews' atlas, once one is drawn (E10-4g).
    pub(crate) tiles: Option<Tiles<'a>>,
}

/// The node previews' atlas the canvas draws its tiles from (E10-4g).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tiles<'a> {
    /// Its texture's bindless index, and the texture's size in pixels.
    pub(crate) texture: u32,
    pub(crate) size: [f32; 2],
    /// Tiles across, and their nodes in their order.
    pub(crate) columns: u32,
    pub(crate) nodes: &'a [String],
}

/// Under a tile until its picture comes (E10-4g), and around a round one.
const TILE_BACK: Vec4 = Vec4::new(0.02, 0.02, 0.025, 1.0);

/// Draws `doc`'s canvas under the panels (layer −1, after the viewport's overlays, which
/// its background covers): the grid, the links, the boxes with their ports, then the link
/// or the box being dragged; `scratch` holds a constant's value while it is drawn.
pub(crate) fn draw(
    doc: &GraphDoc,
    drag: Option<&Drag>,
    look: &Look<'_>,
    text: &mut UiText,
    canvas: &mut Canvas,
    scratch: &mut String,
) {
    let [rx, ry, rw, rh] = look.rect.map(|v| v as f32);
    draw_back(&doc.view, look, canvas);
    let order = CANVAS_ORDER;
    let z = doc.view.zoom() * look.scale;
    let origin = [rx / look.scale, ry / look.scale];
    let to = |c: [f32; 2]| {
        let q = doc.view.panel_point(c, origin);
        Vec2::new(q[0] * look.scale, q[1] * look.scale)
    };
    let p = look.palette;
    let solid = |canvas: &mut Canvas, lo: Vec2, hi: Vec2, color: Vec4| {
        canvas.draw(
            &Quad::solid((lo + hi) * 0.5, hi - lo)
                .color(color)
                .order(order),
        );
    };
    let line = |canvas: &mut Canvas, a: [f32; 2], b: [f32; 2], width: f32, color: Vec4| {
        let mut last = to(a);
        for s in 1..=LINK_SEGMENTS {
            let next = to(link_point(a, b, s as f32 / LINK_SEGMENTS as f32));
            canvas.draw(&Quad::line(last, next, width).color(color).order(order));
            last = next;
        }
    };
    let lifted = match drag {
        Some(Drag::Link {
            lifted: Some(l), ..
        }) => Some(l),
        _ => None,
    };
    let yanked = match drag {
        Some(Drag::Yank { inputs, .. }) => inputs.as_slice(),
        _ => &[],
    };
    // The frames (E10-3e-2b), under the links and the boxes: an area the grid shows
    // through, its title bar and title, a grip at its corner; selected, framed in orange,
    // under the pointer, faintly.
    for (k, (id, f)) in doc.graph.frames.iter().enumerate() {
        let (lo, hi) = (to(f.at), to([f.at[0] + f.size[0], f.at[1] + f.size[1]]));
        if hi.x < rx || hi.y < ry || lo.x > rx + rw || lo.y > ry + rh {
            continue;
        }
        let radius = RADIUS * z;
        canvas.draw(
            &Quad::rounded_rect((lo + hi) * 0.5, hi - lo, radius)
                .color(FRAME_FILL)
                .order(order),
        );
        let bar_hi = Vec2::new(hi.x, lo.y + FRAME_TITLE * z);
        canvas.draw(
            &Quad::rounded_rect((lo + bar_hi) * 0.5, bar_hi - lo, radius)
                .color(FRAME_BAR)
                .order(order),
        );
        solid(
            canvas,
            Vec2::new(lo.x, bar_hi.y - radius),
            bar_hi,
            FRAME_BAR,
        );
        let size = TITLE_TEXT * z;
        if size >= 5.0 {
            text.draw_left(
                canvas,
                look.glyphs,
                &f.title,
                size,
                Vec2::new(lo.x + PAD * z, lo.y + FRAME_TITLE * 0.5 * z),
                Vec4::from_array(p.text_dim),
                order,
            );
        }
        // The grip: two strokes across the corner.
        let grip = FRAME_GRIP * z;
        for s in [0.4, 0.75] {
            canvas.draw(
                &Quad::line(
                    Vec2::new(hi.x - grip * s, hi.y - 3.0 * z),
                    Vec2::new(hi.x - 3.0 * z, hi.y - grip * s),
                    look.scale,
                )
                .color(Vec4::new(1.0, 1.0, 1.0, 0.35))
                .order(order),
            );
        }
        let frame = if doc.selected_frames.contains(id) {
            Some(SELECTED)
        } else if matches!(doc.hover, Hit::Frame(h) | Hit::FrameCorner(h) if h == k) {
            Some(HOVERED)
        } else {
            None
        };
        if let Some(color) = frame {
            canvas.draw(
                &Quad::rounded_rect((lo + hi) * 0.5, hi - lo, radius)
                    .outline(1.5 * look.scale)
                    .color(color)
                    .order(order),
            );
        }
    }
    // The links, those of the selected nodes brighter.
    for (into, from) in &doc.graph.links {
        if Some(into) == lifted || yanked.contains(into) {
            continue;
        }
        let (Some(a), Some(b)) = (doc.port_point(from, true), doc.port_point(into, false)) else {
            continue;
        };
        let ty = doc.carried_by(&from.node, &from.port);
        let lit = doc.selected.contains(&from.node) || doc.selected.contains(&into.node);
        let mut color = port_color(ty);
        color.w = if lit { 1.0 } else { 0.7 };
        let width = if lit { 2.5 } else { 1.75 } * look.scale;
        line(canvas, a, b, width, color);
    }
    let title_size = TITLE_TEXT * z;
    let port_size = PORT_TEXT * z;
    let text_color = Vec4::from_array(p.text);
    let dim = Vec4::from_array(p.text_dim);
    for k in doc.order() {
        let b = &doc.boxes[k];
        let (lo, hi) = (to(b.at), to([b.at[0] + b.size[0], b.at[1] + b.size[1]]));
        if hi.x < rx || hi.y < ry || lo.x > rx + rw || lo.y > ry + rh {
            continue;
        }
        let size = hi - lo;
        // A reroute (E10-3e-2a): a capsule in what it carries, a pin at each end, no title.
        let reroute = b.is_reroute();
        let radius = if reroute { size.y * 0.5 } else { RADIUS * z };
        let carried = reroute.then(|| port_color(doc.carried_by(&b.id, "out")));
        if let Some(color) = carried {
            canvas.draw(
                &Quad::rounded_rect((lo + hi) * 0.5, size, radius)
                    .color(Vec4::new(color.x, color.y, color.z, 0.55))
                    .order(order),
            );
        } else {
            canvas.draw(
                &Quad::rounded_rect((lo + hi) * 0.5, size, radius)
                    .color(BODY)
                    .order(order),
            );
            // The title bar: rounded at the top only.
            let bar = title_color(b.kind.map_or("", |k| k.category));
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
            // A constant's value where its inputs would be: a colour's swatch, the numbers.
            if let Some(kind) = b.kind
                && let Body::Constant(w) = kind.body
                && let Some(node) = doc.graph.nodes.get(&b.id)
            {
                let row = Vec2::new(lo.x + PAD * z, to([0.0, b.at[1] + TITLE + ROW * 0.5]).y);
                if kind.name == "color" {
                    let c = node.value;
                    let swatch = Vec2::new(28.0, 14.0) * z;
                    canvas.draw(
                        &Quad::rounded_rect(row + Vec2::new(swatch.x * 0.5, 0.0), swatch, 2.0 * z)
                            .color(Vec4::new(c[0], c[1], c[2], 1.0))
                            .order(order),
                    );
                } else if port_size >= 6.0 {
                    value_text(scratch, &node.value[..usize::from(w)]);
                    text.draw_left(canvas, look.glyphs, scratch, port_size, row, dim, order);
                }
            }
        }
        let linked_in = |name: &str| {
            doc.graph
                .links
                .keys()
                .any(|to| to.node == b.id && to.port == name)
        };
        let linked_out = |name: &str| {
            doc.graph
                .links
                .values()
                .any(|from| from.node == b.id && from.port == name)
        };
        let dot = PORT_RADIUS * z;
        for (i, name) in b.inputs.iter().enumerate() {
            let at = to(b.input_at(i));
            let lit = doc.hover == Hit::Input(k, i);
            port(
                canvas,
                at,
                dot,
                carried.unwrap_or_else(|| port_color(b.input_type(i))),
                linked_in(name),
                lit,
                order,
            );
            if port_size >= 6.0 && !reroute {
                let left = at + Vec2::new(dot + 5.0 * z, 0.0);
                text.draw_left(canvas, look.glyphs, name, port_size, left, dim, order);
            }
        }
        for (o, name) in b.outputs.iter().enumerate() {
            let at = to(b.output_at(o));
            let lit = doc.hover == Hit::Output(k, o);
            port(
                canvas,
                at,
                dot,
                carried.unwrap_or_else(|| port_color(b.output_type(o))),
                linked_out(name),
                lit,
                order,
            );
            if port_size >= 6.0 && !reroute {
                let width = text.measure(name, port_size, None)[0];
                let left = at - Vec2::new(dot + 5.0 * z + width, 0.0);
                text.draw_left(canvas, look.glyphs, name, port_size, left, dim, order);
            }
        }
        // Its preview (E10-4g): its tile of the atlas, over a dark square that shows until
        // it is drawn and around a round one.
        if let Some([tile_lo, tile_hi]) = b.tile_rect() {
            let (tile_lo, tile_hi) = (to(tile_lo), to(tile_hi));
            solid(canvas, tile_lo, tile_hi, TILE_BACK);
            if let Some(t) = &look.tiles
                && let Some(n) = t.nodes.iter().position(|id| *id == b.id)
            {
                let (n, side) = (n as u32, engine_app::NODE_TILE as f32);
                let columns = t.columns.max(1);
                let x = (n % columns) as f32 * side;
                let y = (n / columns) as f32 * side;
                canvas.draw(
                    &Quad::sprite(t.texture, (tile_lo + tile_hi) * 0.5, tile_hi - tile_lo)
                        .uv([
                            x / t.size[0],
                            y / t.size[1],
                            (x + side) / t.size[0],
                            (y + side) / t.size[1],
                        ])
                        .order(order),
                );
            }
        }
        // Its node stops the graph (E10-3f-3): a red frame, why under the box.
        let error = doc
            .error
            .as_ref()
            .filter(|e| e.node.as_deref() == Some(b.id.as_str()));
        // Selected: an orange frame (the viewport's); under the pointer, a faint one.
        let frame = if error.is_some() {
            Some(Vec4::from_array(p.error))
        } else if doc.selected.contains(&b.id) {
            Some(SELECTED)
        } else if doc.hover == Hit::Node(k) {
            Some(HOVERED)
        } else {
            None
        };
        if let Some(color) = frame {
            canvas.draw(
                &Quad::rounded_rect((lo + hi) * 0.5, size, radius)
                    .outline(1.5 * look.scale)
                    .color(color)
                    .order(order),
            );
        }
        if let Some(e) = error
            && port_size >= 5.0
        {
            text.draw_left(
                canvas,
                look.glyphs,
                &e.message,
                port_size,
                Vec2::new(lo.x, hi.y + ROW * 0.5 * z),
                Vec4::from_array(p.error),
                order,
            );
        }
    }
    // The link being drawn: from its port to the pointer, or to the port it would end on.
    if let Some(Drag::Link { from, output, .. }) = drag
        && let Some(cursor) = doc.cursor
        && let Some(start) = doc.port_point(from, *output)
    {
        let end = match doc.hover {
            Hit::Input(k, i) if *output => doc.boxes[k].input_at(i),
            Hit::Output(k, o) if !*output => doc.boxes[k].output_at(o),
            _ => cursor,
        };
        let (a, b) = if *output { (start, end) } else { (end, start) };
        line(
            canvas,
            a,
            b,
            2.0 * look.scale,
            Vec4::new(1.0, 1.0, 1.0, 0.85),
        );
    }
    // Links being carried: from the pointer, or the output they would go to, into theirs.
    if let Some(cursor) = doc.cursor
        && !yanked.is_empty()
    {
        let start = match doc.hover {
            Hit::Output(k, o) => doc.boxes[k].output_at(o),
            _ => cursor,
        };
        for input in yanked {
            if let Some(end) = doc.port_point(input, false) {
                line(
                    canvas,
                    start,
                    end,
                    2.0 * look.scale,
                    Vec4::new(1.0, 1.0, 1.0, 0.85),
                );
            }
        }
    }
    if let Some((a, b)) = select_box(drag, doc.cursor) {
        draw_select_box(&doc.view, look, a, b, canvas);
    }
    canvas.set_clip(None);
}

/// The order a canvas draws in: under the panels (layer −1), after the viewport's overlays,
/// which its background covers.
pub(crate) const CANVAS_ORDER: Order = Order::layer(-1);

/// A canvas's back, clipped to its panel (the animation graph's canvas's too): its colour,
/// the grid's small squares while they are 8 pixels or more apart, its large ones.
pub(crate) fn draw_back(view: &View, look: &Look<'_>, canvas: &mut Canvas) {
    let [rx, ry, rw, rh] = look.rect.map(|v| v as f32);
    canvas.set_clip(Some(ClipRect {
        x: look.rect[0] as i32,
        y: look.rect[1] as i32,
        width: look.rect[2],
        height: look.rect[3],
    }));
    let z = view.zoom() * look.scale;
    let pan = view.pan;
    let origin = [rx / look.scale, ry / look.scale];
    let to = |c: [f32; 2]| {
        let q = view.panel_point(c, origin);
        Vec2::new(q[0] * look.scale, q[1] * look.scale)
    };
    let solid = |canvas: &mut Canvas, lo: Vec2, hi: Vec2, color: Vec4| {
        canvas.draw(
            &Quad::solid((lo + hi) * 0.5, hi - lo)
                .color(color)
                .order(CANVAS_ORDER),
        );
    };
    solid(
        canvas,
        Vec2::new(rx, ry),
        Vec2::new(rx + rw, ry + rh),
        Vec4::from_array(look.palette.code),
    );
    for (step, alpha) in [(GRID, 0.035), (GRID_MAJOR, 0.07)] {
        if step * z < 8.0 {
            continue;
        }
        let color = Vec4::new(1.0, 1.0, 1.0, alpha);
        let first = [(pan[0] / step).ceil(), (pan[1] / step).ceil()];
        let mut x = first[0] * step;
        while to([x, 0.0]).x < rx + rw {
            let sx = to([x, 0.0]).x;
            solid(
                canvas,
                Vec2::new(sx, ry),
                Vec2::new(sx + 1.0, ry + rh),
                color,
            );
            x += step;
        }
        let mut y = first[1] * step;
        while to([0.0, y]).y < ry + rh {
            let sy = to([0.0, y]).y;
            solid(
                canvas,
                Vec2::new(rx, sy),
                Vec2::new(rx + rw, sy + 1.0),
                color,
            );
            y += step;
        }
    }
}

/// The box a selection is being dragged as, from canvas point `a` to `b`: a faint blue
/// area and its edge.
pub(crate) fn draw_select_box(
    view: &View,
    look: &Look<'_>,
    a: [f32; 2],
    b: [f32; 2],
    canvas: &mut Canvas,
) {
    let origin = [
        look.rect[0] as f32 / look.scale,
        look.rect[1] as f32 / look.scale,
    ];
    let to = |c: [f32; 2]| {
        let q = view.panel_point(c, origin);
        Vec2::new(q[0] * look.scale, q[1] * look.scale)
    };
    let (a, b) = (to(a), to(b));
    let (lo, hi) = (a.min(b), a.max(b));
    canvas.draw(
        &Quad::solid((lo + hi) * 0.5, hi - lo)
            .color(Vec4::new(0.35, 0.55, 0.95, 0.12))
            .order(CANVAS_ORDER),
    );
    canvas.draw(
        &Quad::rect((lo + hi) * 0.5, hi - lo)
            .outline(look.scale)
            .color(Vec4::new(0.45, 0.65, 1.0, 0.7))
            .order(CANVAS_ORDER),
    );
}

/// A port's dot at `at`: filled where linked, a ring where not, white-ringed under the
/// pointer.
fn port(
    canvas: &mut Canvas,
    at: Vec2,
    radius: f32,
    color: Vec4,
    linked: bool,
    lit: bool,
    order: Order,
) {
    let fill = if linked {
        color
    } else {
        Vec4::new(0.02, 0.02, 0.025, 1.0)
    };
    canvas.draw(&Quad::circle(at, radius).color(fill).order(order));
    canvas.draw(
        &Quad::circle(at, radius)
            .outline((radius * 0.35).max(1.0))
            .color(color)
            .order(order),
    );
    if lit {
        canvas.draw(
            &Quad::circle(at, radius * 1.6)
                .outline(1.0)
                .color(Vec4::ONE)
                .order(order),
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_material_graph::Node;

    /// Seven pixels a character, as `FixedMeasure` does at 14 pixels.
    fn measure(s: &str, size: f32) -> f32 {
        s.chars().count() as f32 * size * 0.5
    }

    /// A colour into the surface's base, a number beside it.
    fn doc() -> GraphDoc {
        let mut g = Graph::new(Model::Lit);
        g.nodes.get_mut(OUTPUT).unwrap().at = [300.0, 0.0];
        g.add(
            "c",
            Node::new("color", [0.0, 0.0]).with_value([0.9, 0.1, 0.1, 1.0]),
        );
        g.add(
            "r",
            Node::new("number", [0.0, 120.0])
                .with_value([0.6, 0.0, 0.0, 0.0])
                .named("roughness"),
        );
        g.link(PortRef::new("c", "rgba"), PortRef::new(OUTPUT, "base"));
        let mut d = GraphDoc::new(
            AssetId::new(),
            "materials/glow.mgraph",
            g,
            AssetPaths::new(),
        );
        d.relayout(&mut measure);
        d
    }

    /// E10-3e-2a: a double-click on a link puts a reroute on it (the link passing through,
    /// one step); a reroute is a capsule with a pin at each end, its middle grabbed to move
    /// it, and what goes out of it is what goes in.
    #[test]
    fn a_double_click_on_a_link_puts_a_reroute_on_it() {
        let mut d = doc();
        let (c, out) = (d.box_of("c").unwrap(), d.box_of(OUTPUT).unwrap());
        let middle = link_point(d.boxes[c].output_at(0), d.boxes[out].input_at(0), 0.5);
        let base = PortRef::new(OUTPUT, "base");
        assert_eq!(d.link_at(middle, 3.0), Some(base.clone()));
        assert_eq!(d.link_at([middle[0], middle[1] + 40.0], 3.0), None);
        // A press and its release: a selection; a second press in its place: the reroute.
        let mut drag = None;
        let p = |press, held| Pointer {
            at: Some(middle),
            over: true,
            press,
            held,
            ..Pointer::default()
        };
        assert_eq!(d.pointer(&p(true, true), [0.0; 2], &mut drag), None);
        d.pointer(&p(false, false), [0.0; 2], &mut drag);
        assert!(drag.is_none());
        let Some(Outcome::Edited { name, .. }) = d.pointer(&p(true, true), [0.0; 2], &mut drag)
        else {
            panic!("a reroute")
        };
        assert_eq!(name, "Add Reroute");
        assert_eq!(d.graph.links[&base], PortRef::new("reroute1", "out"));
        assert_eq!(
            d.graph.links[&PortRef::new("reroute1", "in")],
            PortRef::new("c", "rgba")
        );
        assert!(d.selected.contains("reroute1") && d.selected.len() == 1);
        // Laid out as a capsule, its middle where it was put, its pins at its ends.
        d.relayout(&mut measure);
        let k = d.box_of("reroute1").unwrap();
        let b = d.boxes[k].clone();
        assert!(b.is_reroute());
        assert_eq!(b.size, REROUTE);
        let mid = [b.at[0] + REROUTE[0] * 0.5, b.at[1] + REROUTE[1] * 0.5];
        assert!((mid[0] - middle[0]).abs() <= 0.5 && (mid[1] - middle[1]).abs() <= 0.5);
        assert_eq!(b.input_at(0), [b.at[0], mid[1]]);
        assert_eq!(b.output_at(0), [b.at[0] + REROUTE[0], mid[1]]);
        assert_eq!(d.hit(mid, PORT_REACH), Hit::Node(k));
        assert_eq!(d.hit(b.output_at(0), PORT_REACH), Hit::Output(k, 0));
        // What it carries: the colour's four floats, through it (and a chain of them).
        assert_eq!(d.carried_by("reroute1", "out"), PortType::Float(4));
        assert_eq!(d.carried_by("c", "rgba"), PortType::Float(4));
    }

    /// E10-3g-3b: a `[Node]` function's call is added as a step, its module's path in the
    /// file; titled by its function's name, then by its mark once that is known; its
    /// signature made its function's again when that changes, with no step of its own; the
    /// same signature, or a function no node can call, changes nothing.
    #[test]
    fn a_call_is_added_titled_and_follows_its_function() {
        use crate::node_functions::NodeFunction;
        let mut d = doc();
        let module = AssetRef::file(AssetId::from_u128(9));
        let old = "float3(MaterialContext ctx, float3 color)";
        let added = d.add_call(
            CALL_KIND,
            module,
            "shaders/mapping.slang",
            "swirl",
            old,
            [0.0, 400.0],
            None,
        );
        assert!(matches!(
            added,
            Some(Outcome::Edited {
                name: "Add Call",
                ..
            })
        ));
        let id = d.selected.iter().next().unwrap().clone();
        assert!(
            d.text.contains("module = asset(\"shaders/mapping.slang\""),
            "{}",
            d.text
        );
        d.relayout(&mut measure);
        assert_eq!(d.boxes[d.box_of(&id).unwrap()].title, "swirl");
        d.set_call_titles(&[(module, String::from("swirl"), String::from("Swirl"))]);
        d.relayout(&mut measure);
        assert_eq!(d.boxes[d.box_of(&id).unwrap()].title, "Swirl");
        let new = "float3(MaterialContext ctx, float3 color, float amount)";
        let mut functions = BTreeMap::new();
        functions.insert(
            module.file,
            vec![NodeFunction {
                name: String::from("swirl"),
                title: String::from("Swirl"),
                category: String::from("Mapping"),
                signature: Ok(String::from(new)),
            }],
        );
        assert!(d.refresh_calls(&|n| node_function_call(&functions, n)));
        assert_eq!(d.graph.nodes[&id].signature, new);
        assert!(d.text.contains("float amount"), "{}", d.text);
        assert!(!d.refresh_calls(&|n| node_function_call(&functions, n)));
        functions.get_mut(&module.file).unwrap()[0].signature = Err(String::from("no"));
        assert!(!d.refresh_calls(&|n| node_function_call(&functions, n)));
        assert_eq!(d.graph.nodes[&id].signature, new);
    }

    /// E10-3g-2b: a function file's call is added under an id from its name, and follows
    /// its function's name and ports with no step; the same again changes nothing.
    #[test]
    fn a_function_files_call_follows_its_name_and_ports() {
        let mut d = doc();
        let module = AssetRef::file(AssetId::from_u128(8));
        let old = "void(float3 color, out float3 tinted)";
        let added = d.add_call(
            FUNCTION_KIND,
            module,
            "materials/tint.mfunc",
            "tint",
            old,
            [0.0, 400.0],
            None,
        );
        assert!(matches!(
            added,
            Some(Outcome::Edited {
                name: "Add Call",
                ..
            })
        ));
        let id = d.selected.iter().next().unwrap().clone();
        assert_eq!(id, "tint1");
        assert!(d.text.contains("materials/tint.mfunc"), "{}", d.text);
        let new = "void(float3 color, float amount, out float3 tinted)";
        let current = |n: &Node| (n.module == module).then_some(("shade", new));
        assert!(d.refresh_calls(&current));
        assert_eq!(d.graph.nodes[&id].function, "shade");
        assert_eq!(d.graph.nodes[&id].signature, new);
        assert!(!d.refresh_calls(&current));
        assert_eq!(id_base("New Function"), "new_function");
        assert_eq!(id_base("2 tone"), "function");
        assert_eq!(
            function_name("materials/New Function.mfunc"),
            "New Function"
        );
    }

    /// E10-3g-2b: the selection made a function (Ctrl+G): its ports the links across its
    /// edge (an input per output outside read inside, named after the port reading it, as
    /// wide as what it carries; an output per output inside read outside), a call in its
    /// place linked as the nodes were, one step, the call made its function's nodes again
    /// drawing what they drew; the call taken apart (Ctrl+Alt+G) is the nodes under their
    /// own ids, linked as before. The surface alone, nodes nothing outside reads, and no
    /// call selected are refused.
    #[test]
    fn the_selection_is_made_a_function_and_taken_apart_again() {
        let mut d = doc();
        d.edit(|g| {
            g.add(
                "k",
                Node::new("number", [0.0, 240.0]).with_value([0.5, 0.0, 0.0, 0.0]),
            );
            g.add("m", Node::new("multiply", [150.0, 0.0]));
            g.link(PortRef::new("c", "rgba"), PortRef::new("m", "a"))
                .link(PortRef::new("k", "value"), PortRef::new("m", "b"))
                .link(PortRef::new("m", "out"), PortRef::new(OUTPUT, "base"));
        });
        d.relayout(&mut measure);
        let before = d.graph.clone();
        d.selected = [String::from("m")].into_iter().collect();
        let made = d.function_of_selection().unwrap();
        assert_eq!(made.signature, "void(float4 a, float b, out float4 base)");
        assert_eq!(made.function.nodes.len(), 4, "{:?}", made.function.nodes);
        let module = AssetRef::file(AssetId::from_u128(77));
        let path = "materials/New Function.mfunc";
        let outcome = d.call_made(&made, module, path, "New Function");
        assert!(matches!(
            outcome,
            Some(Outcome::Edited {
                name: "Make Function",
                ..
            })
        ));
        let call = d.selected.iter().next().unwrap().clone();
        assert_eq!(call, "new_function1");
        assert!(!d.graph.nodes.contains_key("m"));
        assert_eq!(
            d.graph.links[&PortRef::new(&call, "a")],
            PortRef::new("c", "rgba")
        );
        assert_eq!(
            d.graph.links[&PortRef::new(&call, "b")],
            PortRef::new("k", "value")
        );
        assert_eq!(
            d.graph.links[&PortRef::new(OUTPUT, "base")],
            PortRef::new(&call, "base")
        );
        assert!(d.text.contains(path), "{}", d.text);
        let functions = |r: AssetRef| (r == module).then(|| made.function.clone());
        let expanded = engine_material_graph::expand(&d.graph, &functions)
            .unwrap()
            .graph;
        assert_eq!(
            expanded.links[&PortRef::new(OUTPUT, "base")],
            PortRef::new(&format!("{call}__m"), "out")
        );
        // Taken apart: the product under its own id again, linked as before.
        let mut map = BTreeMap::new();
        map.insert(module, made.function.clone());
        let outcome = d.ungroup(&map, &AssetPaths::new());
        assert!(matches!(
            outcome,
            Ok(Some(Outcome::Edited {
                name: "Ungroup Function",
                ..
            }))
        ));
        assert_eq!(d.selected, [String::from("m")].into_iter().collect());
        let shape = |g: &Graph| {
            (
                g.nodes
                    .iter()
                    .map(|(id, n)| (id.clone(), n.kind.clone()))
                    .collect::<Vec<_>>(),
                g.links.clone(),
            )
        };
        assert_eq!(shape(&d.graph), shape(&before));
        // Refused: the surface alone, nodes nothing outside reads, no call to take apart.
        d.selected = [OUTPUT.to_owned()].into_iter().collect();
        let why = d.function_of_selection().unwrap_err();
        assert!(why.contains("Select the nodes"), "{why}");
        d.edit(|g| {
            g.add("lone", Node::new("number", [0.0, 400.0]));
        });
        d.selected = [String::from("lone")].into_iter().collect();
        let why = d.function_of_selection().unwrap_err();
        assert!(why.contains("Nothing outside reads"), "{why}");
        assert!(matches!(
            d.ungroup(&map, &AssetPaths::new()),
            Ok(Some(Outcome::Refused(_)))
        ));
    }

    /// E10-3g-1: Slang's first error placed in the preview's module goes to the node the
    /// line map gives its line, with the detail at its place (its title without one); one at
    /// a line of no node to none; warnings, and errors placed elsewhere, are passed over. The
    /// Expression node shows its code in its title and its inspector.
    #[test]
    fn slangs_error_goes_to_the_node_of_its_line() {
        let lines = vec![None, Some(String::from("e")), None];
        // As Slang 2026.18 writes them (indented here, read trimmed).
        let diagnostics = "H:\\engine\\forward.slang: shader compilation failed:
            warning[E30081]: implicit conversion not recommended
              --> C:/game/.ofelia/cache/graphs/g-editing.slang:2:3
            error[E30019]: type mismatch
              --> H:\\engine\\forward.slang:2874:31
               |
            2874 | x = 1;
               |     ^ elsewhere
            error[E30015]: undefined identifier
              --> C:/game/.ofelia/cache/graphs/g-editing.slang:2:20
               |
            2 | return a * oops;
               |            ^^^^ undefined identifier 'oops'.
            ---'";
        assert_eq!(
            slang_error(diagnostics, "g-editing.slang", &lines, &[]),
            Some((Some("e"), "undefined identifier 'oops'."))
        );
        assert_eq!(
            slang_error(diagnostics, "h-editing.slang", &lines, &[]),
            None
        );
        let titled = "error[E1]: no detail\n--> C:\\game\\g-editing.slang:3:1";
        assert_eq!(
            slang_error(titled, "g-editing.slang", &lines, &[]),
            Some((None, "no detail"))
        );
        // E10-3g-3b: an error in a module a call names is that call's.
        let calls = [(String::from("forward.slang"), String::from("sw"))];
        assert_eq!(
            slang_error(diagnostics, "h-editing.slang", &lines, &calls),
            Some((Some("sw"), "elsewhere"))
        );
        // The node: its code in its title, its row in the inspector.
        let mut d = doc();
        d.graph.add(
            "e",
            Node::new("expression", [0.0, 300.0]).with_code("a * 0.5 + sin(b) * 0.25 + c * d"),
        );
        d.revision += 1;
        d.relayout(&mut measure);
        let b = &d.boxes[d.box_of("e").unwrap()];
        assert_eq!(b.title, "Expression · a * 0.5 + sin(b) * 0.25…");
        d.selected = [String::from("e")].into_iter().collect();
        let mut rows = Vec::new();
        d.rows(&mut rows);
        assert_eq!(rows[0], NodeRow::Code);
        d.set_code("a.x").unwrap();
        assert_eq!(d.graph.nodes["e"].code, "a.x");
        assert!(d.text.contains("nodes[\"e\"].code = \"a.x\""), "{}", d.text);
    }

    /// E10-3e-2b: C puts the selected nodes in a frame (room round them, a title bar); its
    /// title bar drags it with the nodes it holds whole (not the others), its corner resizes
    /// it, its inside is the canvas's; its title is the inspector's; Delete takes it away
    /// and leaves its nodes.
    #[test]
    fn a_frame_moves_with_what_it_holds_and_goes_alone() {
        let mut d = doc();
        assert_eq!(
            d.frame_selection(),
            Some(Outcome::Refused("Select the nodes to put in a frame"))
        );
        d.selected = [String::from("c")].into_iter().collect();
        let Some(Outcome::Edited { name, .. }) = d.frame_selection() else {
            panic!("framed")
        };
        assert_eq!(name, "Frame Selection");
        let c = d.boxes[d.box_of("c").unwrap()].clone();
        let f = d.graph.frames["frame1"].clone();
        assert_eq!(
            f.at,
            [c.at[0] - FRAME_ROOM, c.at[1] - FRAME_ROOM - FRAME_TITLE]
        );
        assert_eq!(
            f.size,
            [
                (c.size[0] + 2.0 * FRAME_ROOM).round(),
                (c.size[1] + 2.0 * FRAME_ROOM + FRAME_TITLE).round()
            ]
        );
        assert!(d.selected.is_empty() && d.selected_frames.contains("frame1"));
        assert!(
            d.text.contains("frames[\"frame1\"].title = \"Frame\""),
            "{}",
            d.text
        );
        // Its title bar dragged: it and c move; r, outside it, stays.
        d.relayout(&mut measure);
        let mut drag = None;
        let p = |at: [f32; 2], press, held| Pointer {
            at: Some(at),
            over: true,
            press,
            held,
            ..Pointer::default()
        };
        let title = [f.at[0] + 30.0, f.at[1] + 10.0];
        assert_eq!(d.hit(title, PORT_REACH), Hit::Frame(0));
        let to = [title[0] + 50.0, title[1] + 20.0];
        d.pointer(&p(title, true, true), [0.0; 2], &mut drag);
        d.pointer(&p(to, false, true), [0.0; 2], &mut drag);
        let Some(Outcome::Edited { name, .. }) =
            d.pointer(&p(to, false, false), [0.0; 2], &mut drag)
        else {
            panic!("moved")
        };
        assert_eq!(name, "Move Frame");
        assert_eq!(
            d.graph.frames["frame1"].at,
            [f.at[0] + 50.0, f.at[1] + 20.0]
        );
        assert_eq!(d.graph.nodes["c"].at, [c.at[0] + 50.0, c.at[1] + 20.0]);
        assert_eq!(d.graph.nodes["r"].at, [0.0, 120.0]);
        // Its corner: bigger by the drag.
        d.relayout(&mut measure);
        let f = d.graph.frames["frame1"].clone();
        let corner = [f.at[0] + f.size[0] - 4.0, f.at[1] + f.size[1] - 4.0];
        assert_eq!(d.hit(corner, PORT_REACH), Hit::FrameCorner(0));
        let far = [corner[0] + 100.0, corner[1] + 60.0];
        d.pointer(&p(corner, true, true), [0.0; 2], &mut drag);
        d.pointer(&p(far, false, true), [0.0; 2], &mut drag);
        let Some(Outcome::Edited { name, .. }) =
            d.pointer(&p(far, false, false), [0.0; 2], &mut drag)
        else {
            panic!("resized")
        };
        assert_eq!(name, "Resize Frame");
        assert_eq!(
            d.graph.frames["frame1"].size,
            [f.size[0] + 100.0, f.size[1] + 60.0]
        );
        // Its inside, off its boxes: the canvas.
        d.relayout(&mut measure);
        let f = d.graph.frames["frame1"].clone();
        assert_eq!(
            d.hit(
                [f.at[0] + f.size[0] - 30.0, f.at[1] + FRAME_TITLE + 4.0],
                PORT_REACH
            ),
            Hit::Canvas
        );
        // Selected alone: its title in the inspector.
        d.selected.clear();
        d.selected_frames = [String::from("frame1")].into_iter().collect();
        let mut rows = Vec::new();
        d.rows(&mut rows);
        assert_eq!(rows, [NodeRow::Title]);
        assert!(!d.changed(NodeRow::Title));
        d.set_title("Moss on top").unwrap();
        assert_eq!(d.graph.frames["frame1"].title, "Moss on top");
        assert!(d.changed(NodeRow::Title));
        // Deleted: the frame goes, c stays.
        let Some(Outcome::Edited { name, .. }) = d.delete_selected() else {
            panic!("deleted")
        };
        assert_eq!(name, "Delete Frame");
        assert!(d.graph.frames.is_empty() && d.graph.nodes.contains_key("c"));
    }

    #[test]
    fn the_view_zooms_about_the_pointer_by_steps_and_frames_what_it_is_asked_to() {
        let mut v = View::default();
        let origin = [100.0, 50.0];
        let q = [400.0, 300.0];
        let under = v.canvas_point(q, origin);
        v.zoom_by(2, q, origin);
        assert_eq!(v.zoom(), 1.5);
        let after = v.canvas_point(q, origin);
        assert!((after[0] - under[0]).abs() < 1e-3 && (after[1] - under[1]).abs() < 1e-3);
        v.zoom_by(-40, q, origin);
        assert_eq!(v.zoom(), ZOOMS[0], "held at the last step");
        assert_eq!(
            v.canvas_point(v.panel_point([7.0, 9.0], origin), origin),
            [7.0, 9.0]
        );
        // A drag moves the canvas with the pointer.
        let mut v = View::default();
        let before = v.panel_point([10.0, 10.0], origin);
        v.pan_by([30.0, -20.0]);
        let moved = v.panel_point([10.0, 10.0], origin);
        assert_eq!([moved[0] - before[0], moved[1] - before[1]], [30.0, -20.0]);
        // Framed: centered, at most zoom 1, a step down where it does not fit.
        v.frame([[0.0, 0.0], [200.0, 100.0]], [800.0, 600.0]);
        assert_eq!(v.zoom(), 1.0);
        assert_eq!(v.panel_point([100.0, 50.0], [0.0; 2]), [400.0, 300.0]);
        v.frame([[0.0, 0.0], [2000.0, 100.0]], [800.0, 600.0]);
        assert_eq!(v.zoom(), 0.33, "{}", v.zoom());
    }

    #[test]
    fn boxes_fit_their_texts_and_the_surface_shows_its_models_inputs() {
        let d = doc();
        let out = &d.boxes[d.box_of(OUTPUT).unwrap()];
        assert_eq!(
            out.inputs,
            [
                "base",
                "normal",
                "metallic",
                "roughness",
                "emission",
                "alpha",
                "alpha_cutoff",
                "occlusion",
                "thickness"
            ],
            "lit reads no clear coat, sheen, hair or toon"
        );
        assert_eq!(out.title, "Surface");
        let r = &d.boxes[d.box_of("r").unwrap()];
        assert_eq!(r.title, "Number · roughness");
        assert!(r.size[0] >= measure("Number · roughness", TITLE_TEXT) + 2.0 * PAD);
        assert_eq!(r.size[1], TITLE + ROW + PAD * 0.5, "its value's row");
        // A clear coat model shows its own; a linked input shows whatever the model.
        let mut g = d.graph.clone();
        g.model = Model::Clearcoat;
        g.link(PortRef::new("r", "value"), PortRef::new(OUTPUT, "rim"));
        let mut boxes = Vec::new();
        layout(&g, &[], false, &mut measure, &mut boxes);
        let out = boxes.iter().find(|b| b.id == OUTPUT).unwrap();
        assert!(out.inputs.contains(&"clearcoat") && out.inputs.contains(&"rim"));
        assert!(!out.inputs.contains(&"sheen"));
    }

    /// E10-4g: with previews, a node with an output has a tile under its ports, its box
    /// taller by the tile and its room, at least as wide; the surface and a reroute have
    /// none; without previews, no node has one. A function call has one, and so do a
    /// function's input and output (E10-4g-3).
    #[test]
    fn previews_give_nodes_a_tile_under_their_ports() {
        use engine_material_graph::function::{input_kind, output_kind};
        let mut g = Graph::new(Model::Lit);
        g.add("c", Node::new("color", [0.0, 0.0]))
            .add("r", Node::new("reroute", [200.0, 0.0]))
            .add(
                "f",
                Node::new(FUNCTION_KIND, [0.0, 200.0])
                    .with_call("tint", "void(float3 color, out float3 tinted)"),
            )
            .add("i", Node::new(input_kind(3), [0.0, 400.0]).named("color"))
            .add(
                "o",
                Node::new(output_kind(3), [200.0, 400.0]).named("tinted"),
            );
        let mut measure = |s: &str, size: f32| s.len() as f32 * size * 0.5;
        let (mut plain, mut tiled) = (Vec::new(), Vec::new());
        layout(&g, &[], false, &mut measure, &mut plain);
        layout(&g, &[], true, &mut measure, &mut tiled);
        let find = |boxes: &[NodeBox], id: &str| boxes.iter().find(|b| b.id == id).cloned();
        let (c0, c1) = (find(&plain, "c").unwrap(), find(&tiled, "c").unwrap());
        assert!(c0.tile_rect().is_none());
        let [lo, hi] = c1.tile_rect().unwrap();
        assert_eq!([hi[0] - lo[0], hi[1] - lo[1]], [TILE, TILE]);
        assert!(lo[1] >= c1.at[1] + TITLE + ROW, "under its ports");
        assert!(hi[1] < c1.at[1] + c1.size[1] && lo[0] > c1.at[0]);
        assert_eq!(c1.size[1], c0.size[1] + TILE + 2.0 * TILE_ROOM);
        assert!(c1.size[0] >= TILE + 2.0 * PAD);
        assert!(
            find(&tiled, "r").unwrap().tile_rect().is_none(),
            "a reroute"
        );
        assert!(
            find(&tiled, OUTPUT).unwrap().tile_rect().is_none(),
            "the surface"
        );
        for id in ["f", "i", "o"] {
            assert!(find(&tiled, id).unwrap().tile_rect().is_some(), "{id}");
        }
        assert!(plain.iter().all(|b| b.tile_rect().is_none()));
    }

    #[test]
    fn a_point_hits_the_port_near_it_before_the_box_and_the_top_box_first() {
        let mut d = doc();
        let c = d.box_of("c").unwrap();
        let out = d.box_of(OUTPUT).unwrap();
        let rgba = d.boxes[c].output_at(0);
        assert_eq!(
            d.hit([rgba[0] - 3.0, rgba[1] + 2.0], 9.0),
            Hit::Output(c, 0)
        );
        let base = d.boxes[out].input_at(0);
        assert_eq!(d.hit(base, 9.0), Hit::Input(out, 0));
        assert_eq!(d.hit([5.0, 5.0], 9.0), Hit::Node(c));
        assert_eq!(d.hit([-50.0, -50.0], 9.0), Hit::Canvas);
        // Overlapping boxes: the selected one is on top.
        d.graph.nodes.get_mut("r").unwrap().at = [0.0, 0.0];
        d.revision += 1;
        d.relayout(&mut measure);
        let r = d.box_of("r").unwrap();
        assert_eq!(
            d.hit([5.0, 5.0], 9.0),
            Hit::Node(r),
            "the later box is on top"
        );
        d.selected.insert(String::from("c"));
        assert_eq!(d.hit([5.0, 5.0], 9.0), Hit::Node(c));
        assert_eq!(
            d.order().collect::<Vec<_>>(),
            [d.box_of(OUTPUT).unwrap(), r, c]
        );
    }

    #[test]
    fn links_leave_and_arrive_level_and_loops_are_refused() {
        let (a, b) = ([0.0, 0.0], [200.0, 100.0]);
        assert_eq!(link_point(a, b, 0.0), a);
        assert_eq!(link_point(a, b, 1.0), b);
        let near_a = link_point(a, b, 0.01);
        assert!(near_a[1].abs() < 0.1 && near_a[0] > 0.0, "{near_a:?}");
        let mut g = doc().graph;
        g.add("m", Node::new("multiply", [150.0, 0.0]));
        connect(&mut g, PortRef::new("c", "rgba"), PortRef::new("m", "a")).unwrap();
        assert!(depends_on(&g, "m", "c") && !depends_on(&g, "c", "m"));
        assert_eq!(
            connect(&mut g, PortRef::new("m", "out"), PortRef::new("m", "b")),
            Err("A node cannot feed itself")
        );
        g.add("n", Node::new("multiply", [200.0, 0.0]));
        connect(&mut g, PortRef::new("m", "out"), PortRef::new("n", "a")).unwrap();
        assert_eq!(
            connect(&mut g, PortRef::new("n", "out"), PortRef::new("m", "b")),
            Err("That link would loop back into its own node")
        );
        // Deleted nodes take their links; the surface stays.
        let ids = ["m", OUTPUT].map(String::from).into_iter().collect();
        assert_eq!(delete(&mut g, &ids), 1);
        assert!(g.nodes.contains_key(OUTPUT));
        assert!(g.links.keys().all(|to| to.node != "m") && g.links.values().all(|f| f.node != "m"));
        assert!(!g.links.contains_key(&PortRef::new("n", "a")));
    }

    /// A frame of the pointer at canvas point `c` (the panel at the origin, zoom 1).
    fn at(
        d: &mut GraphDoc,
        drag: &mut Option<Drag>,
        c: [f32; 2],
        press: bool,
        held: bool,
    ) -> Option<Outcome> {
        let q = d.view.panel_point(c, [0.0; 2]);
        let p = Pointer {
            at: Some(q),
            over: true,
            press,
            held,
            ..Pointer::default()
        };
        let outcome = d.pointer(&p, [0.0; 2], drag);
        d.relayout(&mut measure);
        outcome
    }

    #[test]
    fn the_pointer_links_lifts_moves_selects_and_escape_takes_a_drag_back() {
        let mut d = doc();
        d.view.pan = [-50.0, -50.0];
        let mut drag = None;
        let (c, r, out) = (
            d.box_of("c").unwrap(),
            d.box_of("r").unwrap(),
            d.box_of(OUTPUT).unwrap(),
        );
        // From the number's output onto the surface's roughness: a link, one step.
        let value = d.boxes[r].output_at(0);
        let roughness = d.boxes[out].input_at(3);
        assert_eq!(at(&mut d, &mut drag, value, true, true), None);
        assert!(matches!(drag, Some(Drag::Link { output: true, .. })));
        at(&mut d, &mut drag, [200.0, 80.0], false, true);
        let Some(Outcome::Edited {
            name,
            before,
            after,
        }) = at(&mut d, &mut drag, roughness, false, false)
        else {
            panic!("linked")
        };
        assert_eq!(name, "Link");
        assert!(
            !before.contains("out.roughness")
                && after.contains("links[\"out.roughness\"] = \"r.value\""),
            "{after}"
        );
        assert!(drag.is_none());
        // The base's link lifted off and dropped on the empty canvas: taken out.
        let base = d.boxes[out].input_at(0);
        at(&mut d, &mut drag, base, true, true);
        assert!(matches!(&drag, Some(Drag::Link { lifted: Some(l), .. }) if l.port == "base"));
        let Some(Outcome::Edited { name, .. }) =
            at(&mut d, &mut drag, [600.0, 400.0], false, false)
        else {
            panic!("removed")
        };
        assert_eq!(name, "Remove Link");
        assert!(!d.graph.links.contains_key(&PortRef::new(OUTPUT, "base")));
        // From an unlinked input back to an output: a link the other way.
        let base = d.boxes[out].input_at(0);
        let rgba = d.boxes[c].output_at(0);
        at(&mut d, &mut drag, base, true, true);
        assert!(matches!(drag, Some(Drag::Link { output: false, .. })));
        assert!(matches!(
            at(&mut d, &mut drag, rgba, false, false),
            Some(Outcome::Edited { name: "Link", .. })
        ));
        assert_eq!(
            d.graph.links[&PortRef::new(OUTPUT, "base")],
            PortRef::new("c", "rgba")
        );
        // A press on a box selects it; a drag moves it, ending on whole units.
        let grab = [d.boxes[c].at[0] + 10.0, d.boxes[c].at[1] + 5.0];
        at(&mut d, &mut drag, grab, true, true);
        assert_eq!(d.selected.iter().collect::<Vec<_>>(), ["c"]);
        at(
            &mut d,
            &mut drag,
            [grab[0] + 40.3, grab[1] + 19.6],
            false,
            true,
        );
        let moved = d.graph.nodes["c"].at;
        assert!(
            (moved[0] - 40.3).abs() < 1e-3 && (moved[1] - 19.6).abs() < 1e-3,
            "{moved:?}"
        );
        let Some(Outcome::Edited { name, .. }) = at(
            &mut d,
            &mut drag,
            [grab[0] + 40.3, grab[1] + 19.6],
            false,
            false,
        ) else {
            panic!("moved")
        };
        assert_eq!(name, "Move Node");
        assert_eq!(d.graph.nodes["c"].at, [40.0, 20.0]);
        // A click on a box without moving it is no step.
        let grab = [d.boxes[c].at[0] + 10.0, d.boxes[c].at[1] + 5.0];
        at(&mut d, &mut drag, grab, true, true);
        assert_eq!(at(&mut d, &mut drag, grab, false, false), None);
        // Escape takes a move back.
        at(&mut d, &mut drag, grab, true, true);
        at(&mut d, &mut drag, [grab[0] + 100.0, grab[1]], false, true);
        assert_eq!(d.graph.nodes["c"].at, [140.0, 20.0]);
        let escape = Pointer {
            escape: true,
            ..Pointer::default()
        };
        assert_eq!(d.pointer(&escape, [0.0; 2], &mut drag), None);
        assert_eq!(d.graph.nodes["c"].at, [40.0, 20.0]);
        assert!(drag.is_none());
        // A box on the empty canvas selects what it meets; a click there lets go.
        at(&mut d, &mut drag, [-40.0, 100.0], true, true);
        at(&mut d, &mut drag, [20.0, 160.0], false, true);
        at(&mut d, &mut drag, [20.0, 160.0], false, false);
        assert_eq!(d.selected.iter().collect::<Vec<_>>(), ["r"]);
        at(&mut d, &mut drag, [-40.0, 400.0], true, true);
        at(&mut d, &mut drag, [-40.0, 400.0], false, false);
        assert!(d.selected.is_empty());
        // A loop refused, the graph as it was.
        d.graph.add("m", Node::new("multiply", [100.0, 300.0]));
        d.revision += 1;
        d.relayout(&mut measure);
        d.graph
            .links
            .insert(PortRef::new("m", "a"), PortRef::new("c", "rgba"));
        d.revision += 1;
        d.relayout(&mut measure);
        let m = d.box_of("m").unwrap();
        let c = d.box_of("c").unwrap();
        let text = d.text.clone();
        let m_out = d.boxes[m].output_at(0);
        at(&mut d, &mut drag, m_out, true, true);
        let c_box = d.boxes[c].clone();
        assert!(
            c_box.inputs.is_empty(),
            "a colour has no input to loop into"
        );
        // Into m's own b: a node cannot feed itself.
        let b = d.boxes[m].input_at(1);
        assert_eq!(
            at(&mut d, &mut drag, b, false, false),
            Some(Outcome::Refused("A node cannot feed itself"))
        );
        assert_eq!(d.text, text);
    }

    #[test]
    fn duplicates_take_their_links_and_ctrl_drag_carries_an_outputs_links() {
        let mut d = doc();
        // Number r into the surface's roughness and metallic.
        d.graph
            .link(
                PortRef::new("r", "value"),
                PortRef::new(OUTPUT, "roughness"),
            )
            .link(PortRef::new("r", "value"), PortRef::new(OUTPUT, "metallic"));
        d.graph.add("m", Node::new("multiply", [150.0, 200.0]));
        d.graph
            .link(PortRef::new("c", "rgba"), PortRef::new("m", "a"))
            .link(PortRef::new("r", "value"), PortRef::new("m", "b"));
        d.revision += 1;
        d.relayout(&mut measure);
        // Ctrl+D on m and r: copies with ids and a parameter of their own, m's copy fed by
        // r's copy and still by c (not copied); the copies selected.
        d.selected = ["m", "r"].map(String::from).into_iter().collect();
        let Some(Outcome::Edited { name, .. }) = d.duplicate_selected() else {
            panic!("duplicated")
        };
        assert_eq!(name, "Duplicate Nodes");
        assert_eq!(
            d.selected.iter().collect::<Vec<_>>(),
            ["multiply1", "number1"]
        );
        assert_eq!(d.graph.nodes["number1"].param, "roughness_2");
        assert_eq!(d.graph.nodes["number1"].at, [30.0, 150.0]);
        assert_eq!(
            d.graph.links[&PortRef::new("multiply1", "b")],
            PortRef::new("number1", "value")
        );
        assert_eq!(
            d.graph.links[&PortRef::new("multiply1", "a")],
            PortRef::new("c", "rgba")
        );
        assert!(
            !d.graph.links.values().any(|f| f.node == "number1"
                && d.graph
                    .links
                    .keys()
                    .any(|t| t.node == OUTPUT && d.graph.links[t] == *f)),
            "the surface not fed by a copy"
        );
        d.selected = [OUTPUT.to_owned()].into_iter().collect();
        assert_eq!(
            d.duplicate_selected(),
            Some(Outcome::Refused(
                "The surface is not copied: a graph has one"
            ))
        );
        // Ctrl+drag from r's output onto number1's: its three links carried over, one step.
        d.relayout(&mut measure);
        let mut drag = None;
        let (r, n) = (d.box_of("r").unwrap(), d.box_of("number1").unwrap());
        let (from, onto) = (d.boxes[r].output_at(0), d.boxes[n].output_at(0));
        let p = |c: [f32; 2], press, held| Pointer {
            at: Some(c),
            over: true,
            press,
            held,
            ctrl: true,
            ..Pointer::default()
        };
        assert_eq!(d.pointer(&p(from, true, true), [0.0; 2], &mut drag), None);
        assert!(matches!(&drag, Some(Drag::Yank { inputs, .. }) if inputs.len() == 3));
        let Some(Outcome::Edited { name, .. }) =
            d.pointer(&p(onto, false, false), [0.0; 2], &mut drag)
        else {
            panic!("carried")
        };
        assert_eq!(name, "Move Links");
        assert!(d.graph.links.values().all(|f| f.node != "r"));
        assert_eq!(
            d.graph.links[&PortRef::new(OUTPUT, "metallic")],
            PortRef::new("number1", "value")
        );
        // Carried onto an output fed by them: refused, all of them kept where they were.
        d.relayout(&mut measure);
        let (n, m1) = (d.box_of("number1").unwrap(), d.box_of("multiply1").unwrap());
        let text = d.text.clone();
        d.pointer(&p(d.boxes[n].output_at(0), true, true), [0.0; 2], &mut drag);
        assert_eq!(
            d.pointer(
                &p(d.boxes[m1].output_at(0), false, false),
                [0.0; 2],
                &mut drag
            ),
            Some(Outcome::Refused("A node cannot feed itself"))
        );
        assert_eq!(d.text, text);
        // Dropped on the canvas: taken out.
        d.pointer(&p(d.boxes[n].output_at(0), true, true), [0.0; 2], &mut drag);
        let Some(Outcome::Edited { name, .. }) =
            d.pointer(&p([-400.0, -400.0], false, false), [0.0; 2], &mut drag)
        else {
            panic!("removed")
        };
        assert_eq!(name, "Remove Links");
        assert!(d.graph.links.values().all(|f| f.node != "number1"));
    }

    #[test]
    fn a_starter_graph_draws_a_gltf_materials_own_look() {
        // No look: the standard material's grey and numbers.
        let (g, images, data) = starter(None, &mut |_| None);
        assert!(images.is_empty() && data.is_empty());
        assert_eq!(g.nodes["base_color"].value, [0.8, 0.8, 0.8, 1.0]);
        assert_eq!(g.nodes["roughness"].value[0], 0.5);
        assert_eq!(
            g.links[&PortRef::new(OUTPUT, "metallic")],
            PortRef::new("metallic", "value")
        );
        engine_material_graph::generate(&g, "GraphMaterial", engine_material_graph::Build::Saved)
            .unwrap();
        // Wood: its colour map times its tint, its metal/rough map's channels (roughness
        // times 0.7), its normal map through a Normal Map node, its glow alone; a map whose
        // file the project lacks left out.
        let wood = OutlineMaps {
            base_color: [0.9, 0.8, 0.7, 1.0],
            metallic: 1.0,
            roughness: 0.7,
            emissive: [0.2, 0.1, 0.0],
            base_color_image: Some(String::from("wood.png")),
            metallic_roughness_image: Some(String::from("wood_mr.png")),
            normal_image: Some(String::from("wood_n.png")),
            occlusion_image: Some(String::from("gone.png")),
            emissive_image: None,
        };
        let ids: BTreeMap<&str, AssetRef> = ["wood.png", "wood_mr.png", "wood_n.png"]
            .into_iter()
            .map(|p| (p, AssetRef::file(AssetId::new())))
            .collect();
        let (g, images, data) = starter(Some(&wood), &mut |uri| {
            ids.get(uri).map(|&r| (r, format!("props/{uri}")))
        });
        assert_eq!(images.len(), 3);
        assert_eq!(data, ["metallic_roughness_map"]);
        assert_eq!(g.nodes["base_map"].texture, ids["wood.png"]);
        assert_eq!(
            g.links[&PortRef::new("base", "a")],
            PortRef::new("base_map", "rgba")
        );
        assert_eq!(g.nodes["tint"].value, wood.base_color);
        assert_eq!(
            g.links[&PortRef::new(OUTPUT, "metallic")],
            PortRef::new("mr_map", "b"),
            "a factor of 1: the channel itself"
        );
        assert_eq!(
            g.links[&PortRef::new("roughness_times", "a")],
            PortRef::new("mr_map", "g")
        );
        assert_eq!(
            g.links[&PortRef::new(OUTPUT, "normal")],
            PortRef::new("normal", "normal")
        );
        assert_eq!(
            g.links[&PortRef::new(OUTPUT, "emission")],
            PortRef::new("glow", "rgba")
        );
        assert!(!g.nodes.contains_key("occlusion_map"));
        let module = engine_material_graph::generate(
            &g,
            "GraphMaterial",
            engine_material_graph::Build::Saved,
        )
        .unwrap();
        assert!(module.text.contains("struct GraphMaterial : IMaterial"));
    }

    #[test]
    fn the_search_adds_nodes_linked_by_the_port_that_fits_and_images_become_textures() {
        // The kinds offered: never the surface; from an output, those with an input.
        assert!(addable(None, false).all(|k| k.name != OUTPUT_KIND));
        assert!(addable(Some(true), false).all(|k| !k.inputs.is_empty()));
        assert!(
            addable(Some(true), false).any(|k| k.name == "texture"),
            "its uv input"
        );
        assert!(addable(Some(false), false).all(|k| !k.outputs.is_empty()));
        assert_eq!(ports_text(library::kind("multiply").unwrap()), "a, b → out");
        assert_eq!(ports_text(library::kind("number").unwrap()), "→ value");
        let mut d = doc();
        // Added plainly: an id of its own, on whole units, selected.
        let Some(Outcome::Edited { name, .. }) = d.add_node("multiply", [10.4, 520.6], None) else {
            panic!("added")
        };
        assert_eq!(name, "Add Node");
        assert_eq!(d.graph.nodes["multiply1"].at, [10.0, 521.0]);
        assert_eq!(d.selected.iter().collect::<Vec<_>>(), ["multiply1"]);
        // From the colour's four-wide output: into a dynamic input (none is four wide).
        let from = (PortRef::new("c", "rgba"), true);
        d.add_node("one_minus", [0.0, 300.0], Some(&from)).unwrap();
        let input = library::kind("one_minus").unwrap().inputs[0].name;
        assert_eq!(
            d.graph.links[&PortRef::new("one_minus1", input)],
            PortRef::new("c", "rgba")
        );
        // Into the surface's roughness (a scalar): a number's value.
        let into = (PortRef::new(OUTPUT, "roughness"), false);
        d.add_node("number", [200.0, 300.0], Some(&into)).unwrap();
        assert_eq!(
            d.graph.links[&PortRef::new(OUTPUT, "roughness")],
            PortRef::new("number1", "value")
        );
        assert_eq!(
            d.add_node(OUTPUT_KIND, [0.0; 2], None),
            Some(Outcome::Refused("A graph has one surface"))
        );
        // A link dropped on the empty canvas asks for the search there.
        d.relayout(&mut measure);
        let mut drag = None;
        let rgba = d.boxes[d.box_of("c").unwrap()].output_at(0);
        at(&mut d, &mut drag, rgba, true, true);
        assert_eq!(
            at(&mut d, &mut drag, [-300.0, 500.0], false, false),
            Some(Outcome::Search {
                at: [-300.0, 500.0],
                from: (PortRef::new("c", "rgba"), true)
            })
        );
        // An image from the dock: a texture node named after its file, its path known.
        let image = AssetRef::file(AssetId::new());
        d.add_texture(image, "textures/Wood Grain.png", [0.0, 600.0])
            .unwrap();
        assert_eq!(d.graph.nodes["texture1"].param, "wood_grain");
        assert_eq!(d.graph.nodes["texture1"].texture, image);
        assert!(d.text.contains("textures/Wood Grain.png"), "{}", d.text);
        d.add_texture(image, "textures/Wood Grain.png", [0.0, 700.0])
            .unwrap();
        assert_eq!(d.graph.nodes["texture2"].param, "wood_grain_2");
        assert_eq!(param_of("maps/2k-albedo.png"), "texture_2k_albedo");
    }

    #[test]
    fn the_inspector_shows_the_node_alone_selected_or_the_graph_and_edits_its_values() {
        let mut d = doc();
        let mut rows = Vec::new();
        // None selected: the graph's model.
        d.rows(&mut rows);
        assert_eq!(rows, [NodeRow::Model]);
        let mut texts = Vec::new();
        assert_eq!(d.row_texts(NodeRow::Model, &mut texts, 0), 1);
        assert_eq!(texts[0], "lit");
        assert_eq!(
            MODEL_NAMES.map(String::from),
            Model::ALL.map(|m| m.name().to_owned())
        );
        let (before, _) = d.set_model(1).unwrap();
        assert_eq!(d.graph.model, Model::Unlit);
        assert!(d.changed(NodeRow::Model));
        d.put_text(&before).unwrap();
        // The colour alone: its parameter, its value as a colour's channels, its note.
        d.selected.insert(String::from("c"));
        d.rows(&mut rows);
        assert_eq!(rows, [NodeRow::Param, NodeRow::Value(4), NodeRow::Comment]);
        assert_eq!(NodeRow::Value(4).name(true), "Color");
        assert_eq!(NodeRow::Value(4).labels(true), ["R", "G", "B", "A"]);
        assert_eq!(d.row_texts(NodeRow::Value(4), &mut texts, 0), 4);
        assert_eq!(texts[..4], ["0.9", "0.1", "0.1", "1"]);
        d.set_numbers(NodeRow::Value(4), [0.2, 0.4, 0.6, 1.0])
            .unwrap();
        assert_eq!(d.graph.nodes["c"].value, [0.2, 0.4, 0.6, 1.0]);
        // Its parameter: a Slang field's name, no other node's.
        assert!(d.set_param("tint").unwrap().is_some());
        assert_eq!(d.graph.nodes["c"].param, "tint");
        assert_eq!(
            d.set_param("roughness"),
            Err("Another node's parameter has that name")
        );
        assert!(d.set_param("2nd").is_err() && d.set_param("a b").is_err());
        assert!(d.changed(NodeRow::Param));
        d.reset(NodeRow::Param).unwrap();
        assert!(d.graph.nodes["c"].param.is_empty());
        // Several selected: the graph again.
        d.selected.insert(String::from("r"));
        d.rows(&mut rows);
        assert_eq!(rows, [NodeRow::Model]);
        // A Multiply: its unlinked inputs' own values (dynamic: one number filling all).
        d.selected.clear();
        d.graph.add("m", Node::new("multiply", [0.0, 300.0]));
        d.graph
            .link(PortRef::new("c", "rgba"), PortRef::new("m", "a"));
        d.revision += 1;
        d.selected.insert(String::from("m"));
        d.rows(&mut rows);
        assert_eq!(
            rows,
            [NodeRow::Input("b", 1), NodeRow::Comment],
            "a is linked"
        );
        assert_eq!(
            d.numbers(NodeRow::Input("b", 1))[0],
            1.0,
            "its kind's value"
        );
        assert!(!d.changed(NodeRow::Input("b", 1)));
        d.set_numbers(NodeRow::Input("b", 1), [0.5, 0.0, 0.0, 0.0])
            .unwrap();
        assert_eq!(d.graph.nodes["m"].inputs["b"], [0.5; 4]);
        d.reset(NodeRow::Input("b", 1)).unwrap();
        assert!(d.graph.nodes["m"].inputs.is_empty());
        // The surface: its model's inputs with values, the linked base and the normal (an
        // expression of the fragment) left out.
        d.selected = [OUTPUT.to_owned()].into_iter().collect();
        d.rows(&mut rows);
        assert!(rows.contains(&NodeRow::Input("roughness", 1)));
        assert!(rows.contains(&NodeRow::Input("emission", 3)));
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r, NodeRow::Input("base" | "normal" | "clearcoat", _)))
        );
        // A texture: its parameter and its image, by its file's name.
        let image = AssetRef::file(AssetId::new());
        d.graph.add("t", Node::new("texture", [0.0, 500.0]));
        d.revision += 1;
        d.selected = [String::from("t")].into_iter().collect();
        d.rows(&mut rows);
        assert_eq!(rows, [NodeRow::Param, NodeRow::Texture, NodeRow::Comment]);
        d.row_texts(NodeRow::Texture, &mut texts, 0);
        assert_eq!(texts[0], "none");
        d.set_texture(image, "textures/wood.png").unwrap();
        d.row_texts(NodeRow::Texture, &mut texts, 0);
        assert_eq!(texts[0], "wood.png");
        assert!(d.text.contains("textures/wood.png"));
        d.set_comment("the grain").unwrap();
        assert_eq!(d.graph.nodes["t"].comment, "the grain");
    }

    #[test]
    fn the_preview_draws_the_editing_build_whose_values_change_no_code() {
        let mut d = doc();
        // The roughness into the surface: only what the surface reaches is in the code.
        d.graph.link(
            PortRef::new("r", "value"),
            PortRef::new(OUTPUT, "roughness"),
        );
        let module = AssetRef::file(AssetId::new());
        let (generated, file, paths) = preview_material(
            &d,
            std::path::Path::new("."),
            &|_| None,
            module,
            ".ofelia/cache/graphs/x-editing.slang",
        )
        .unwrap();
        let text = generated.text;
        assert!(text.contains("struct GraphMaterial : IMaterial"), "{text}");
        assert_eq!(file.shader, module);
        assert_eq!(file.type_name, GRAPH_TYPE);
        assert_eq!(
            paths.get(module),
            Some(".ofelia/cache/graphs/x-editing.slang")
        );
        // Every number a parameter: the named roughness, the colour by its node.
        assert_eq!(file.numbers.get("roughness"), Some(&0.6));
        assert_eq!(
            file.vectors.get("_c").map(Vec::as_slice),
            Some(&[0.9, 0.1, 0.1, 1.0][..])
        );
        // A value changed: the same code, the new value.
        d.selected.insert(String::from("c"));
        d.set_numbers(NodeRow::Value(4), [0.1, 0.8, 0.2, 1.0])
            .unwrap();
        let (again, file, _) = preview_material(
            &d,
            std::path::Path::new("."),
            &|_| None,
            module,
            ".ofelia/cache/graphs/x-editing.slang",
        )
        .unwrap();
        let again = again.text;
        assert_eq!(again, text);
        assert_eq!(
            file.vectors.get("_c").map(Vec::as_slice),
            Some(&[0.1, 0.8, 0.2, 1.0][..])
        );
        // A texture into a Normal Map node: its image, a normal map's.
        let image = AssetRef::file(AssetId::new());
        d.add_texture(image, "textures/bumps.png", [0.0, 600.0])
            .unwrap();
        d.graph.add("nm", Node::new("normal_map", [150.0, 600.0]));
        d.graph
            .link(
                PortRef::new("texture1", "rgba"),
                PortRef::new("nm", "color"),
            )
            .link(PortRef::new("nm", "normal"), PortRef::new(OUTPUT, "normal"));
        let (_, file, paths) = preview_material(
            &d,
            std::path::Path::new("."),
            &|_| None,
            module,
            ".ofelia/cache/graphs/x-editing.slang",
        )
        .unwrap();
        assert_eq!(file.textures["bumps"].image, image);
        assert_eq!(
            file.textures["bumps"].encoding,
            engine_assets::material_file::TextureEncoding::Normal
        );
        assert_eq!(paths.get(image), Some("textures/bumps.png"));
    }

    #[test]
    fn deleting_keeps_the_surface_and_undoing_puts_the_text_back() {
        let mut d = doc();
        d.selected.insert(OUTPUT.to_owned());
        assert_eq!(
            d.delete_selected(),
            Some(Outcome::Refused("The surface stays: every graph has one"))
        );
        d.selected.insert(String::from("c"));
        let Some(Outcome::Edited {
            name,
            before,
            after,
        }) = d.delete_selected()
        else {
            panic!("deleted")
        };
        assert_eq!(name, "Delete Node");
        assert!(!d.graph.nodes.contains_key("c") && d.graph.links.is_empty());
        assert_eq!(d.selected.iter().collect::<Vec<_>>(), [OUTPUT]);
        assert!(d.dirty());
        d.put_text(&before).unwrap();
        assert!(d.graph.nodes.contains_key("c") && !d.dirty());
        d.put_text(&after).unwrap();
        assert!(!d.graph.nodes.contains_key("c"));
        d.refresh_tab();
        assert_eq!(d.tab, "glow.mgraph *");
        d.saved_now();
        assert!(!d.dirty());
        d.refresh_tab();
        assert_eq!(d.tab, "glow.mgraph");
    }

    #[test]
    fn a_graph_file_opens_and_values_read_short() {
        let dir = std::env::temp_dir().join(format!("graph_doc_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("materials")).unwrap();
        let d = doc();
        std::fs::write(dir.join("materials/glow.mgraph"), &d.text).unwrap();
        let opened = GraphDoc::open(&dir, "materials/glow.mgraph").unwrap();
        assert_eq!(opened.id, d.id);
        assert_eq!(opened.graph, d.graph);
        assert!(!opened.dirty());
        assert!(GraphDoc::open(&dir, "materials/none.mgraph").is_err());
        let mut s = String::new();
        value_text(&mut s, &[0.6, 2.0, -0.0001, 0.125]);
        assert_eq!(s, "0.6, 2, 0, 0.125");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
