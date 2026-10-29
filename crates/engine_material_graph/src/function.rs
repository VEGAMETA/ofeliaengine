//! Functions (ADR 0123's E10-3g-2): a graph of nodes in a file of its own (`.mfunc`), called
//! from other graphs as one node, as Unreal's material functions and Blender's node groups
//! are.
//!
//! A function's ports are its input and output nodes (`input_float` to `input_float4`,
//! `output_float` to `output_float4`), each named by its `param`, in their order on its
//! canvas, top to bottom. A call keeps the function's file (`Node::module`), its name and
//! its ports as a signature (`void(float3 color, out float3 tinted)`), as a `[Node]` call
//! keeps its function's, so it is drawn and linked without the file.
//!
//! Before a graph is written its calls are made their functions' nodes ([`expand`]), as
//! Unreal inlines material functions: each of a function's nodes is `<call>__<node>` in the
//! graph, a call's input linked where its function's input was read (its value, unlinked,
//! a constant), its output read where its function's output was given. What is written for
//! those nodes, and what is wrong with them, is the call's.

use crate::call::{Direction, Parameter, Signature};
use crate::generate::GraphError;
use crate::graph::{Graph, Node, PortRef};
use crate::library::{self, Body};
use engine_scene::{AssetId, AssetRef};
use std::collections::BTreeMap;

/// A call of a function file: its `module` the file, `function` its name, `signature` its
/// ports.
pub const FUNCTION_KIND: &str = "function";

/// The group functions are found under in the node search.
pub const FUNCTION_CATEGORY: &str = "Functions";

/// The kind of a call of the function named `function` whose ports are `signature`
/// ([`signature_of`]'s text): [`crate::call_kind`]'s, for a function file's call.
///
/// # Errors
///
/// The signature is not one a call can have.
pub fn function_call_kind(
    function: &str,
    signature: &str,
) -> Result<&'static library::Kind, String> {
    crate::call::made_kind(FUNCTION_KIND, function, signature)
}

/// The kind of a function's input of width `w` (1 to 4).
pub fn input_kind(w: u8) -> &'static str {
    [
        "input_float",
        "input_float2",
        "input_float3",
        "input_float4",
    ][usize::from(w.clamp(1, 4)) - 1]
}

/// The kind of a function's output of width `w` (1 to 4).
pub fn output_kind(w: u8) -> &'static str {
    [
        "output_float",
        "output_float2",
        "output_float3",
        "output_float4",
    ][usize::from(w.clamp(1, 4)) - 1]
}

/// What a node is to a function: an input or an output of its width, or neither.
fn port_of(node: &Node) -> Option<(bool, u8)> {
    match library::kind(&node.kind)?.body {
        Body::FunctionInput(w) => Some((true, w)),
        Body::FunctionOutput(w) => Some((false, w)),
        _ => None,
    }
}

/// `function`'s input and output nodes in their order on its canvas (top to bottom, then
/// left to right): id, whether an input, width.
fn ports(function: &Graph) -> Vec<(&str, bool, u8)> {
    let mut ports: Vec<(&str, &Node, bool, u8)> = function
        .nodes
        .iter()
        .filter_map(|(id, n)| port_of(n).map(|(input, w)| (id.as_str(), n, input, w)))
        .collect();
    ports.sort_by(|a, b| {
        (a.1.at[1], a.1.at[0])
            .partial_cmp(&(b.1.at[1], b.1.at[0]))
            .unwrap_or(core::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    ports
        .into_iter()
        .map(|(id, _, input, w)| (id, input, w))
        .collect()
}

/// `function`'s ports as a call keeps them: its inputs, then its outputs, each in its order
/// on the canvas, every output an `out` parameter.
///
/// # Errors
///
/// A port without a name, with one a port cannot have, or with another's; no output.
pub fn signature_of(function: &Graph) -> Result<Signature, String> {
    let mut parameters = Vec::new();
    let all = ports(function);
    for wanted in [true, false] {
        for &(id, input, width) in all.iter().filter(|p| p.1 == wanted) {
            let name = &function.nodes[id].param;
            if name.is_empty() {
                let what = if input { "input" } else { "output" };
                return Err(format!("the {what} `{id}` has no name"));
            }
            parameters.push(Parameter {
                name: name.clone(),
                width,
                direction: if input { Direction::In } else { Direction::Out },
            });
        }
    }
    let signature = Signature {
        result: None,
        context: false,
        parameters,
    };
    // What a call's ports cannot be (two of a name, `out`, no output).
    Signature::parse(&signature.to_string())
}

/// Where each call's outputs come from once calls are made their functions' nodes
/// (E10-4g-3): by the call's id in the graph made (a call inside a function by its id
/// there, `tint__inner`), each output's name and the port that gives it.
pub type CallOutputs = BTreeMap<String, BTreeMap<String, PortRef>>;

/// A graph with its function calls made their functions' nodes ([`expand`]).
#[derive(Debug, Clone)]
pub struct Expansion {
    /// The graph made.
    pub graph: Graph,
    /// For every node of it, the node of the graph it was made from that it came from
    /// (itself, or the call it was made for).
    pub origin: BTreeMap<String, String>,
    /// Where every call's outputs come from in it, the calls inside its functions too.
    pub outputs: CallOutputs,
}

/// `graph` with its function calls made their functions' nodes (see the module), the
/// functions read by `functions` (`None`: not found); for every node of it, the node of
/// `graph` it came from; and where every call's outputs come from.
///
/// # Errors
///
/// A call whose function is not found, calls itself (or a function that calls it), or
/// whose ports are not the function's; what is wrong inside a function, said at its call.
pub fn expand(
    graph: &Graph,
    functions: &dyn Fn(AssetRef) -> Option<Graph>,
) -> Result<Expansion, GraphError> {
    let mut stack = Vec::new();
    expand_in(graph, functions, &mut stack)
}

fn expand_in(
    graph: &Graph,
    functions: &dyn Fn(AssetRef) -> Option<Graph>,
    stack: &mut Vec<AssetId>,
) -> Result<Expansion, GraphError> {
    let mut out = graph.clone();
    let mut origin: BTreeMap<String, String> =
        graph.nodes.keys().map(|k| (k.clone(), k.clone())).collect();
    let mut outputs = CallOutputs::new();
    for (call_id, call) in graph.nodes.iter().filter(|(_, n)| n.kind == FUNCTION_KIND) {
        let at = |message: String| GraphError::at(call_id, message);
        let file = call.module.file;
        if call.module.is_none() {
            return Err(at(String::from("the call names no function")));
        }
        if stack.contains(&file) {
            return Err(at(format!("{} calls itself", call.function)));
        }
        let function =
            functions(call.module).ok_or_else(|| at(format!("{} is not found", call.function)))?;
        stack.push(file);
        let expanded = expand_in(&function, functions, stack);
        stack.pop();
        let inner = expanded.map_err(|e| at(format!("in {}: {}", call.function, e.message)))?;
        let prefix = |id: &str| format!("{call_id}__{id}");
        let made =
            inline_into(&mut out, call_id, &inner.graph, &prefix, &inner.outputs).map_err(at)?;
        for id in made.added {
            origin.insert(id, call_id.clone());
        }
        outputs.insert(call_id.clone(), made.gives);
        outputs.extend(made.nested);
        origin.remove(call_id);
    }
    resolve(&out, &mut outputs);
    Ok(Expansion {
        graph: out,
        origin,
        outputs,
    })
}

/// `outputs`' ports that name a call made since (gone from `graph`: a call fed by one
/// after it), each made the port that call's output comes from, through as many calls as
/// it takes.
fn resolve(graph: &Graph, outputs: &mut CallOutputs) {
    for _ in 0..=outputs.len() {
        let before = outputs.clone();
        let mut changed = false;
        for port in outputs.values_mut().flat_map(BTreeMap::values_mut) {
            if graph.nodes.contains_key(&port.node) {
                continue;
            }
            if let Some(from) = before.get(&port.node).and_then(|o| o.get(&port.port)) {
                from.clone_into(port);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// `graph` with its call `call_id` made the nodes of `function`, the function it calls (its
/// own calls kept calls), as Blender's Ungroup takes a group apart (E10-3g-2b): each node
/// under the id `id_for` gives its own id, which must be free in `graph`; the ids made.
///
/// # Errors
///
/// No node `call_id`, or a link of it to a port the function has not.
pub fn inline(
    graph: &Graph,
    call_id: &str,
    function: &Graph,
    id_for: &dyn Fn(&str) -> String,
) -> Result<(Graph, Vec<String>), String> {
    let mut out = graph.clone();
    let made = inline_into(&mut out, call_id, function, id_for, &CallOutputs::new())?;
    Ok((out, made.added))
}

/// What a call made its function's nodes made: the ids added, the port each of its outputs
/// comes from, and the function's own calls' outputs (`nested`) under their ids here.
struct Inlined {
    added: Vec<String>,
    gives: BTreeMap<String, PortRef>,
    nested: CallOutputs,
}

/// Call `call_id` of `out` made `function`'s nodes in `out` (see the module), each under
/// the id `id_for` gives its own; `nested`, where `function`'s own calls' outputs come from
/// in it.
fn inline_into(
    out: &mut Graph,
    call_id: &str,
    function: &Graph,
    id_for: &dyn Fn(&str) -> String,
    nested: &CallOutputs,
) -> Result<Inlined, String> {
    let call = out
        .nodes
        .get(call_id)
        .cloned()
        .ok_or_else(|| format!("no node `{call_id}`"))?;
    let mut added = Vec::new();
    // What each of the function's inputs reads: the call's link into it, else a constant
    // of the call's own value for it (or the input's).
    let mut given: BTreeMap<PortRef, PortRef> = BTreeMap::new();
    let mut names: Vec<(&str, bool)> = Vec::new();
    for (id, node) in &function.nodes {
        let Some((input, width)) = port_of(node) else {
            added.push(id_for(id));
            out.nodes.insert(id_for(id), node.clone());
            continue;
        };
        names.push((node.param.as_str(), input));
        if !input {
            continue;
        }
        // From the graph as made so far: a call before it may feed it.
        let source = match out.links.get(&PortRef::new(call_id, &node.param)) {
            Some(from) => from.clone(),
            None => {
                let value = call.inputs.get(&node.param).copied().unwrap_or(node.value);
                added.push(id_for(id));
                out.nodes
                    .insert(id_for(id), constant(width, value, node.at));
                PortRef::new(&id_for(id), "value")
            }
        };
        given.insert(PortRef::new(id, "value"), source);
    }
    let through = |from: &PortRef| {
        given
            .get(from)
            .cloned()
            .unwrap_or_else(|| PortRef::new(&id_for(&from.node), &from.port))
    };
    // The function's links, but into its outputs.
    let mut gives: BTreeMap<String, PortRef> = BTreeMap::new();
    for (to, from) in &function.links {
        match function.nodes.get(&to.node).and_then(port_of) {
            Some((false, _)) => {
                gives.insert(function.nodes[&to.node].param.clone(), through(from));
            }
            _ => {
                out.links
                    .insert(PortRef::new(&id_for(&to.node), &to.port), through(from));
            }
        }
    }
    // An output given nothing gives its own value.
    for (id, node) in &function.nodes {
        if let Some((false, width)) = port_of(node)
            && !gives.contains_key(&node.param)
        {
            let value = node.inputs.get("value").copied().unwrap_or([0.0; 4]);
            added.push(id_for(id));
            out.nodes
                .insert(id_for(id), constant(width, value, node.at));
            gives.insert(node.param.clone(), PortRef::new(&id_for(id), "value"));
        }
    }
    // The call's links: into it, gone (read inside now) once each is the function's input;
    // out of it, from what the function gives.
    if let Some(name) = out
        .links
        .keys()
        .filter(|to| to.node == call_id)
        .find(|to| !names.contains(&(to.port.as_str(), true)))
    {
        return Err(format!("{} has no input `{}`", call.function, name.port));
    }
    out.links.retain(|to, _| to.node != call_id);
    for (to, from) in &mut out.links {
        if from.node != call_id {
            continue;
        }
        *from = gives.get(&from.port).cloned().ok_or_else(|| {
            format!(
                "{} has no output `{}` (read by {to})",
                call.function, from.port
            )
        })?;
    }
    out.nodes.remove(call_id);
    let nested = nested
        .iter()
        .map(|(id, ports)| {
            let ports = ports
                .iter()
                .map(|(name, from)| (name.clone(), through(from)))
                .collect();
            (id_for(id), ports)
        })
        .collect();
    Ok(Inlined {
        added,
        gives,
        nested,
    })
}

/// A constant node of width `width` holding `value`, at `at`.
fn constant(width: u8, value: [f32; 4], at: [f32; 2]) -> Node {
    let kind = ["number", "vector2", "vector3", "vector4"][usize::from(width.clamp(1, 4)) - 1];
    Node::new(kind, at).with_value(value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::graph::{Model, OUTPUT};

    fn port(node: &str, port: &str) -> PortRef {
        PortRef::new(node, port)
    }

    /// A function giving its colour input darkened by its `amount`, and the amount.
    fn darken() -> Graph {
        let mut f = Graph::new(Model::Lit);
        f.nodes.clear();
        f.add(
            "c",
            Node::new(input_kind(3), [0.0, 0.0])
                .named("color")
                .with_value([1.0, 0.0, 0.0, 0.0]),
        )
        .add(
            "a",
            Node::new(input_kind(1), [0.0, 100.0])
                .named("amount")
                .with_value([0.5, 0.0, 0.0, 0.0]),
        )
        .add("m", Node::new("multiply", [150.0, 0.0]))
        .add(
            "o",
            Node::new(output_kind(3), [300.0, 0.0]).named("darkened"),
        )
        .add(
            "k",
            Node::new(output_kind(1), [300.0, 100.0]).named("amount_out"),
        )
        .link(port("c", "value"), port("m", "a"))
        .link(port("a", "value"), port("m", "b"))
        .link(port("m", "out"), port("o", "value"))
        .link(port("a", "value"), port("k", "value"));
        f
    }

    /// A function's ports are its inputs then its outputs, each top to bottom, named by
    /// their nodes; one without a name, or two of a name, is refused.
    #[test]
    fn a_functions_ports_are_its_input_and_output_nodes() {
        let f = darken();
        assert_eq!(
            signature_of(&f).unwrap().to_string(),
            "void(float3 color, float amount, out float3 darkened, out float amount_out)"
        );
        let mut unnamed = f.clone();
        unnamed.nodes.get_mut("a").unwrap().param.clear();
        assert!(
            signature_of(&unnamed)
                .unwrap_err()
                .contains("`a` has no name")
        );
        let mut twice = f.clone();
        "color".clone_into(&mut twice.nodes.get_mut("k").unwrap().param);
        assert!(signature_of(&twice).unwrap_err().contains("twice"));
        let mut none = f;
        none.nodes
            .retain(|_, n| port_of(n).is_none_or(|(input, _)| input));
        assert!(signature_of(&none).unwrap_err().contains("nothing back"));
    }

    /// A call made its function's nodes: each `<call>__<node>`, the call's link into an
    /// input read where the input was, an unlinked input a constant of the call's own value
    /// (or the input's), the outputs read where the call's were; every node made for the
    /// call is the call's. Calls of a function that calls itself, of one not found, and to
    /// a port it has not are refused at the call.
    #[test]
    fn a_call_is_made_its_functions_nodes() {
        let file = AssetRef::file(AssetId::from_u128(5));
        let signature = signature_of(&darken()).unwrap().to_string();
        let mut g = Graph::new(Model::Lit);
        let mut call = Node::new(FUNCTION_KIND, [200.0, 0.0])
            .with_call("darken", &signature)
            .with_input("amount", [0.25; 4]);
        call.module = file;
        g.add(
            "col",
            Node::new("color", [0.0; 2]).with_value([0.8, 0.6, 0.4, 1.0]),
        )
        .add("d", call)
        .link(port("col", "rgba"), port("d", "color"))
        .link(port("d", "darkened"), port(OUTPUT, "base"))
        .link(port("d", "amount_out"), port(OUTPUT, "roughness"));
        let functions = |r: AssetRef| (r == file).then(darken);
        let Expansion {
            graph: out,
            origin,
            outputs,
        } = expand(&g, &functions).unwrap();
        assert!(!out.nodes.contains_key("d"));
        assert_eq!(outputs["d"]["darkened"], port("d__m", "out"));
        assert_eq!(outputs["d"]["amount_out"], port("d__a", "value"));
        assert_eq!(out.nodes["d__m"].kind, "multiply");
        assert_eq!(out.links[&port("d__m", "a")], port("col", "rgba"));
        assert_eq!(out.nodes["d__a"].kind, "number");
        assert_eq!(out.nodes["d__a"].value[0], 0.25, "the call's own value");
        assert_eq!(out.links[&port("d__m", "b")], port("d__a", "value"));
        assert_eq!(out.links[&port(OUTPUT, "base")], port("d__m", "out"));
        assert_eq!(out.links[&port(OUTPUT, "roughness")], port("d__a", "value"));
        assert_eq!(origin["d__m"], "d");
        assert_eq!(origin["col"], "col");
        assert!(!out.nodes.contains_key("d__o") && !out.nodes.contains_key("d__c"));
        // Not found, calls itself, a port it has not.
        let missing = expand(&g, &|_| None).unwrap_err();
        assert_eq!(missing.node.as_deref(), Some("d"));
        let itself = |r: AssetRef| {
            let mut f = darken();
            let mut again = Node::new(FUNCTION_KIND, [0.0, 300.0]).with_call("darken", &signature);
            again.module = r;
            f.add("again", again);
            Some(f)
        };
        let error = expand(&g, &itself).unwrap_err();
        assert_eq!(error.node.as_deref(), Some("d"));
        assert!(error.message.contains("calls itself"), "{}", error.message);
        let mut wrong = g.clone();
        wrong
            .links
            .insert(port(OUTPUT, "metallic"), port("d", "nope"));
        let error = expand(&wrong, &functions).unwrap_err();
        assert!(
            error.message.contains("no output `nope`"),
            "{}",
            error.message
        );
    }

    /// E10-4g-3: where each call's outputs come from in the graph made: a call fed by a call
    /// made after it (`a` by `b`, made in their ids' order) reads through to that call's
    /// nodes, and a function's own call is there under its id in the graph made, its
    /// outputs read through the function's inputs.
    #[test]
    fn each_calls_outputs_are_found_in_the_graph_made() {
        let darken_file = AssetRef::file(AssetId::from_u128(5));
        let outer_file = AssetRef::file(AssetId::from_u128(6));
        let signature = signature_of(&darken()).unwrap().to_string();
        let darken_call = |at: f32| {
            let mut call = Node::new(FUNCTION_KIND, [at, 0.0]).with_call("darken", &signature);
            call.module = darken_file;
            call
        };
        // `a`'s amount is `b`'s amount out, which is `b`'s own amount: a constant of `b`'s.
        let mut g = Graph::new(Model::Lit);
        g.add("col", Node::new("color", [0.0; 2]))
            .add("a", darken_call(400.0))
            .add("b", darken_call(200.0))
            .link(port("col", "rgba"), port("b", "color"))
            .link(port("b", "amount_out"), port("a", "amount"))
            .link(port("b", "darkened"), port("a", "color"))
            .link(port("a", "amount_out"), port(OUTPUT, "roughness"))
            .link(port("a", "darkened"), port(OUTPUT, "base"));
        let functions = |r: AssetRef| {
            if r == darken_file {
                return Some(darken());
            }
            (r == outer_file).then(|| {
                let mut outer = Graph::new(Model::Lit);
                outer.nodes.clear();
                outer
                    .add("i", Node::new(input_kind(3), [0.0, 0.0]).named("base"))
                    .add("d", darken_call(100.0))
                    .add("o", Node::new(output_kind(3), [200.0, 0.0]).named("result"))
                    .link(port("i", "value"), port("d", "color"))
                    .link(port("d", "darkened"), port("o", "value"));
                outer
            })
        };
        let made = expand(&g, &functions).unwrap();
        assert_eq!(made.outputs["a"]["amount_out"], port("b__a", "value"));
        assert_eq!(made.outputs["a"]["darkened"], port("a__m", "out"));
        assert_eq!(made.outputs["b"]["darkened"], port("b__m", "out"));
        assert!(made.graph.nodes.contains_key("b__a"));
        // A function's own call: under its id in the graph made.
        let outer_signature = signature_of(&functions(outer_file).unwrap())
            .unwrap()
            .to_string();
        let mut f = Node::new(FUNCTION_KIND, [0.0, 0.0]).with_call("outer", &outer_signature);
        f.module = outer_file;
        let mut g = Graph::new(Model::Lit);
        g.add("tex", Node::new("vector3", [-200.0, 0.0]))
            .add("f", f)
            .link(port("tex", "value"), port("f", "base"))
            .link(port("f", "result"), port(OUTPUT, "base"));
        let made = expand(&g, &functions).unwrap();
        assert_eq!(made.outputs["f"]["result"], port("f__d__m", "out"));
        assert_eq!(made.outputs["f__d"]["darkened"], port("f__d__m", "out"));
        assert_eq!(made.outputs["f__d"]["amount_out"], port("f__d__a", "value"));
        assert_eq!(
            made.graph.links[&port("f__d__m", "a")],
            port("tex", "value")
        );
    }

    /// E10-3g-2b: a call taken apart one level (Ungroup) is its function's nodes under the
    /// ids asked for, the call's links read through them, a call inside the function kept a
    /// call; a link of the call to a port the function has not is refused.
    #[test]
    fn a_call_taken_apart_is_its_functions_nodes_one_level() {
        let signature = signature_of(&darken()).unwrap().to_string();
        let mut outer = Graph::new(Model::Lit);
        outer.nodes.clear();
        let mut call = Node::new(FUNCTION_KIND, [100.0, 0.0]).with_call("darken", &signature);
        call.module = AssetRef::file(AssetId::from_u128(6));
        outer
            .add("i", Node::new(input_kind(3), [0.0, 0.0]).named("base"))
            .add("d", call)
            .add("o", Node::new(output_kind(3), [200.0, 0.0]).named("result"))
            .link(port("i", "value"), port("d", "color"))
            .link(port("d", "darkened"), port("o", "value"));
        let outer_signature = signature_of(&outer).unwrap().to_string();
        let mut g = Graph::new(Model::Lit);
        let mut call = Node::new(FUNCTION_KIND, [0.0, 0.0]).with_call("outer", &outer_signature);
        call.module = AssetRef::file(AssetId::from_u128(7));
        g.add("tex", Node::new("vector3", [-200.0, 0.0]))
            .add("f", call)
            .link(port("tex", "value"), port("f", "base"))
            .link(port("f", "result"), port(OUTPUT, "base"));
        let (made, ids) = inline(&g, "f", &outer, &|id| format!("{id}_x")).unwrap();
        assert!(!made.nodes.contains_key("f"));
        assert_eq!(ids, ["d_x"]);
        assert_eq!(made.nodes["d_x"].kind, FUNCTION_KIND);
        assert_eq!(made.links[&port("d_x", "color")], port("tex", "value"));
        assert_eq!(made.links[&port(OUTPUT, "base")], port("d_x", "darkened"));
        let mut wrong = g.clone();
        wrong.link(port("tex", "value"), port("f", "nope"));
        let why = inline(&wrong, "f", &outer, &|id| id.to_owned()).unwrap_err();
        assert!(why.contains("no input `nope`"), "{why}");
    }
}
