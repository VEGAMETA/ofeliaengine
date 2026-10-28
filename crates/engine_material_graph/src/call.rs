//! Nodes that call a Slang function of the project's modules (ADR 0123's E10-3g-3): a
//! function marked `[Node("Title", "Category")]` (the engine's `nodes.slang`), the node's
//! ports its signature's.
//!
//! The signature is kept in the graph's file with the call, as Unreal keeps a material
//! function call's inputs and outputs in the material: the cook and the player generate the
//! module without reflecting Slang, and the editor writes the signature again from Slang's
//! reflection when the function changes. A signature that no longer matches the function
//! fails to compile at the call's line.
//!
//! A call's kind is made from its function and signature once per process and kept
//! ([`call_kind`]): built-in kinds are `'static`, and so are these, so every reader of a
//! kind takes both alike. What is kept grows with the different signatures a process sees.

use crate::graph::Node;
use crate::library::{self, Body, Fill, Input, Kind, Output, PortType};
use std::fmt;
use std::sync::{Mutex, PoisonError};

/// A call node's kind: its function and signature are the node's (`Node::function`,
/// `Node::signature`), its module `Node::module`.
pub const CALL_KIND: &str = "call";

/// The group calls are found under when their mark's is not known (the editor shows the
/// mark's own).
pub const CALL_CATEGORY: &str = "Functions";

/// How a parameter passes its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// An input.
    In,
    /// `out`: an output.
    Out,
    /// `inout`: an input and an output of the same name.
    InOut,
}

/// A parameter of a [`Signature`]: `float` to `float4`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Parameter {
    /// Its name: the port's.
    pub name: String,
    /// Its width (1 to 4).
    pub width: u8,
    /// In, out or both.
    pub direction: Direction,
}

/// What a call node's function takes and gives: its file text is Slang's own spelling,
/// `float4(MaterialContext ctx, float3 position, out float mask)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Signature {
    /// What it returns (the output `out`), `None` for `void`.
    pub result: Option<u8>,
    /// Its first parameter is the fragment (`MaterialContext`), not a port.
    pub context: bool,
    /// Its ports' parameters, in order.
    pub parameters: Vec<Parameter>,
}

impl Signature {
    /// The signature `text` spells (see [`Signature`]), checked: identifiers, each once, not
    /// `out` (the result's port); widths 1 to 4; something given back.
    ///
    /// # Errors
    ///
    /// What is wrong with it, in a sentence.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (result, rest) = text
            .split_once('(')
            .ok_or_else(|| format!("`{text}` is not a signature"))?;
        let list = rest
            .trim_end()
            .strip_suffix(')')
            .ok_or_else(|| format!("`{text}` is not a signature"))?;
        let result = match result.trim() {
            "void" => None,
            ty => Some(width(ty).ok_or_else(|| format!("a call cannot return `{ty}`"))?),
        };
        let mut context = false;
        let mut parameters: Vec<Parameter> = Vec::new();
        for (i, part) in list.split(',').map(str::trim).enumerate() {
            if part.is_empty() && list.trim().is_empty() {
                break;
            }
            let words: Vec<&str> = part.split_whitespace().collect();
            let (direction, ty, name) = match words.as_slice() {
                ["out", ty, name] => (Direction::Out, *ty, *name),
                ["inout", ty, name] => (Direction::InOut, *ty, *name),
                ["in", ty, name] | [ty, name] => (Direction::In, *ty, *name),
                _ => return Err(format!("`{part}` is not a parameter")),
            };
            if ty == "MaterialContext" && direction == Direction::In && i == 0 {
                context = true;
                continue;
            }
            let width = width(ty).ok_or_else(|| {
                format!(
                    "`{name}` is a `{ty}`: a node's ports are float to float4 (and a first \
                     MaterialContext)"
                )
            })?;
            if !crate::generate::identifier(name) || name == "out" || name == "ctx" {
                return Err(format!("`{name}` cannot name a port"));
            }
            if parameters.iter().any(|p| p.name == name) {
                return Err(format!("`{name}` is there twice"));
            }
            parameters.push(Parameter {
                name: name.to_owned(),
                width,
                direction,
            });
        }
        if result.is_none() && parameters.iter().all(|p| p.direction == Direction::In) {
            return Err(String::from(
                "it gives nothing back: no result, no out parameter",
            ));
        }
        Ok(Self {
            result,
            context,
            parameters,
        })
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.result {
            Some(w) => f.write_str(&crate::generate::float_type(w))?,
            None => f.write_str("void")?,
        }
        f.write_str("(")?;
        let mut first = true;
        if self.context {
            f.write_str("MaterialContext ctx")?;
            first = false;
        }
        for p in &self.parameters {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            match p.direction {
                Direction::In => {}
                Direction::Out => f.write_str("out ")?,
                Direction::InOut => f.write_str("inout ")?,
            }
            write!(f, "{} {}", crate::generate::float_type(p.width), p.name)?;
        }
        f.write_str(")")
    }
}

/// `float` to `float4`'s width.
fn width(ty: &str) -> Option<u8> {
    match ty {
        "float" | "float1" => Some(1),
        "float2" => Some(2),
        "float3" => Some(3),
        "float4" => Some(4),
        _ => None,
    }
}

/// What a call node's body writes ([`Body::Call`]).
#[derive(Debug, PartialEq, Eq)]
pub struct Call {
    /// The function.
    pub function: &'static str,
    /// Its parameters after the fragment, in order.
    pub parameters: &'static [CallParameter],
    /// It takes the fragment first.
    pub context: bool,
    /// Its result's width, `None` for `void`.
    pub result: Option<u8>,
}

/// A parameter of a [`Call`].
#[derive(Debug, PartialEq, Eq)]
pub struct CallParameter {
    /// Its name: its ports'.
    pub name: &'static str,
    /// Its width.
    pub width: u8,
    /// In, out or both.
    pub direction: Direction,
}

/// The kinds made for calls, by their kind's name ([`CALL_KIND`] or
/// [`crate::function::FUNCTION_KIND`]), function and signature text.
#[allow(clippy::type_complexity)]
static CALLS: Mutex<Vec<(&'static str, &'static str, &'static str, &'static Kind)>> =
    Mutex::new(Vec::new());

/// The kind of a call to `function` of `signature` (its file text, [`Signature::parse`]):
/// its inputs the `in` and `inout` parameters (zero where unlinked), its outputs `out` (the
/// result) and the `out` and `inout` parameters. Made once per function and signature.
///
/// # Errors
///
/// The function's name or the signature is not one a call can have.
pub fn call_kind(function: &str, signature: &str) -> Result<&'static Kind, String> {
    made_kind(CALL_KIND, function, signature)
}

/// [`call_kind`] for a call of kind `name`: a Slang function's ([`Body::Call`]) or a
/// function file's ([`Body::FunctionCall`], E10-3g-2, its function the file's name).
pub(crate) fn made_kind(
    name: &'static str,
    function: &str,
    signature: &str,
) -> Result<&'static Kind, String> {
    let mut calls = CALLS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(&(.., kind)) = calls
        .iter()
        .find(|(n, f, s, _)| *n == name && *f == function && *s == signature)
    {
        return Ok(kind);
    }
    if name == CALL_KIND && !crate::generate::identifier(function) {
        return Err(format!("`{function}` cannot name a Slang function"));
    }
    let parsed = Signature::parse(signature)?;
    let leak = |s: &str| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) };
    let parameters: &'static [CallParameter] = Box::leak(
        parsed
            .parameters
            .iter()
            .map(|p| CallParameter {
                name: leak(&p.name),
                width: p.width,
                direction: p.direction,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let inputs: Vec<Input> = parameters
        .iter()
        .filter(|p| p.direction != Direction::Out)
        .map(|p| Input {
            name: p.name,
            ty: PortType::Float(p.width),
            fill: Fill::Value([0.0; 4]),
        })
        .collect();
    let outputs: Vec<Output> = parsed
        .result
        .map(|w| Output {
            name: "out",
            ty: PortType::Float(w),
        })
        .into_iter()
        .chain(
            parameters
                .iter()
                .filter(|p| p.direction != Direction::In)
                .map(|p| Output {
                    name: p.name,
                    ty: PortType::Float(p.width),
                }),
        )
        .collect();
    let function = leak(function);
    let call: &'static Call = Box::leak(Box::new(Call {
        function,
        parameters,
        context: parsed.context,
        result: parsed.result,
    }));
    let (category, body) = if name == CALL_KIND {
        (CALL_CATEGORY, Body::Call(call))
    } else {
        (crate::function::FUNCTION_CATEGORY, Body::FunctionCall(call))
    };
    let kind: &'static Kind = Box::leak(Box::new(Kind {
        name,
        title: function,
        category,
        inputs: Box::leak(inputs.into_boxed_slice()),
        outputs: Box::leak(outputs.into_boxed_slice()),
        body,
        context: parsed.context,
    }));
    calls.push((name, function, leak(signature), kind));
    Ok(kind)
}

/// `node`'s kind: the library's by its name, or its call's ([`call_kind`]).
///
/// # Errors
///
/// No kind has its name, or its call is not one a node can make.
pub fn node_kind(node: &Node) -> Result<&'static Kind, String> {
    if node.kind == CALL_KIND {
        return call_kind(&node.function, &node.signature);
    }
    if node.kind == crate::function::FUNCTION_KIND {
        return made_kind(
            crate::function::FUNCTION_KIND,
            &node.function,
            &node.signature,
        );
    }
    library::kind(&node.kind).ok_or_else(|| format!("no node kind `{}`", node.kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A signature reads as Slang spells it and writes back the same; what a port cannot
    /// be is refused with why.
    #[test]
    fn a_signature_reads_and_writes_as_slang_spells_it() {
        let text = "float4(MaterialContext ctx, float3 position, out float mask, inout float2 uv)";
        let s = Signature::parse(text).unwrap();
        assert_eq!(s.result, Some(4));
        assert!(s.context);
        let shape: Vec<(&str, u8, Direction)> = s
            .parameters
            .iter()
            .map(|p| (p.name.as_str(), p.width, p.direction))
            .collect();
        assert_eq!(
            shape,
            [
                ("position", 3, Direction::In),
                ("mask", 1, Direction::Out),
                ("uv", 2, Direction::InOut)
            ]
        );
        assert_eq!(s.to_string(), text);
        assert_eq!(
            Signature::parse(" void ( out float3 a ) ")
                .unwrap()
                .to_string(),
            "void(out float3 a)"
        );
        for (bad, why) in [
            ("float4(int i)", "float to float4"),
            ("float4(float a, float a)", "twice"),
            ("float4(float out)", "cannot name a port"),
            ("void(float a)", "gives nothing back"),
            ("float5()", "cannot return"),
            ("float4", "not a signature"),
            ("float4(float3 MaterialContext ctx)", "not a parameter"),
            ("float4(float a, MaterialContext ctx)", "MaterialContext"),
        ] {
            let error = Signature::parse(bad).unwrap_err();
            assert!(error.contains(why), "{bad}: {error}");
        }
    }

    /// A call's kind: its inputs the in and inout parameters, its outputs the result and
    /// the out and inout ones; made once for a function and signature.
    #[test]
    fn a_calls_kind_has_its_signatures_ports() {
        let signature =
            "float4(MaterialContext ctx, float3 position, inout float2 uv, out float mask)";
        let kind = call_kind("triplanar", signature).unwrap();
        let inputs: Vec<&str> = kind.inputs.iter().map(|i| i.name).collect();
        let outputs: Vec<&str> = kind.outputs.iter().map(|o| o.name).collect();
        assert_eq!(inputs, ["position", "uv"]);
        assert_eq!(outputs, ["out", "uv", "mask"]);
        assert_eq!(kind.output("mask").map(|o| o.ty), Some(PortType::Float(1)));
        assert!(kind.context);
        assert!(core::ptr::eq(
            kind,
            call_kind("triplanar", signature).unwrap()
        ));
        assert!(!core::ptr::eq(kind, call_kind("other", signature).unwrap()));
        assert!(call_kind("not a name", signature).is_err());
        let node = Node::new(CALL_KIND, [0.0; 2]).with_call("triplanar", signature);
        assert!(core::ptr::eq(node_kind(&node).unwrap(), kind));
        assert!(node_kind(&Node::new("nope", [0.0; 2])).is_err());
    }
}
