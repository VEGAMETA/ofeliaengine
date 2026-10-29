//! The Slang module a graph makes (ADR 0123): its parameters' struct and a type
//! implementing `IMaterial`, every written line with the node that made it.
//!
//! - **Each function of `IMaterial`** (`surface`, `occlusion`, `emission`, `alpha`,
//!   `alpha_cutoff`, `thickness`) writes the nodes its output terms reach, in the order
//!   they are needed; nodes nothing reaches are not written.
//! - **A node's output read once is written in place,** one read more often once into a
//!   variable; a texture's sample is always a variable.
//! - **Types follow Slang's rules:** a scalar fills a wider port, a wider vector loses its
//!   last components, a narrower one gains zeros (and 1 in `w`); a node's dynamic ports
//!   take the narrowest vector linked to them (a scalar where only scalars are).
//! - **Numbers** ([`Build`]): in the saved build an unnamed number is a literal; in the
//!   editing build every number is a parameter (`_<node>`), so a drag compiles nothing (the
//!   team's 3a, measured in ADR 0123). A named number is a parameter in both.
//! - **Node previews** (the editing build, E10-4g): `node_preview(ctx, p, node)` writes
//!   every node shown on the canvas in the order they are read, each followed by a return
//!   of its value when it is the one asked for ([`Generated::previews`] gives the order).
//!   One function for every node: an edit of the graph's code compiles once, however many
//!   nodes it shows.

use crate::call::{Direction, node_kind};
use crate::graph::{Graph, OUTPUT, PortRef, SURFACE_TERMS};
use crate::library::{Body, Fill, Kind, OUTPUT_KIND, PortType, REROUTE_KIND};
use engine_scene::AssetRef;
use std::collections::{BTreeMap, HashMap};
use std::fmt::{self, Write as _};

/// Which build of a graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// What the game ships: unnamed numbers as literals.
    Saved,
    /// What the editor draws while the graph is open: every number a parameter.
    Editing,
}

/// What a parameter holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    /// A texture (`TextureRef`).
    Texture,
    /// `float` to `float4`, by its width.
    Float(u8),
}

/// A parameter of the generated type: a field of its `Params`.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    /// The field's name: the node's parameter name, or `_<node>`.
    pub name: String,
    /// Its node's id.
    pub node: String,
    /// What it holds.
    pub ty: ParamType,
    /// A number's value as its node holds it.
    pub value: [f32; 4],
}

/// A generated module.
#[derive(Debug, Clone, PartialEq)]
pub struct Generated {
    /// The module's text.
    pub text: String,
    /// The type's name (its parameters' struct is `<type>Params`).
    pub type_name: String,
    /// The parameters, textures first, then by name.
    pub params: Vec<Param>,
    /// Each line's node, where Slang's errors go: a node of the graph given (a call's
    /// for what its function's nodes wrote, E10-3g-2).
    pub lines: Vec<Option<String>>,
    /// The graph written: the one given, its function calls made their functions' nodes
    /// (`<call>__<node>`, which the parameters' `node` may name).
    pub expanded: Graph,
    /// The editing build's node previews (E10-4g), in the order `node_preview` numbers
    /// them; none in the saved build.
    pub previews: Vec<NodePreview>,
}

/// A node's preview on the canvas (E10-4g): its value, drawn on a sphere when it reads the
/// surface's place, normal or tangent (or code that may), else over a flat square whose
/// texture coordinates run from 0 to 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePreview {
    /// The node.
    pub node: String,
    /// Drawn on a sphere.
    pub sphere: bool,
}

/// The kinds whose values differ over a sphere, not over a square of texture coordinates.
const ROUND: &[&str] = &[
    "world_position",
    "normal",
    "tangent",
    "view_direction",
    "normal_map",
    "fresnel",
];

impl Generated {
    /// The node that made line `line` (0-based).
    pub fn node_at(&self, line: usize) -> Option<&str> {
        self.lines.get(line)?.as_deref()
    }
}

/// What stops a graph from generating, at its node when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphError {
    /// The node.
    pub node: Option<String>,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl GraphError {
    pub(crate) fn at(node: &str, message: impl Into<String>) -> Self {
        Self {
            node: Some(node.to_owned()),
            message: message.into(),
        }
    }

    fn graph(message: impl Into<String>) -> Self {
        Self {
            node: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.node {
            Some(node) => write!(f, "node {node}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for GraphError {}

/// Words a parameter cannot be named (Slang's, and the generated code's own).
const RESERVED: &[&str] = &[
    "bool", "break", "case", "const", "continue", "ctx", "default", "do", "else", "false", "float",
    "for", "if", "in", "inout", "int", "let", "out", "p", "return", "s", "static", "struct",
    "switch", "this", "true", "uint", "var", "void", "while",
];

/// `IMaterial`'s functions after `surface` and `load`: their names, return types, the
/// output's term each returns, and whether it reads the fragment.
const RETURNS: [(&str, &str, &str, bool); 5] = [
    ("occlusion", "float", "occlusion", true),
    ("emission", "float3", "emission", true),
    ("alpha", "float", "alpha", true),
    ("alpha_cutoff", "float", "alpha_cutoff", false),
    ("thickness", "float", "thickness", true),
];

/// Where a graph's calls find what they call.
pub struct Sources<'a> {
    /// The Slang module a `[Node]` call names (E10-3g-3), as its include spells it from the
    /// generated module's folder.
    pub modules: &'a dyn Fn(AssetRef) -> Option<String>,
    /// The function a function call names (E10-3g-2).
    pub functions: &'a dyn Fn(AssetRef) -> Option<Graph>,
}

impl Sources<'static> {
    /// Nothing found: a graph with calls cannot be written.
    pub fn none() -> Self {
        Self {
            modules: &|_| None,
            functions: &|_| None,
        }
    }
}

/// Whose nodes the editing build previews (E10-4g).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shown<'a> {
    /// The graph's own nodes, its function calls among them.
    #[default]
    Graph,
    /// The nodes of the function that the call of this id calls, by their ids in the
    /// function: a function's tab, whose preview is a graph calling it (E10-4g-3).
    Function(&'a str),
}

/// The module `graph` makes, its type named `type_name`. A graph with calls (E10-3g-2,
/// E10-3g-3) needs [`generate_with`]: here what they call is not found.
pub fn generate(graph: &Graph, type_name: &str, build: Build) -> Result<Generated, GraphError> {
    generate_with(graph, type_name, build, &Sources::none())
}

/// [`generate`], what calls name found in `sources`: a function call made its function's
/// nodes first ([`crate::function::expand`]), so what they write, and what is wrong with
/// them, is the call's.
pub fn generate_with(
    graph: &Graph,
    type_name: &str,
    build: Build,
    sources: &Sources<'_>,
) -> Result<Generated, GraphError> {
    generate_shown(graph, type_name, build, sources, Shown::Graph)
}

/// [`generate_with`], the editing build's previews of `shown`'s nodes (E10-4g-3).
///
/// # Errors
///
/// As [`generate_with`].
pub fn generate_shown(
    graph: &Graph,
    type_name: &str,
    build: Build,
    sources: &Sources<'_>,
    shown: Shown<'_>,
) -> Result<Generated, GraphError> {
    engine_profiler::zone!("material_graph::generate");
    let expansion = crate::function::expand(graph, sources.functions)?;
    let points = if build == Build::Editing {
        preview_points(graph, &expansion, shown, sources.functions)
    } else {
        Vec::new()
    };
    let crate::function::Expansion {
        graph: expanded,
        origin,
        ..
    } = expansion;
    match write_module(&expanded, type_name, build, sources.modules, &points) {
        Ok(mut generated) => {
            for line in generated.lines.iter_mut().flatten() {
                if let Some(top) = origin.get(line.as_str()) {
                    top.clone_into(line);
                }
            }
            generated.expanded = expanded;
            Ok(generated)
        }
        Err(mut e) => {
            if let Some(node) = &e.node
                && let Some(top) = origin.get(node)
                && top != node
            {
                let function = graph.nodes.get(top).map_or("", |n| n.function.as_str());
                e.message = format!("in {function}: {}", e.message);
                e.node = Some(top.clone());
            }
            Err(e)
        }
    }
}

/// A node the editing build previews (E10-4g): its id on the canvas, the port of the graph
/// written whose value it shows, and the width it gives that value at (a call's or a
/// function's output's; `None`, the port's own).
struct Point {
    id: String,
    port: PortRef,
    width: Option<u8>,
}

/// The nodes of `shown` the editing build previews, each with the port of `expansion`'s
/// graph whose value it shows: every node with an output but a reroute and the surface, a
/// function call by its first output's; in a function's tab its inputs (the values the
/// call gives) and outputs (what they are given) too.
fn preview_points(
    graph: &Graph,
    expansion: &crate::function::Expansion,
    shown: Shown<'_>,
    functions: &dyn Fn(AssetRef) -> Option<Graph>,
) -> Vec<Point> {
    let width_of = |ty: PortType| match ty {
        PortType::Float(w) => Some(w),
        PortType::Dynamic => None,
    };
    let given = |call: &str, output: &str| {
        expansion
            .outputs
            .get(call)
            .and_then(|o| o.get(output))
            .cloned()
    };
    let mut points = Vec::new();
    match shown {
        Shown::Graph => {
            for (id, node) in &graph.nodes {
                let Ok(kind) = node_kind(node) else {
                    continue;
                };
                let Some(first) = kind.outputs.first() else {
                    continue;
                };
                if kind.name == REROUTE_KIND || kind.name == OUTPUT_KIND {
                    continue;
                }
                let point = if matches!(kind.body, Body::FunctionCall(_)) {
                    given(id, first.name).map(|port| (port, width_of(first.ty)))
                } else {
                    Some((PortRef::new(id, first.name), None))
                };
                if let Some((port, width)) = point {
                    points.push(Point {
                        id: id.clone(),
                        port,
                        width,
                    });
                }
            }
        }
        Shown::Function(call) => {
            let function = graph
                .nodes
                .get(call)
                .filter(|n| n.kind == crate::function::FUNCTION_KIND)
                .and_then(|n| functions(n.module));
            let Some(function) = function else {
                return points;
            };
            let inner = |id: &str| format!("{call}__{id}");
            for (id, node) in &function.nodes {
                let Ok(kind) = node_kind(node) else {
                    continue;
                };
                if kind.name == REROUTE_KIND {
                    continue;
                }
                let point = match kind.body {
                    Body::FunctionInput(_) => Some((PortRef::new(&inner(id), "value"), None)),
                    Body::FunctionOutput(w) => given(call, &node.param).map(|p| (p, Some(w))),
                    Body::FunctionCall(_) => kind
                        .outputs
                        .first()
                        .and_then(|o| given(&inner(id), o.name).map(|p| (p, width_of(o.ty)))),
                    _ => kind
                        .outputs
                        .first()
                        .map(|o| (PortRef::new(&inner(id), o.name), None)),
                };
                if let Some((port, width)) = point {
                    points.push(Point {
                        id: id.clone(),
                        port,
                        width,
                    });
                }
            }
        }
    }
    points
}

/// The module of `graph` (its calls of functions made their nodes), its `[Node]` calls'
/// modules from `modules`; the editing build's previews of `points`.
fn write_module(
    graph: &Graph,
    type_name: &str,
    build: Build,
    modules: &dyn Fn(AssetRef) -> Option<String>,
    points: &[Point],
) -> Result<Generated, GraphError> {
    if !identifier(type_name) {
        return Err(GraphError::graph(format!(
            "`{type_name}` cannot name a Slang type"
        )));
    }
    let kinds = check(graph)?;
    let output = kinds[OUTPUT];
    let model = graph.model;
    // The nodes each function's terms reach, in the order they are needed.
    let surface_terms: Vec<&str> = SURFACE_TERMS.iter().chain(model.terms()).copied().collect();
    let surface_nodes = reach(graph, &kinds, &surface_terms)?;
    let mut returns = Vec::new();
    for (name, ty, term, context) in RETURNS {
        let nodes = reach(graph, &kinds, &[term])?;
        if !context && let Some(n) = nodes.iter().find(|n| kinds[*n].context) {
            return Err(GraphError::at(
                n,
                format!(
                    "the {term} takes numbers and parameters only: this node reads the surface"
                ),
            ));
        }
        returns.push((name, ty, term, context, nodes));
    }
    // The widths of every node written.
    let mut every: Vec<&str> = surface_nodes.clone();
    for (.., nodes) in &returns {
        for n in nodes {
            if !every.contains(n) {
                every.push(n);
            }
        }
    }
    // The editing build's previews: every node the canvas shows, each after what it reads;
    // the images of those nothing reaches yet parameters too, so a dropped image shows.
    let previewed: Vec<&str> = if build == Build::Editing {
        let mut state = HashMap::new();
        let mut order = Vec::new();
        let mut ids: Vec<&str> = graph.nodes.keys().map(String::as_str).collect();
        ids.sort_unstable();
        let mut whole = true;
        for id in ids.into_iter().filter(|&n| n != OUTPUT) {
            // A loop among nodes nothing reaches: no previews, the rest as it was.
            if visit(graph, &kinds, id, &mut state, &mut order).is_err() {
                whole = false;
                break;
            }
        }
        if whole { order } else { Vec::new() }
    } else {
        Vec::new()
    };
    let mut written = every.clone();
    for &n in &previewed {
        if kinds[n].body == Body::Texture && !written.contains(&n) {
            written.push(n);
        }
    }
    let widths = widths(
        graph,
        &kinds,
        if previewed.is_empty() {
            &every
        } else {
            &previewed
        },
    );
    let params = params(graph, &kinds, &written, build)?;
    let param_of: HashMap<&str, &str> = params
        .iter()
        .map(|p| (p.node.as_str(), p.name.as_str()))
        .collect();

    let mut out = Writer::default();
    out.line(
        "// Generated from a material graph (ADR 0123): edits here are lost; edit the graph.",
        None,
    );
    // The editing build's node previews (E10-4g): `forward.slang`'s `nodePreviewFragment`
    // calls `node_preview` where the module says it has one, so the type's every entry
    // point compiles in one request. A compile reads one type's module; the guard keeps
    // several editing builds read together quiet.
    if !previewed.is_empty() {
        out.line("#ifndef MATERIAL_NODE_PREVIEWS", None);
        out.line("#define MATERIAL_NODE_PREVIEWS 1", None);
        out.line("#endif", None);
    }
    // The modules its calls' functions are in (E10-3g-3), after the mark they use; each
    // line its first call's, so a module not found is that node's.
    let mut included: Vec<AssetRef> = Vec::new();
    for &n in &every {
        if !matches!(kinds[n].body, Body::Call(_)) {
            continue;
        }
        let module = graph.nodes[n].module;
        if included.contains(&module) {
            continue;
        }
        if included.is_empty() {
            out.line("#include \"nodes.slang\"", None);
        }
        let path = (!module.is_none())
            .then(|| modules(module))
            .flatten()
            .ok_or_else(|| GraphError::at(n, "the module of its function is not found"))?;
        out.line(
            &format!("#include \"{}\"", path.replace('\\', "/")),
            Some(n),
        );
        included.push(module);
    }
    out.line(&format!("struct {type_name}Params"), None);
    out.line("{", None);
    if params.is_empty() {
        out.line("    float _unused;", None);
    }
    for p in &params {
        let ty = match p.ty {
            ParamType::Texture => String::from("TextureRef"),
            ParamType::Float(w) => float_type(w),
        };
        out.line(&format!("    {ty} {};", p.name), Some(&p.node));
    }
    out.line("};", None);
    out.line("", None);
    out.line(&format!("struct {type_name} : IMaterial"), None);
    out.line("{", None);
    out.line(&format!("    typealias Params = {type_name}Params;"), None);
    out.line("", None);
    // The normal map's function, a member of the type so modules of several graphs can
    // be read together.
    if every.iter().any(|n| kinds[*n].name == "normal_map") {
        for l in NORMAL_MAP.lines() {
            out.line(&format!("    {l}"), None);
        }
        out.line("", None);
    }
    // Each Expression node's Slang (E10-3g-1), a function of the type whose lines are the
    // node's: Slang's errors in it go to the node.
    for &n in every.iter().filter(|n| kinds[*n].body == Body::Expression) {
        let code = graph.nodes[n].code.trim();
        let code = if code.is_empty() { "a" } else { code };
        out.line(
            &format!(
                "    static float4 expression_{n}(MaterialContext ctx, float4 a, float4 b, float4 c, float4 d)"
            ),
            Some(n),
        );
        out.line("    {", Some(n));
        if code.contains("return") {
            for l in code.lines() {
                out.line(&format!("        {l}"), Some(n));
            }
        } else {
            let one: Vec<&str> = code.lines().map(str::trim).collect();
            out.line(&format!("        return {};", one.join(" ")), Some(n));
        }
        out.line("    }", Some(n));
        out.line("", None);
    }
    out.line(
        &format!("    static uint model() {{ return {}; }}", model.constant()),
        None,
    );
    out.line("", None);
    out.line(
        &format!("    static {type_name}Params load(MaterialContext ctx)"),
        None,
    );
    out.line("    {", None);
    out.line(
        &format!("        return material_params<{type_name}Params>(ctx);"),
        None,
    );
    out.line("    }", None);
    out.line("", None);
    let context = Context {
        graph,
        kinds: &kinds,
        widths: &widths,
        param_of: &param_of,
    };
    // The surface.
    out.line(
        &format!(
            "    static Surface surface(MaterialContext ctx, {type_name}Params p, bool masked)"
        ),
        None,
    );
    out.line("    {", None);
    let values = context.write(&mut out, &surface_nodes, &surface_terms)?;
    let term = |name: &str| context.input(output, OUTPUT, name, &values);
    out.line("        Surface s;", Some(OUTPUT));
    out.line(
        &format!("        s.base = {};", term("base")?),
        Some(OUTPUT),
    );
    out.line(
        &format!("        s.normal = {};", term("normal")?),
        Some(OUTPUT),
    );
    out.line(
        &format!("        s.metallic = {};", term("metallic")?),
        Some(OUTPUT),
    );
    out.line(
        &format!("        s.roughness = {};", term("roughness")?),
        Some(OUTPUT),
    );
    out.line("        s.subsurface = 0;", Some(OUTPUT));
    out.line("        s = surface_terms(s);", Some(OUTPUT));
    for &t in model.terms() {
        out.line(&format!("        s.{t} = {};", term(t)?), Some(OUTPUT));
    }
    out.line("        return s;", Some(OUTPUT));
    out.line("    }", None);
    // The other functions, each a value of the output's.
    for (name, ty, term, reads, nodes) in &returns {
        out.line("", None);
        let args = if *reads {
            format!("MaterialContext ctx, {type_name}Params p")
        } else {
            format!("{type_name}Params p")
        };
        out.line(&format!("    static {ty} {name}({args})"), None);
        out.line("    {", None);
        let values = context.write(&mut out, nodes, &[term])?;
        let value = context.input(output, OUTPUT, term, &values)?;
        out.line(&format!("        return {value};"), Some(OUTPUT));
        out.line("    }", None);
    }
    let previews = if previewed.is_empty() {
        Vec::new()
    } else {
        write_previews(&mut out, &context, &previewed, type_name, points)?
    };
    out.line("}", None);
    Ok(Generated {
        text: out.text,
        type_name: type_name.to_owned(),
        params,
        lines: out.lines,
        expanded: Graph::new(graph.model),
        previews,
    })
}

/// The editing build's `node_preview` (E10-4g): the nodes of `order` written in it, each
/// followed by a return of the value a point shows when it is the one asked for, as a
/// colour (a scalar grey, a pair red and green). Which points it numbers (in the order
/// their values are written), and how each is drawn.
fn write_previews(
    out: &mut Writer,
    context: &Context<'_, '_>,
    order: &[&str],
    type_name: &str,
    points: &[Point],
) -> Result<Vec<NodePreview>, GraphError> {
    let kinds = context.kinds;
    // Round where it reads the surface's place, normal or tangent, or code that may.
    let mut round: HashMap<&str, bool> = HashMap::new();
    for &n in order {
        let kind = kinds[n];
        let own = ROUND.contains(&kind.name)
            || kind.body == Body::Expression
            || matches!(kind.body, Body::Call(call) if call.context);
        let read = kind.inputs.iter().any(|input| {
            context
                .graph
                .links
                .get(&PortRef::new(n, input.name))
                .is_some_and(|from| round.get(from.node.as_str()).copied().unwrap_or(false))
        });
        round.insert(n, own || read);
    }
    let read: Vec<PortRef> = points.iter().map(|p| p.port.clone()).collect();
    let mut numbered: Vec<NodePreview> = Vec::new();
    out.line("", None);
    out.line(
        &format!(
            "    static float4 node_preview(MaterialContext ctx, {type_name}Params p, uint node)"
        ),
        None,
    );
    out.line("    {", None);
    context.write_with(out, order, &[], &read, &mut |out, id, values| {
        for point in points.iter().filter(|p| p.port.node == id) {
            let value = values
                .get(&point.port)
                .ok_or_else(|| GraphError::at(id, "its value is not written"))?;
            let from = output_width(kinds, context.widths, id, &point.port.port);
            let width = point.width.unwrap_or(from);
            let k = numbered.len();
            out.line(
                &format!(
                    "        if (node == {k}) return {};",
                    convert(&convert(value, from, width), width, 4)
                ),
                Some(&point.id),
            );
            numbered.push(NodePreview {
                node: point.id.clone(),
                sphere: round.get(id).copied().unwrap_or(false),
            });
        }
        Ok(())
    })?;
    out.line("        return float4(0.0, 0.0, 0.0, 0.0);", None);
    out.line("    }", None);
    Ok(numbered)
}

/// `graph`'s nodes' kinds once its nodes, links and values are checked.
fn check(graph: &Graph) -> Result<HashMap<&str, &'static Kind>, GraphError> {
    let mut kinds = HashMap::new();
    for (id, node) in &graph.nodes {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(GraphError::at(id, "a node's id is letters, digits and `_`"));
        }
        let kind = node_kind(node).map_err(|why| GraphError::at(id, why))?;
        if matches!(kind.body, Body::FunctionInput(_) | Body::FunctionOutput(_)) {
            return Err(GraphError::at(
                id,
                "a function's input or output: it belongs in a function's file",
            ));
        }
        if kind.name == OUTPUT_KIND && id != OUTPUT {
            return Err(GraphError::at(id, "a graph has one surface"));
        }
        let finite = |v: &[f32; 4]| v.iter().all(|x| x.is_finite());
        if !finite(&node.value) || !node.inputs.values().all(finite) {
            return Err(GraphError::at(id, "a value is not a number"));
        }
        if let Some(name) = node.inputs.keys().find(|n| kind.input(n).is_none()) {
            return Err(GraphError::at(
                id,
                format!("{} has no input `{name}`", kind.title),
            ));
        }
        kinds.insert(id.as_str(), kind);
    }
    match graph.nodes.get(OUTPUT) {
        Some(node) if node.kind == OUTPUT_KIND => {}
        _ => return Err(GraphError::graph("the graph has no surface")),
    }
    for (to, from) in &graph.links {
        let into = kinds.get(to.node.as_str()).ok_or_else(|| {
            GraphError::graph(format!("a link goes into {to}, which is not there"))
        })?;
        if into.input(&to.port).is_none() {
            return Err(GraphError::at(
                &to.node,
                format!("{} has no input `{}`", into.title, to.port),
            ));
        }
        let source = kinds.get(from.node.as_str()).ok_or_else(|| {
            GraphError::at(
                &to.node,
                format!("its {} comes from {from}, which is not there", to.port),
            )
        })?;
        if source.output(&from.port).is_none() {
            return Err(GraphError::at(
                &from.node,
                format!("{} has no output `{}`", source.title, from.port),
            ));
        }
    }
    Ok(kinds)
}

/// The nodes the output's `terms` reach, each after the nodes it reads.
fn reach<'g>(
    graph: &'g Graph,
    kinds: &HashMap<&'g str, &'static Kind>,
    terms: &[&str],
) -> Result<Vec<&'g str>, GraphError> {
    let mut order = Vec::new();
    let mut state = HashMap::new();
    for term in terms {
        if let Some(from) = graph.links.get(&PortRef::new(OUTPUT, term)) {
            visit(graph, kinds, &from.node, &mut state, &mut order)?;
        }
    }
    Ok(order)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    Open,
    Done,
}

fn visit<'g>(
    graph: &'g Graph,
    kinds: &HashMap<&'g str, &'static Kind>,
    node: &'g str,
    state: &mut HashMap<&'g str, Visit>,
    order: &mut Vec<&'g str>,
) -> Result<(), GraphError> {
    match state.get(node) {
        Some(Visit::Done) => return Ok(()),
        Some(Visit::Open) => return Err(GraphError::at(node, "a loop runs through this node")),
        None => {}
    }
    if node == OUTPUT {
        return Err(GraphError::at(node, "the surface cannot feed itself"));
    }
    state.insert(node, Visit::Open);
    for input in kinds[node].inputs {
        if let Some((_, from)) = graph.links.get_key_value(&PortRef::new(node, input.name)) {
            visit(graph, kinds, &from.node, state, order)?;
        }
    }
    state.insert(node, Visit::Done);
    order.push(node);
    Ok(())
}

/// Each node's dynamic width: the narrowest vector linked to its dynamic inputs, 1 where
/// only scalars are (or nothing is). `order` has every node after those it reads.
fn widths<'g>(
    graph: &'g Graph,
    kinds: &HashMap<&'g str, &'static Kind>,
    order: &[&'g str],
) -> HashMap<&'g str, u8> {
    let mut widths: HashMap<&str, u8> = HashMap::new();
    for &node in order {
        let mut width: Option<u8> = None;
        for input in kinds[node].inputs {
            if input.ty != PortType::Dynamic {
                continue;
            }
            if let Some(from) = graph.links.get(&PortRef::new(node, input.name)) {
                let w = output_width(kinds, &widths, &from.node, &from.port);
                if w > 1 {
                    width = Some(width.map_or(w, |v| v.min(w)));
                }
            }
        }
        widths.insert(node, width.unwrap_or(1));
    }
    widths
}

/// The width of output `port` of `node`.
fn output_width(
    kinds: &HashMap<&str, &'static Kind>,
    widths: &HashMap<&str, u8>,
    node: &str,
    port: &str,
) -> u8 {
    match kinds[node].output(port).map(|o| o.ty) {
        Some(PortType::Float(w)) => w,
        Some(PortType::Dynamic) | None => widths.get(node).copied().unwrap_or(1),
    }
}

/// The parameters of the nodes written: textures always, numbers named (and every number
/// in the editing build).
fn params(
    graph: &Graph,
    kinds: &HashMap<&str, &'static Kind>,
    written: &[&str],
    build: Build,
) -> Result<Vec<Param>, GraphError> {
    let mut by_name: BTreeMap<(bool, String), Param> = BTreeMap::new();
    let mut ids: Vec<&str> = written.to_vec();
    ids.sort_unstable();
    for id in ids {
        let node = &graph.nodes[id];
        let ty = match kinds[id].body {
            Body::Texture => ParamType::Texture,
            Body::Constant(w) if !node.param.is_empty() || build == Build::Editing => {
                ParamType::Float(w)
            }
            _ => continue,
        };
        let name = if node.param.is_empty() {
            format!("_{id}")
        } else {
            let p = &node.param;
            if !identifier(p) || p.starts_with('_') || RESERVED.contains(&p.as_str()) {
                return Err(GraphError::at(
                    id,
                    format!(
                        "`{p}` cannot name a parameter: a letter first, then letters, digits and `_`"
                    ),
                ));
            }
            p.clone()
        };
        let key = (ty != ParamType::Texture, name.clone());
        if let Some(taken) = by_name
            .get(&key)
            .or_else(|| by_name.get(&(!key.0, name.clone())))
        {
            // Copies of a function's named parameter, one a call: the one parameter, as
            // Unreal's functions' parameters are (E10-3g-2).
            if id.contains("__") && taken.node.contains("__") && taken.ty == ty {
                continue;
            }
            return Err(GraphError::at(
                id,
                format!("the parameter `{name}` is node {}'s already", taken.node),
            ));
        }
        by_name.insert(
            key,
            Param {
                name,
                node: id.to_owned(),
                ty,
                value: node.value,
            },
        );
    }
    Ok(by_name.into_values().collect())
}

/// What [`Context::write_with`] gives each node once written: the writer, the node, and
/// the values written so far.
type AfterNode<'f> =
    dyn FnMut(&mut Writer, &str, &HashMap<PortRef, String>) -> Result<(), GraphError> + 'f;

/// What writing a function's nodes needs.
struct Context<'a, 'g> {
    graph: &'g Graph,
    kinds: &'a HashMap<&'g str, &'static Kind>,
    widths: &'a HashMap<&'g str, u8>,
    param_of: &'a HashMap<&'a str, &'a str>,
}

impl Context<'_, '_> {
    /// `nodes` written for the output's `terms`: each output's value (a variable, or its
    /// expression when read once).
    fn write(
        &self,
        out: &mut Writer,
        nodes: &[&str],
        terms: &[&str],
    ) -> Result<HashMap<PortRef, String>, GraphError> {
        self.write_with(out, nodes, terms, &[], &mut |_, _, _| Ok(()))
    }

    /// [`Self::write`], the outputs `also` read once more each (written once into a
    /// variable when anything else reads them too), and `after` given each node as soon as
    /// it is written.
    fn write_with(
        &self,
        out: &mut Writer,
        nodes: &[&str],
        terms: &[&str],
        also: &[PortRef],
        after: &mut AfterNode<'_>,
    ) -> Result<HashMap<PortRef, String>, GraphError> {
        // How often each output is read here.
        let mut reads: HashMap<PortRef, usize> = HashMap::new();
        for port in also {
            *reads.entry(port.clone()).or_default() += 1;
        }
        let mut count = |to: PortRef| {
            if let Some(from) = self.graph.links.get(&to) {
                *reads.entry(from.clone()).or_default() += 1;
            }
        };
        for &n in nodes {
            for input in self.kinds[n].inputs {
                count(PortRef::new(n, input.name));
            }
        }
        for t in terms {
            count(PortRef::new(OUTPUT, t));
        }
        let mut values: HashMap<PortRef, String> = HashMap::new();
        for &id in nodes {
            let kind = self.kinds[id];
            let node = &self.graph.nodes[id];
            let width = self.widths[id];
            match kind.body {
                Body::Constant(w) => {
                    let value = match self.param_of.get(id) {
                        Some(name) => format!("p.{name}"),
                        None => literal(node.value, w),
                    };
                    values.insert(PortRef::new(id, kind.outputs[0].name), value);
                }
                Body::Texture => {
                    let uv = self.input(kind, id, "uv", &values)?;
                    let name = self.param_of.get(id).copied().unwrap_or_default();
                    let var = format!("n_{id}");
                    out.line(
                        &format!("        float4 {var} = sample_material(p.{name}.packed, {uv});"),
                        Some(id),
                    );
                    for o in kind.outputs {
                        let value = match o.name {
                            "rgba" => var.clone(),
                            channels => format!("{var}.{channels}"),
                        };
                        values.insert(PortRef::new(id, o.name), value);
                    }
                }
                Body::Expr(exprs) | Body::Math { vector: exprs, .. } => {
                    let exprs = match kind.body {
                        Body::Math { scalar, .. } if width == 1 => scalar,
                        _ => exprs,
                    };
                    for (o, template) in kind.outputs.iter().zip(exprs) {
                        let mut expr = (*template).to_owned();
                        for input in kind.inputs {
                            let mark = format!("{{{}}}", input.name);
                            if expr.contains(&mark) {
                                let value = self.input(kind, id, input.name, &values)?;
                                expr = expr.replace(&mark, &value);
                            }
                        }
                        let port = PortRef::new(id, o.name);
                        if reads.get(&port).copied().unwrap_or(0) > 1 {
                            let var = format!("n_{id}_{}", o.name);
                            let ty = float_type(port_width(o.ty, width));
                            out.line(&format!("        {ty} {var} = {expr};"), Some(id));
                            values.insert(port, var);
                        } else {
                            values.insert(port, expr);
                        }
                    }
                }
                Body::Expression => {
                    let mut call = format!("expression_{id}(ctx");
                    for input in kind.inputs {
                        call.push_str(", ");
                        call.push_str(&self.input(kind, id, input.name, &values)?);
                    }
                    call.push(')');
                    let port = PortRef::new(id, "out");
                    if reads.get(&port).copied().unwrap_or(0) > 1 {
                        let var = format!("n_{id}_out");
                        out.line(&format!("        float4 {var} = {call};"), Some(id));
                        values.insert(port, var);
                    } else {
                        values.insert(port, call);
                    }
                }
                Body::Call(call) => {
                    // Its `out` and `inout` parameters variables, its result one.
                    let mut arguments: Vec<String> = Vec::new();
                    if call.context {
                        arguments.push(String::from("ctx"));
                    }
                    for p in call.parameters {
                        let var = format!("n_{id}_{}", p.name);
                        let ty = float_type(p.width);
                        match p.direction {
                            Direction::In => {
                                arguments.push(self.input(kind, id, p.name, &values)?);
                                continue;
                            }
                            Direction::Out => out.line(&format!("        {ty} {var};"), Some(id)),
                            Direction::InOut => {
                                let value = self.input(kind, id, p.name, &values)?;
                                out.line(&format!("        {ty} {var} = {value};"), Some(id));
                            }
                        }
                        arguments.push(var.clone());
                        values.insert(PortRef::new(id, p.name), var);
                    }
                    let text = format!("{}({})", call.function, arguments.join(", "));
                    match call.result {
                        Some(w) => {
                            let var = format!("n_{id}_out");
                            let ty = float_type(w);
                            out.line(&format!("        {ty} {var} = {text};"), Some(id));
                            values.insert(PortRef::new(id, "out"), var);
                        }
                        None => out.line(&format!("        {text};"), Some(id)),
                    }
                }
                Body::FunctionCall(_) | Body::FunctionInput(_) | Body::FunctionOutput(_) => {
                    return Err(GraphError::at(id, "a call not made its function's nodes"));
                }
                Body::Output => {}
            }
            after(out, id, &values)?;
        }
        Ok(values)
    }

    /// Input `name` of node `id` (of `kind`): what is linked to it, converted to its type,
    /// or its own value, or its kind's.
    fn input(
        &self,
        kind: &Kind,
        id: &str,
        name: &str,
        values: &HashMap<PortRef, String>,
    ) -> Result<String, GraphError> {
        let input = kind
            .input(name)
            .ok_or_else(|| GraphError::at(id, format!("{} has no input `{name}`", kind.title)))?;
        let width = port_width(input.ty, self.widths.get(id).copied().unwrap_or(1));
        if let Some(from) = self.graph.links.get(&PortRef::new(id, name)) {
            let value = values.get(from).ok_or_else(|| {
                GraphError::at(
                    id,
                    format!("its {name} comes from {from}, which is not written"),
                )
            })?;
            let from_width = output_width(self.kinds, self.widths, &from.node, &from.port);
            return Ok(convert(value, from_width, width));
        }
        let own = self.graph.nodes.get(id).and_then(|n| n.inputs.get(name));
        Ok(match (own, input.fill) {
            (Some(v), _) => literal(*v, width),
            (None, Fill::Value(v)) => literal(v, width),
            (None, Fill::Expr(e)) => e.to_owned(),
        })
    }
}

/// A port's width on a node of dynamic width `dynamic`.
fn port_width(ty: PortType, dynamic: u8) -> u8 {
    match ty {
        PortType::Float(w) => w,
        PortType::Dynamic => dynamic,
    }
}

/// `value` of width `from` as width `to`, by Slang's rules.
fn convert(value: &str, from: u8, to: u8) -> String {
    use core::cmp::Ordering;
    match from.cmp(&to) {
        Ordering::Equal => value.to_owned(),
        Ordering::Greater => format!("({value}).{}", &"xyzw"[..usize::from(to)]),
        Ordering::Less => match (from, to) {
            (1, n) => format!("{}({value})", float_type(n)),
            (2, 3) => format!("float3({value}, 0.0)"),
            (2, 4) => format!("float4({value}, 0.0, 1.0)"),
            _ => format!("float4({value}, 1.0)"),
        },
    }
}

/// Slang's float type of width `w`.
pub(crate) fn float_type(w: u8) -> String {
    match w {
        1 => String::from("float"),
        n => format!("float{n}"),
    }
}

/// A literal of `v`'s first `w` components.
fn literal(v: [f32; 4], w: u8) -> String {
    let number = |x: f32| {
        if x.fract() == 0.0 && x.abs() < 1e7 {
            format!("{x:.1}")
        } else {
            format!("{x:?}")
        }
    };
    match w {
        1 => number(v[0]),
        n => {
            let parts: Vec<String> = v[..usize::from(n)].iter().map(|&x| number(x)).collect();
            format!("{}({})", float_type(n), parts.join(", "))
        }
    }
}

/// `name` can name a Slang type or field.
pub(crate) fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The normal map node's function (written once, where a graph uses one): a tangent-space
/// map's colour as a world normal, `strength` of it.
const NORMAL_MAP: &str = r"// A tangent-space normal map's color as a world normal, `strength` of it.
static float3 graph_normal_map(MaterialContext ctx, float4 color, float strength)
{
    float3 geometric = normalize(ctx.normal * ctx.side);
    float3 t = ctx.tangent.xyz * ctx.side;
    float3 b = ctx.tangent.w * cross(ctx.normal, ctx.tangent.xyz) * ctx.side;
    float2 xy = (color.xy * 2.0 - 1.0) * strength;
    float z = sqrt(saturate(1.0 - dot(xy, xy)));
    return normalize(xy.x * t + xy.y * b + z * geometric);
}";

/// The module's text, each line with its node.
#[derive(Default)]
struct Writer {
    text: String,
    lines: Vec<Option<String>>,
}

impl Writer {
    fn line(&mut self, text: &str, node: Option<&str>) {
        let _ = writeln!(self.text, "{text}");
        self.lines.push(node.map(str::to_owned));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::graph::{Model, Node};

    fn port(node: &str, port: &str) -> PortRef {
        PortRef::new(node, port)
    }

    /// E10-3g-3: a call's `inout` parameter a variable of its input's value, its `out` one
    /// declared, both its outputs; a `void` call a statement; the module included once
    /// after the mark's declaration, its line the first call's; a call naming no module, or
    /// one not found, is its node's error.
    #[test]
    fn a_call_writes_its_out_and_inout_parameters_as_variables() {
        let module = AssetRef::file(engine_scene::AssetId::from_u128(3));
        let signature = "void(inout float2 uv, out float mask)";
        let mut g = Graph::new(Model::Lit);
        for (id, at) in [("w", 0.0), ("v", 200.0)] {
            let mut call =
                Node::new(crate::call::CALL_KIND, [at, 0.0]).with_call("warp", signature);
            call.module = module;
            g.add(id, call);
        }
        g.link(port("w", "uv"), port("v", "uv"))
            .link(port("v", "mask"), port(OUTPUT, "roughness"));
        let modules = |m: AssetRef| (m == module).then(|| String::from("../shaders/warp.slang"));
        let sources = Sources {
            modules: &modules,
            functions: &|_| None,
        };
        let out = generate_with(&g, "Warp", Build::Saved, &sources).unwrap();
        let lines: Vec<&str> = out.text.lines().collect();
        let include = lines
            .iter()
            .position(|l| *l == "#include \"../shaders/warp.slang\"")
            .unwrap();
        assert_eq!(lines[include - 1], "#include \"nodes.slang\"");
        assert_eq!(
            out.text
                .matches("#include \"../shaders/warp.slang\"")
                .count(),
            1
        );
        assert_eq!(out.node_at(include), Some("w"));
        for line in [
            "        float2 n_w_uv = float2(0.0, 0.0);",
            "        float n_w_mask;",
            "        warp(n_w_uv, n_w_mask);",
            "        float2 n_v_uv = n_w_uv;",
            "        warp(n_v_uv, n_v_mask);",
        ] {
            assert!(lines.contains(&line), "{line}\n{}", out.text);
        }
        assert!(out.text.contains("s.roughness = n_v_mask"), "{}", out.text);
        let missing = generate_with(&g, "Warp", Build::Saved, &Sources::none()).unwrap_err();
        assert_eq!(missing.node.as_deref(), Some("w"));
        g.nodes.get_mut("w").unwrap().module = AssetRef::NONE;
        let none = generate_with(&g, "Warp", Build::Saved, &sources).unwrap_err();
        assert_eq!(none.node.as_deref(), Some("w"));
    }

    /// E10-3g-1: an Expression node's Slang is a function of the type, its lines the node's;
    /// an expression is returned, a body that returns kept as written.
    #[test]
    fn an_expression_is_a_function_of_the_type_its_lines_the_nodes() {
        let mut g = Graph::new(Model::Lit);
        g.add(
            "c",
            Node::new("color", [0.0; 2]).with_value([0.9, 0.1, 0.1, 1.0]),
        );
        g.add(
            "e",
            Node::new("expression", [100.0, 0.0]).with_code("a * 0.5 + b"),
        );
        g.link(port("c", "rgba"), port("e", "a"))
            .link(port("e", "out"), port(OUTPUT, "base"));
        let out = generate(&g, "Glow", Build::Saved).unwrap();
        let header = "    static float4 expression_e(MaterialContext ctx, float4 a, float4 b, float4 c, float4 d)";
        assert!(out.text.contains(header), "{}", out.text);
        let line = out
            .text
            .lines()
            .position(|l| l == "        return a * 0.5 + b;")
            .unwrap();
        assert_eq!(out.node_at(line), Some("e"));
        assert!(
            out.text.contains("s.base = expression_e(ctx, "),
            "{}",
            out.text
        );
        // A body that returns, as written.
        g.nodes.get_mut("e").unwrap().code = String::from("float4 x = a;\nreturn x * 2.0;");
        let out = generate(&g, "Glow", Build::Saved).unwrap();
        assert!(
            out.text
                .contains("        float4 x = a;\n        return x * 2.0;"),
            "{}",
            out.text
        );
        // Read twice in one function: one call, kept in a variable.
        g.link(port("e", "out"), port(OUTPUT, "metallic"));
        let out = generate(&g, "Glow", Build::Saved).unwrap();
        assert_eq!(
            out.text.matches("expression_e(ctx, ").count(),
            1,
            "{}",
            out.text
        );
    }

    /// A reroute (E10-3e-2a) writes nothing of its own: a graph through one generates the
    /// module it does without, in both builds.
    #[test]
    fn a_reroute_passes_its_input_on_and_writes_no_code() {
        let mut direct = Graph::new(Model::Lit);
        direct.add(
            "c",
            Node::new("color", [0.0; 2]).with_value([0.9, 0.1, 0.1, 1.0]),
        );
        direct.link(port("c", "rgba"), port(OUTPUT, "base"));
        let mut through = direct.clone();
        through.links.clear();
        through.add("r1", Node::new("reroute", [100.0, 0.0]));
        through.add("r2", Node::new("reroute", [200.0, 0.0]));
        through
            .link(port("c", "rgba"), port("r1", "in"))
            .link(port("r1", "out"), port("r2", "in"))
            .link(port("r2", "out"), port(OUTPUT, "base"));
        for build in [Build::Saved, Build::Editing] {
            let a = generate(&direct, "Glow", build).unwrap();
            let b = generate(&through, "Glow", build).unwrap();
            assert_eq!(a.text, b.text, "{build:?}");
        }
    }

    /// A textured, numbered lit graph: an albedo texture into the base, a roughness.
    fn lit() -> Graph {
        let mut g = Graph::new(Model::Lit);
        g.add("t", Node::new("texture", [0.0; 2]).named("albedo"))
            .add(
                "r",
                Node::new("number", [0.0; 2])
                    .with_value([0.6, 0.0, 0.0, 0.0])
                    .named("roughness"),
            )
            .link(port("t", "rgba"), port(OUTPUT, "base"))
            .link(port("r", "value"), port(OUTPUT, "roughness"));
        g
    }

    #[test]
    fn a_graph_writes_its_parameters_and_its_surface_from_the_outputs_terms() {
        let out = generate(&lit(), "Moss", Build::Saved).unwrap();
        let names: Vec<&str> = out.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["albedo", "roughness"], "textures first");
        assert_eq!(out.params[0].ty, ParamType::Texture);
        assert_eq!(out.params[1].ty, ParamType::Float(1));
        let text = &out.text;
        for line in [
            "struct MossParams",
            "    TextureRef albedo;",
            "    float roughness;",
            "struct Moss : IMaterial",
            "    static uint model() { return MODEL_LIT; }",
            "        float4 n_t = sample_material(p.albedo.packed, ctx.uv);",
            "        s.base = n_t;",
            "        s.normal = normalize(ctx.normal * ctx.side);",
            "        s.metallic = 0.0;",
            "        s.roughness = p.roughness;",
            "    static float alpha_cutoff(MossParams p)",
            "        return 0.5;",
        ] {
            assert!(text.lines().any(|l| l == line), "{line}\n{text}");
        }
        // The sample is written in the surface only: the other functions do not read it.
        assert_eq!(text.matches("sample_material").count(), 1, "{text}");
        // Each line knows its node.
        let at = text
            .lines()
            .position(|l| l.contains("sample_material("))
            .unwrap();
        assert_eq!(out.node_at(at), Some("t"));
        let base = text.lines().position(|l| l.contains("s.base")).unwrap();
        assert_eq!(out.node_at(base), Some(OUTPUT));
        assert_eq!(out.node_at(0), None);
    }

    #[test]
    fn numbers_are_literals_when_saved_and_parameters_while_editing() {
        let mut g = Graph::new(Model::Lit);
        g.add(
            "n",
            Node::new("number", [0.0; 2]).with_value([0.3, 0.0, 0.0, 0.0]),
        )
        .add(
            "c",
            Node::new("color", [0.0; 2]).with_value([1.0, 0.5, 0.25, 1.0]),
        )
        .link(port("n", "value"), port(OUTPUT, "roughness"))
        .link(port("c", "rgba"), port(OUTPUT, "base"));
        let saved = generate(&g, "G", Build::Saved).unwrap();
        assert!(saved.params.is_empty());
        assert!(
            saved.text.contains("    float _unused;"),
            "an empty struct holds a field"
        );
        assert!(
            saved.text.contains("        s.roughness = 0.3;"),
            "{}",
            saved.text
        );
        assert!(
            saved
                .text
                .contains("        s.base = float4(1.0, 0.5, 0.25, 1.0);")
        );
        let editing = generate(&g, "G", Build::Editing).unwrap();
        let names: Vec<&str> = editing.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["_c", "_n"]);
        assert_eq!(editing.params[1].value[0], 0.3);
        assert!(
            editing.text.contains("        s.roughness = p._n;"),
            "{}",
            editing.text
        );
        assert!(editing.text.contains("    float4 _c;"));
    }

    #[test]
    fn an_output_read_twice_is_a_variable_and_once_is_written_in_place() {
        let mut g = Graph::new(Model::Lit);
        g.add(
            "a",
            Node::new("add", [0.0; 2])
                .with_input("a", [0.25; 4])
                .with_input("b", [0.25; 4]),
        )
        .link(port("a", "out"), port(OUTPUT, "metallic"))
        .link(port("a", "out"), port(OUTPUT, "roughness"));
        let out = generate(&g, "G", Build::Saved).unwrap();
        assert!(
            out.text.contains("        float n_a_out = (0.25 + 0.25);"),
            "{}",
            out.text
        );
        assert!(out.text.contains("        s.metallic = n_a_out;"));
        assert!(out.text.contains("        s.roughness = n_a_out;"));
        // Once: in place.
        g.links.remove(&port(OUTPUT, "roughness"));
        let out = generate(&g, "G", Build::Saved).unwrap();
        assert!(
            out.text.contains("        s.metallic = (0.25 + 0.25);"),
            "{}",
            out.text
        );
    }

    #[test]
    fn ports_convert_by_slangs_rules_and_dynamic_ports_take_the_narrowest_vector() {
        let mut g = Graph::new(Model::Lit);
        g.add("one", Node::new("number", [0.0; 2]).with_value([0.5; 4]))
            .add("two", Node::new("vector2", [0.0; 2]).with_value([0.1, 0.2, 0.0, 0.0]))
            .add("four", Node::new("vector4", [0.0; 2]).with_value([1.0; 4]))
            // A scalar fills the base.
            .link(port("one", "value"), port(OUTPUT, "base"))
            // A float2 gains a zero.
            .link(port("two", "value"), port(OUTPUT, "emission"))
            // A float4 loses its w.
            .add("m", Node::new("multiply", [0.0; 2]))
            .link(port("four", "value"), port("m", "a"))
            .link(port("two", "value"), port("m", "b"))
            .link(port("m", "out"), port(OUTPUT, "normal"));
        let out = generate(&g, "G", Build::Saved).unwrap();
        let text = &out.text;
        assert!(text.contains("        s.base = float4(0.5);"), "{text}");
        assert!(
            text.contains("        return float3(float2(0.1, 0.2), 0.0);"),
            "{text}"
        );
        // The multiply's width is the narrowest vector linked (2), its float4 input cut.
        assert!(
            text.contains("        s.normal = float3(((float4(1.0, 1.0, 1.0, 1.0)).xy * float2(0.1, 0.2)), 0.0);"),
            "{text}"
        );
        // A dot of scalars is a product.
        let mut d = Graph::new(Model::Lit);
        d.add(
            "d",
            Node::new("dot", [0.0; 2])
                .with_input("a", [0.5; 4])
                .with_input("b", [0.5; 4]),
        )
        .link(port("d", "out"), port(OUTPUT, "roughness"));
        let out = generate(&d, "G", Build::Saved).unwrap();
        assert!(
            out.text.contains("        s.roughness = (0.5 * 0.5);"),
            "{}",
            out.text
        );
    }

    #[test]
    fn a_models_own_terms_are_written_after_the_common_ones() {
        let mut g = Graph::new(Model::Toon);
        g.add(
            "n",
            Node::new("number", [0.0; 2]).with_value([3.0, 0.0, 0.0, 0.0]),
        )
        .link(port("n", "value"), port(OUTPUT, "toon_steps"));
        let out = generate(&g, "G", Build::Saved).unwrap();
        let text = &out.text;
        assert!(text.contains("return MODEL_TOON;"));
        let terms = text
            .lines()
            .position(|l| l.contains("s = surface_terms(s);"))
            .unwrap();
        let steps = text
            .lines()
            .position(|l| l == "        s.toon_steps = 3.0;")
            .unwrap();
        assert!(steps > terms, "{text}");
        assert!(text.contains("        s.rim_width = 0.3;"));
        assert!(!text.contains("s.clearcoat"), "a toon has no coat");
    }

    #[test]
    fn what_a_graph_cannot_be_is_an_error_at_its_node() {
        let err = |g: &Graph| generate(g, "G", Build::Saved).unwrap_err();
        // A loop.
        let mut g = Graph::new(Model::Lit);
        g.add("a", Node::new("add", [0.0; 2]))
            .add("b", Node::new("add", [0.0; 2]))
            .link(port("a", "out"), port("b", "a"))
            .link(port("b", "out"), port("a", "a"))
            .link(port("b", "out"), port(OUTPUT, "roughness"));
        let e = err(&g);
        assert!(e.message.contains("loop") && e.node.is_some(), "{e}");
        // A kind, a port, a link from nowhere.
        let mut g = Graph::new(Model::Lit);
        g.add("x", Node::new("wobble", [0.0; 2]));
        assert_eq!(err(&g).node.as_deref(), Some("x"));
        let mut g = Graph::new(Model::Lit);
        g.add("n", Node::new("number", [0.0; 2]))
            .link(port("n", "nothing"), port(OUTPUT, "roughness"));
        assert_eq!(err(&g).node.as_deref(), Some("n"));
        let mut g = Graph::new(Model::Lit);
        g.link(port("gone", "value"), port(OUTPUT, "roughness"));
        assert_eq!(err(&g).node.as_deref(), Some(OUTPUT));
        // Parameters: one name twice, a name Slang cannot take.
        let mut g = Graph::new(Model::Lit);
        g.add("a", Node::new("number", [0.0; 2]).named("shine"))
            .add("b", Node::new("number", [0.0; 2]).named("shine"))
            .link(port("a", "value"), port(OUTPUT, "roughness"))
            .link(port("b", "value"), port(OUTPUT, "metallic"));
        let e = err(&g);
        assert!(
            e.message.contains("shine") && e.node.as_deref() == Some("b"),
            "{e}"
        );
        for bad in ["2x", "out", "_hidden", "a b"] {
            let mut g = Graph::new(Model::Lit);
            g.add("a", Node::new("number", [0.0; 2]).named(bad))
                .link(port("a", "value"), port(OUTPUT, "roughness"));
            assert_eq!(err(&g).node.as_deref(), Some("a"), "{bad}");
        }
        // Not a number.
        let mut g = Graph::new(Model::Lit);
        g.add("a", Node::new("number", [0.0; 2]).with_value([f32::NAN; 4]));
        assert_eq!(err(&g).node.as_deref(), Some("a"));
        // A second surface; none.
        let mut g = Graph::new(Model::Lit);
        g.add("again", Node::new(OUTPUT_KIND, [0.0; 2]));
        assert_eq!(err(&g).node.as_deref(), Some("again"));
        let mut g = Graph::new(Model::Lit);
        g.nodes.remove(OUTPUT);
        assert_eq!(err(&g).node, None);
        // A type name.
        assert!(generate(&lit(), "a b", Build::Saved).is_err());
    }

    #[test]
    fn the_alpha_cutoff_takes_numbers_only() {
        let mut g = lit();
        g.link(port("t", "a"), port(OUTPUT, "alpha_cutoff"));
        let e = generate(&g, "G", Build::Saved).unwrap_err();
        assert_eq!(e.node.as_deref(), Some("t"), "{e}");
        // A number, named, is fine: a parameter of the function.
        let mut g = lit();
        g.add(
            "cut",
            Node::new("number", [0.0; 2])
                .with_value([0.3; 4])
                .named("cutoff"),
        )
        .link(port("cut", "value"), port(OUTPUT, "alpha_cutoff"));
        let out = generate(&g, "G", Build::Saved).unwrap();
        assert!(
            out.text.contains("        return p.cutoff;"),
            "{}",
            out.text
        );
    }

    #[test]
    fn a_normal_map_writes_its_function_once() {
        let mut g = lit();
        g.add("nt", Node::new("texture", [0.0; 2]).named("bumps"))
            .add(
                "nm",
                Node::new("normal_map", [0.0; 2]).with_input("strength", [0.5; 4]),
            )
            .link(port("nt", "rgba"), port("nm", "color"))
            .link(port("nm", "normal"), port(OUTPUT, "normal"));
        let out = generate(&g, "G", Build::Saved).unwrap();
        assert_eq!(
            out.text.matches("static float3 graph_normal_map(").count(),
            1
        );
        assert!(
            out.text
                .contains("        s.normal = graph_normal_map(ctx, n_nt, 0.5);"),
            "{}",
            out.text
        );
    }

    /// E10-4g: the editing build previews every node the canvas shows (not the surface,
    /// not a reroute), each after what it reads, numbered as `node_preview` returns them;
    /// round where it reads the surface's normal, flat otherwise. An image nothing reaches
    /// is a parameter there, so it shows; the saved build has none of it.
    #[test]
    fn the_editing_build_previews_every_shown_node() {
        let mut g = Graph::new(Model::Lit);
        g.add("a", Node::new("uv", [0.0; 2]))
            .add("b", Node::new("normal", [0.0; 2]))
            .add("c", Node::new("add", [0.0; 2]))
            .add("d", Node::new("number", [0.0; 2]).with_value([0.5; 4]))
            .add("r", Node::new("reroute", [0.0; 2]))
            .add("t", Node::new("texture", [0.0; 2]))
            .link(port("a", "uv"), port("c", "a"))
            .link(port("d", "value"), port("c", "b"))
            .link(port("c", "out"), port("r", "in"))
            .link(port("r", "out"), port(OUTPUT, "roughness"));
        let editing = generate(&g, "Previewed", Build::Editing).unwrap();
        let shown: Vec<(&str, bool)> = editing
            .previews
            .iter()
            .map(|p| (p.node.as_str(), p.sphere))
            .collect();
        assert_eq!(
            shown,
            [
                ("a", false),
                ("b", true),
                ("d", false),
                ("c", false),
                ("t", false)
            ],
            "each after what it reads; the normal round"
        );
        let text = &editing.text;
        assert!(
            text.contains(
                "static float4 node_preview(MaterialContext ctx, PreviewedParams p, uint node)"
            ),
            "{text}"
        );
        for k in 0..shown.len() {
            assert!(
                text.contains(&format!("if (node == {k}) return ")),
                "{text}"
            );
        }
        // The image nothing reaches yet: a parameter, sampled for its preview.
        assert!(
            editing.params.iter().any(|p| p.node == "t"),
            "{:?}",
            editing.params
        );
        assert!(text.contains("#define MATERIAL_NODE_PREVIEWS 1"), "{text}");
        let saved = generate(&g, "Previewed", Build::Saved).unwrap();
        assert!(saved.previews.is_empty());
        assert!(!saved.text.contains("node_preview"));
        assert!(!saved.text.contains("MATERIAL_NODE_PREVIEWS"));
        assert!(!saved.params.iter().any(|p| p.node == "t"));
        // A loop among nodes nothing reaches: no previews, the material as it was.
        g.add("x", Node::new("add", [0.0; 2]))
            .add("y", Node::new("add", [0.0; 2]))
            .link(port("x", "out"), port("y", "a"))
            .link(port("y", "out"), port("x", "a"));
        let looped = generate(&g, "Previewed", Build::Editing).unwrap();
        assert!(looped.previews.is_empty());
        assert!(!looped.text.contains("node_preview"));
    }

    /// E10-4g-3: a function call is previewed by what its first output gives, once that is
    /// written; a function's tab (its preview's graph a call of it) previews the function's
    /// own nodes by their ids: an input by the value the call gives it, an output by what it
    /// is given, at the output's width.
    #[test]
    fn calls_and_a_functions_nodes_are_previewed() {
        use crate::function::{FUNCTION_KIND, input_kind, output_kind, signature_of};
        use engine_scene::{AssetId, AssetRef};
        let mut tint = Graph::new(Model::Lit);
        tint.nodes.clear();
        tint.add(
            "c",
            Node::new(input_kind(3), [0.0, 0.0])
                .named("color")
                .with_value([1.0, 0.0, 0.0, 0.0]),
        )
        .add("h", Node::new("number", [0.0, 100.0]).with_value([0.5; 4]))
        .add("m", Node::new("multiply", [150.0, 0.0]))
        .add("o", Node::new(output_kind(3), [300.0, 0.0]).named("tinted"))
        .link(port("c", "value"), port("m", "a"))
        .link(port("h", "value"), port("m", "b"))
        .link(port("m", "out"), port("o", "value"));
        let file = AssetRef::file(AssetId::from_u128(9));
        let signature = signature_of(&tint).unwrap().to_string();
        let call = || {
            let mut n = Node::new(FUNCTION_KIND, [200.0, 0.0]).with_call("tint", &signature);
            n.module = file;
            n
        };
        let functions = |r: AssetRef| (r == file).then(|| tint.clone());
        let sources = Sources {
            modules: &|_| None,
            functions: &functions,
        };
        let shown =
            |g: &Generated| -> Vec<String> { g.previews.iter().map(|p| p.node.clone()).collect() };
        // On a graph: the colour, then the call once its multiply is written.
        let mut g = Graph::new(Model::Lit);
        g.add(
            "col",
            Node::new("color", [0.0; 2]).with_value([0.8, 0.6, 0.4, 1.0]),
        )
        .add("t", call())
        .link(port("col", "rgba"), port("t", "color"))
        .link(port("t", "tinted"), port(OUTPUT, "base"));
        let editing = generate_with(&g, "Tinted", Build::Editing, &sources).unwrap();
        assert_eq!(shown(&editing), ["col", "t"]);
        // The call's value: the colour times the function's number, at the call's float3.
        assert!(
            editing
                .text
                .contains("if (node == 1) return float4(((p._col * float4(p._t__h))).xyz, 1.0);"),
            "{}",
            editing.text
        );
        // A function's tab: its input, its number, its multiply and its output.
        let mut wrapper = Graph::new(Model::Unlit);
        wrapper
            .add("function", call())
            .link(port("function", "tinted"), port(OUTPUT, "base"));
        let tab = generate_shown(
            &wrapper,
            "Tinting",
            Build::Editing,
            &sources,
            Shown::Function("function"),
        )
        .unwrap();
        assert_eq!(shown(&tab), ["c", "h", "m", "o"]);
        // The output at its own width (a float3 made a colour), the input the call's value.
        assert!(
            tab.text.contains("if (node == 3) return float4("),
            "{}",
            tab.text
        );
        assert!(
            tab.lines.iter().flatten().any(|n| n == "o"),
            "the output's line is its own: {:?}",
            tab.lines
        );
        // Neither build previews in a saved build, nor a call the tab does not hold.
        let saved = generate_shown(
            &wrapper,
            "Tinting",
            Build::Saved,
            &sources,
            Shown::Function("function"),
        )
        .unwrap();
        assert!(saved.previews.is_empty());
        let none = generate_shown(
            &wrapper,
            "Tinting",
            Build::Editing,
            &sources,
            Shown::Function("x"),
        )
        .unwrap();
        assert!(none.previews.is_empty());
    }
}
