//! The node kinds (ADR 0123's first set): each a snippet of Slang with typed ports, as
//! Material Maker writes its nodes, so a new kind is a few lines here.

/// What a port carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    /// `float` to `float4`, by its width (1 to 4).
    Float(u8),
    /// The width of what is linked (Shader Graph's dynamic vectors): the narrowest vector
    /// linked to the node's dynamic inputs, a scalar where only scalars are.
    Dynamic,
}

/// What an input takes when nothing is linked to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fill {
    /// These components (as many as the input is wide); a node's own value for the input
    /// replaces them.
    Value([f32; 4]),
    /// This expression of the fragment (`ctx.uv`), of the input's type.
    Expr(&'static str),
}

/// An input port.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Input {
    /// Its name in files and on the node.
    pub name: &'static str,
    /// What it takes.
    pub ty: PortType,
    /// What it takes when nothing is linked.
    pub fill: Fill,
}

/// An output port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    /// Its name in files and on the node.
    pub name: &'static str,
    /// What it gives.
    pub ty: PortType,
}

/// How a kind writes its outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// One expression an output, `{input}` the inputs' values.
    Expr(&'static [&'static str]),
    /// As [`Body::Expr`], with `scalar`'s expressions where the node's dynamic width is 1
    /// (Slang has no `dot`, `length` or `normalize` of a scalar).
    Math {
        /// For vectors.
        vector: &'static [&'static str],
        /// For scalars.
        scalar: &'static [&'static str],
    },
    /// A constant of this width: the node's value, or its parameter.
    Constant(u8),
    /// A texture, always a parameter, sampled at the `uv` input; its outputs the sample
    /// (`rgba`), its colour (`rgb`) and each channel.
    Texture,
    /// Slang the node holds (`Node::code`, E10-3g-1): a function of the type taking the
    /// fragment and the node's inputs, returning its output.
    Expression,
    /// A call of a project module's `[Node]` function (E10-3g-3, [`crate::call`]): its
    /// `out` and `inout` parameters variables, its result one.
    Call(&'static crate::call::Call),
    /// The surface: the graph's one output node.
    Output,
}

/// A node kind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Kind {
    /// Its name in files (`add`).
    pub name: &'static str,
    /// Its name on the canvas and in the node search (`Add`).
    pub title: &'static str,
    /// Its group in the node search.
    pub category: &'static str,
    /// Its inputs.
    pub inputs: &'static [Input],
    /// Its outputs.
    pub outputs: &'static [Output],
    /// How it writes them.
    pub body: Body,
    /// It reads the fragment (`ctx`): not allowed where only numbers can be (the alpha
    /// cutoff).
    pub context: bool,
}

impl Kind {
    /// Its input `name`.
    pub fn input(&self, name: &str) -> Option<&Input> {
        self.inputs.iter().find(|i| i.name == name)
    }

    /// Its output `name`.
    pub fn output(&self, name: &str) -> Option<&Output> {
        self.outputs.iter().find(|o| o.name == name)
    }
}

/// The kind named `name`.
pub fn kind(name: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.name == name)
}

/// The output node's kind.
pub const OUTPUT_KIND: &str = "output";

/// A reroute's kind: a point a link passes through (E10-3e-2a), its value its input's.
pub const REROUTE_KIND: &str = "reroute";

/// An Expression node's kind: Slang written in the node (E10-3g-1, Godot's Expression).
pub const EXPRESSION_KIND: &str = "expression";

const F1: PortType = PortType::Float(1);
const F2: PortType = PortType::Float(2);
const F3: PortType = PortType::Float(3);
const F4: PortType = PortType::Float(4);
const DYN: PortType = PortType::Dynamic;

const fn input(name: &'static str, ty: PortType, fill: Fill) -> Input {
    Input { name, ty, fill }
}

const fn output(name: &'static str, ty: PortType) -> Output {
    Output { name, ty }
}

/// The geometric normal: the interpolated one, on the fragment's side.
pub const GEOMETRIC_NORMAL: &str = "normalize(ctx.normal * ctx.side)";

/// The direction from the fragment to the camera.
pub const VIEW_DIRECTION: &str =
    "normalize(read_float4(&push.frame.camera_position).xyz - ctx.world_position)";

const ZERO: [f32; 4] = [0.0; 4];
const ONE: [f32; 4] = [1.0; 4];
const HALF: [f32; 4] = [0.5; 4];

/// Every kind but the output, for the node search, then the output.
pub const KINDS: &[Kind] = &[
    // Inputs.
    Kind {
        name: "number",
        title: "Number",
        category: "Input",
        inputs: &[],
        outputs: &[output("value", F1)],
        body: Body::Constant(1),
        context: false,
    },
    Kind {
        name: "vector2",
        title: "Vector 2",
        category: "Input",
        inputs: &[],
        outputs: &[output("value", F2)],
        body: Body::Constant(2),
        context: false,
    },
    Kind {
        name: "vector3",
        title: "Vector 3",
        category: "Input",
        inputs: &[],
        outputs: &[output("value", F3)],
        body: Body::Constant(3),
        context: false,
    },
    Kind {
        name: "vector4",
        title: "Vector 4",
        category: "Input",
        inputs: &[],
        outputs: &[output("value", F4)],
        body: Body::Constant(4),
        context: false,
    },
    Kind {
        name: "color",
        title: "Color",
        category: "Input",
        inputs: &[],
        outputs: &[output("rgba", F4)],
        body: Body::Constant(4),
        context: false,
    },
    Kind {
        name: "texture",
        title: "Texture",
        category: "Input",
        inputs: &[input("uv", F2, Fill::Expr("ctx.uv"))],
        outputs: &[
            output("rgba", F4),
            output("rgb", F3),
            output("r", F1),
            output("g", F1),
            output("b", F1),
            output("a", F1),
        ],
        body: Body::Texture,
        context: true,
    },
    Kind {
        name: "uv",
        title: "Texture Coordinates",
        category: "Input",
        inputs: &[],
        outputs: &[output("uv", F2)],
        body: Body::Expr(&["ctx.uv"]),
        context: true,
    },
    Kind {
        name: "world_position",
        title: "World Position",
        category: "Input",
        inputs: &[],
        outputs: &[output("position", F3)],
        body: Body::Expr(&["ctx.world_position"]),
        context: true,
    },
    Kind {
        name: "normal",
        title: "Normal",
        category: "Input",
        inputs: &[],
        outputs: &[output("normal", F3)],
        body: Body::Expr(&[GEOMETRIC_NORMAL]),
        context: true,
    },
    Kind {
        name: "tangent",
        title: "Tangent",
        category: "Input",
        inputs: &[],
        outputs: &[output("tangent", F3)],
        body: Body::Expr(&["normalize(ctx.tangent.xyz * ctx.side)"]),
        context: true,
    },
    Kind {
        name: "view_direction",
        title: "View Direction",
        category: "Input",
        inputs: &[],
        outputs: &[output("direction", F3)],
        body: Body::Expr(&[VIEW_DIRECTION]),
        context: true,
    },
    // Math.
    Kind {
        name: "add",
        title: "Add",
        category: "Math",
        inputs: &[
            input("a", DYN, Fill::Value(ZERO)),
            input("b", DYN, Fill::Value(ZERO)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["({a} + {b})"]),
        context: false,
    },
    Kind {
        name: "subtract",
        title: "Subtract",
        category: "Math",
        inputs: &[
            input("a", DYN, Fill::Value(ZERO)),
            input("b", DYN, Fill::Value(ZERO)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["({a} - {b})"]),
        context: false,
    },
    Kind {
        name: "multiply",
        title: "Multiply",
        category: "Math",
        inputs: &[
            input("a", DYN, Fill::Value(ONE)),
            input("b", DYN, Fill::Value(ONE)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["({a} * {b})"]),
        context: false,
    },
    Kind {
        name: "divide",
        title: "Divide",
        category: "Math",
        inputs: &[
            input("a", DYN, Fill::Value(ONE)),
            input("b", DYN, Fill::Value(ONE)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["({a} / {b})"]),
        context: false,
    },
    Kind {
        name: "one_minus",
        title: "One Minus",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ONE))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["(1.0 - {in})"]),
        context: false,
    },
    Kind {
        name: "negate",
        title: "Negate",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ZERO))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["(-{in})"]),
        context: false,
    },
    Kind {
        name: "abs",
        title: "Absolute",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ZERO))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["abs({in})"]),
        context: false,
    },
    Kind {
        name: "floor",
        title: "Floor",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ZERO))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["floor({in})"]),
        context: false,
    },
    Kind {
        name: "fraction",
        title: "Fraction",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ZERO))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["frac({in})"]),
        context: false,
    },
    Kind {
        name: "min",
        title: "Minimum",
        category: "Math",
        inputs: &[
            input("a", DYN, Fill::Value(ZERO)),
            input("b", DYN, Fill::Value(ZERO)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["min({a}, {b})"]),
        context: false,
    },
    Kind {
        name: "max",
        title: "Maximum",
        category: "Math",
        inputs: &[
            input("a", DYN, Fill::Value(ZERO)),
            input("b", DYN, Fill::Value(ZERO)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["max({a}, {b})"]),
        context: false,
    },
    Kind {
        name: "clamp",
        title: "Clamp",
        category: "Math",
        inputs: &[
            input("in", DYN, Fill::Value(ZERO)),
            input("min", DYN, Fill::Value(ZERO)),
            input("max", DYN, Fill::Value(ONE)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["clamp({in}, {min}, {max})"]),
        context: false,
    },
    Kind {
        name: "saturate",
        title: "Saturate",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ZERO))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["saturate({in})"]),
        context: false,
    },
    Kind {
        name: "power",
        title: "Power",
        category: "Math",
        inputs: &[
            input("base", DYN, Fill::Value(ONE)),
            input("exponent", DYN, Fill::Value(ONE)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["pow({base}, {exponent})"]),
        context: false,
    },
    Kind {
        name: "square_root",
        title: "Square Root",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ONE))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["sqrt({in})"]),
        context: false,
    },
    Kind {
        name: "sine",
        title: "Sine",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ZERO))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["sin({in})"]),
        context: false,
    },
    Kind {
        name: "cosine",
        title: "Cosine",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ZERO))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["cos({in})"]),
        context: false,
    },
    Kind {
        name: "lerp",
        title: "Lerp",
        category: "Math",
        inputs: &[
            input("a", DYN, Fill::Value(ZERO)),
            input("b", DYN, Fill::Value(ONE)),
            input("t", DYN, Fill::Value(HALF)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["lerp({a}, {b}, {t})"]),
        context: false,
    },
    Kind {
        name: "step",
        title: "Step",
        category: "Math",
        inputs: &[
            input("edge", DYN, Fill::Value(HALF)),
            input("in", DYN, Fill::Value(ZERO)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["step({edge}, {in})"]),
        context: false,
    },
    Kind {
        name: "smoothstep",
        title: "Smoothstep",
        category: "Math",
        inputs: &[
            input("from", DYN, Fill::Value(ZERO)),
            input("to", DYN, Fill::Value(ONE)),
            input("in", DYN, Fill::Value(HALF)),
        ],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["smoothstep({from}, {to}, {in})"]),
        context: false,
    },
    Kind {
        name: "dot",
        title: "Dot Product",
        category: "Math",
        inputs: &[
            input("a", DYN, Fill::Value(ZERO)),
            input("b", DYN, Fill::Value(ZERO)),
        ],
        outputs: &[output("out", F1)],
        body: Body::Math {
            vector: &["dot({a}, {b})"],
            scalar: &["({a} * {b})"],
        },
        context: false,
    },
    Kind {
        name: "cross",
        title: "Cross Product",
        category: "Math",
        inputs: &[
            input("a", F3, Fill::Value(ZERO)),
            input("b", F3, Fill::Value(ZERO)),
        ],
        outputs: &[output("out", F3)],
        body: Body::Expr(&["cross({a}, {b})"]),
        context: false,
    },
    Kind {
        name: "normalize",
        title: "Normalize",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ONE))],
        outputs: &[output("out", DYN)],
        body: Body::Math {
            vector: &["normalize({in})"],
            scalar: &["sign({in})"],
        },
        context: false,
    },
    Kind {
        name: "length",
        title: "Length",
        category: "Math",
        inputs: &[input("in", DYN, Fill::Value(ZERO))],
        outputs: &[output("out", F1)],
        body: Body::Math {
            vector: &["length({in})"],
            scalar: &["abs({in})"],
        },
        context: false,
    },
    Kind {
        name: "distance",
        title: "Distance",
        category: "Math",
        inputs: &[
            input("a", DYN, Fill::Value(ZERO)),
            input("b", DYN, Fill::Value(ZERO)),
        ],
        outputs: &[output("out", F1)],
        body: Body::Math {
            vector: &["distance({a}, {b})"],
            scalar: &["abs({a} - {b})"],
        },
        context: false,
    },
    // Vectors.
    Kind {
        name: "split",
        title: "Split",
        category: "Vector",
        inputs: &[input("in", F4, Fill::Value([0.0, 0.0, 0.0, 1.0]))],
        outputs: &[
            output("x", F1),
            output("y", F1),
            output("z", F1),
            output("w", F1),
        ],
        body: Body::Expr(&["({in}).x", "({in}).y", "({in}).z", "({in}).w"]),
        context: false,
    },
    Kind {
        name: "combine",
        title: "Combine",
        category: "Vector",
        inputs: &[
            input("x", F1, Fill::Value(ZERO)),
            input("y", F1, Fill::Value(ZERO)),
            input("z", F1, Fill::Value(ZERO)),
            input("w", F1, Fill::Value(ONE)),
        ],
        outputs: &[output("xyzw", F4), output("xyz", F3), output("xy", F2)],
        body: Body::Expr(&[
            "float4({x}, {y}, {z}, {w})",
            "float3({x}, {y}, {z})",
            "float2({x}, {y})",
        ]),
        context: false,
    },
    // Surface.
    Kind {
        name: "normal_map",
        title: "Normal Map",
        category: "Surface",
        inputs: &[
            input("color", F4, Fill::Value([0.5, 0.5, 1.0, 1.0])),
            input("strength", F1, Fill::Value(ONE)),
        ],
        outputs: &[output("normal", F3)],
        body: Body::Expr(&["graph_normal_map(ctx, {color}, {strength})"]),
        context: true,
    },
    Kind {
        name: "fresnel",
        title: "Fresnel",
        category: "Surface",
        inputs: &[
            input("power", F1, Fill::Value([5.0, 0.0, 0.0, 0.0])),
            input("normal", F3, Fill::Expr(GEOMETRIC_NORMAL)),
        ],
        outputs: &[output("out", F1)],
        body: Body::Expr(&[
            "pow(1.0 - saturate(dot({normal}, normalize(read_float4(&push.frame.camera_position).xyz - ctx.world_position))), {power})",
        ]),
        context: true,
    },
    // Slang written in the node, Godot's Expression: an expression of `a` to `d` (four
    // floats each, what is linked converted as everywhere) and the fragment's `ctx`, or a
    // body that returns; the code a function of the type, Slang's errors in it the node's.
    Kind {
        name: EXPRESSION_KIND,
        title: "Expression",
        category: "Code",
        inputs: &[
            input("a", F4, Fill::Value(ZERO)),
            input("b", F4, Fill::Value(ZERO)),
            input("c", F4, Fill::Value(ZERO)),
            input("d", F4, Fill::Value(ZERO)),
        ],
        outputs: &[output("out", F4)],
        body: Body::Expression,
        context: true,
    },
    // A point a link passes through to keep a graph tidy: its value its input's, written
    // in place (no code of its own).
    Kind {
        name: REROUTE_KIND,
        title: "Reroute",
        category: "Layout",
        inputs: &[input("in", DYN, Fill::Value(ZERO))],
        outputs: &[output("out", DYN)],
        body: Body::Expr(&["{in}"]),
        context: false,
    },
    // The surface's terms: those of every model, then each model's own (`Model::terms`).
    Kind {
        name: OUTPUT_KIND,
        title: "Surface",
        category: "Output",
        inputs: &[
            input("base", F4, Fill::Value([0.8, 0.8, 0.8, 1.0])),
            input("normal", F3, Fill::Expr(GEOMETRIC_NORMAL)),
            input("metallic", F1, Fill::Value(ZERO)),
            input("roughness", F1, Fill::Value(HALF)),
            input("emission", F3, Fill::Value(ZERO)),
            input("alpha", F1, Fill::Value(ONE)),
            input("alpha_cutoff", F1, Fill::Value(HALF)),
            input("occlusion", F1, Fill::Value(ONE)),
            input("thickness", F1, Fill::Value(ZERO)),
            input("clearcoat", F1, Fill::Value(ONE)),
            input(
                "clearcoat_roughness",
                F1,
                Fill::Value([0.03, 0.0, 0.0, 0.0]),
            ),
            input("sheen", F3, Fill::Value(ONE)),
            input("sheen_roughness", F1, Fill::Value(HALF)),
            input(
                "hair_tangent",
                F3,
                Fill::Expr("normalize(ctx.tangent.xyz * ctx.side)"),
            ),
            input("hair_shift", F1, Fill::Value([0.05, 0.0, 0.0, 0.0])),
            input("toon_steps", F1, Fill::Value([2.0, 0.0, 0.0, 0.0])),
            input("toon_softness", F1, Fill::Value([0.05, 0.0, 0.0, 0.0])),
            input("toon_highlight", F3, Fill::Value(ONE)),
            input("rim", F3, Fill::Value(ZERO)),
            input("rim_width", F1, Fill::Value([0.3, 0.0, 0.0, 0.0])),
        ],
        outputs: &[],
        body: Body::Output,
        context: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_have_unique_names_and_their_bodies_fit_their_ports() {
        for (i, k) in KINDS.iter().enumerate() {
            assert!(
                KINDS[..i].iter().all(|other| other.name != k.name),
                "{} twice",
                k.name
            );
            match k.body {
                Body::Expr(exprs) => assert_eq!(exprs.len(), k.outputs.len(), "{}", k.name),
                Body::Math { vector, scalar } => {
                    assert_eq!(vector.len(), k.outputs.len(), "{}", k.name);
                    assert_eq!(scalar.len(), k.outputs.len(), "{}", k.name);
                }
                Body::Constant(_) | Body::Texture | Body::Output => {}
                // One output, the code's value.
                Body::Expression => assert_eq!(k.outputs.len(), 1, "{}", k.name),
                Body::Call(_) => panic!("{}: calls are made from signatures", k.name),
            }
            // Every `{input}` a body names is one of its inputs.
            let exprs: Vec<&str> = match k.body {
                Body::Expr(e) => e.to_vec(),
                Body::Math { vector, scalar } => vector.iter().chain(scalar).copied().collect(),
                _ => Vec::new(),
            };
            for e in exprs {
                let mut rest = e;
                while let Some(open) = rest.find('{') {
                    let close = rest[open..].find('}').unwrap() + open;
                    let name = &rest[open + 1..close];
                    assert!(k.input(name).is_some(), "{}: {{{name}}}", k.name);
                    rest = &rest[close + 1..];
                }
            }
        }
        assert_eq!(KINDS.last().map(|k| k.name), Some(OUTPUT_KIND));
        assert!(kind("smoothstep").is_some() && kind("nothing").is_none());
    }
}
