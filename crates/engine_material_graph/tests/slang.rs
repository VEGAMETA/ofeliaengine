//! Every node kind and shading model generated, then checked by Slang's front end inside
//! the engine's `forward.slang` (ADR 0092), as the player will compile a graph (ADR 0123).
//! Without Slang's library (the Vulkan SDK's, or `ENGINE_SLANG_LIBRARY`) the test is
//! skipped, unless `ENGINE_REQUIRE_SLANG` is set.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_material_graph::graph::OUTPUT;
use engine_material_graph::library::{Body, KINDS, Kind, PortType};
use engine_material_graph::{Build, Graph, Model, Node, PortRef, generate};
use engine_shader::material_types::{material_type_options, reflect_material_params};
use engine_shader::{Compiler, SlangLibrary};
use std::path::{Path, PathBuf};

/// The engine's shaders: `forward.slang` and what it includes.
fn shader_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine_render/shaders")
}

/// Slang's compiler, or none (said) without its library.
fn compiler() -> Option<Compiler> {
    match SlangLibrary::load().and_then(Compiler::new) {
        Ok(compiler) => Some(compiler),
        Err(e) => {
            assert!(std::env::var("ENGINE_REQUIRE_SLANG").is_err(), "{e}");
            println!("skipped: no Slang library ({e})");
            None
        }
    }
}

/// The output's terms a node's outputs feed, one each, every width converted to theirs.
const ROOTS: [&str; 8] = [
    "base",
    "emission",
    "metallic",
    "roughness",
    "occlusion",
    "alpha",
    "thickness",
    "normal",
];

/// A graph of one node of `kind`, each of its outputs into one of the surface's terms;
/// with `vectors`, a float3 into each of its dynamic inputs (else they take their
/// scalars).
fn one(kind: &Kind, vectors: bool) -> Graph {
    let mut g = Graph::new(Model::Lit);
    let mut node = Node::new(kind.name, [0.0; 2]).with_value([0.5, 0.25, 0.75, 1.0]);
    if kind.body == Body::Texture {
        node = node.named("image");
    }
    g.add("k", node);
    if vectors {
        g.add(
            "v",
            Node::new("vector3", [0.0; 2]).with_value([0.3, 0.4, 0.5, 0.0]),
        );
        for input in kind.inputs.iter().filter(|i| i.ty == PortType::Dynamic) {
            g.link(PortRef::new("v", "value"), PortRef::new("k", input.name));
        }
    }
    for (output, root) in kind.outputs.iter().zip(ROOTS) {
        g.link(PortRef::new("k", output.name), PortRef::new(OUTPUT, root));
    }
    g
}

/// E10-3g-1: Slang's error in an Expression node's code is at a line the generator's line
/// map gives to that node.
#[test]
fn an_error_in_an_expressions_code_is_at_its_node() {
    let Some(mut compiler) = compiler() else {
        return;
    };
    let mut g = Graph::new(Model::Lit);
    g.add(
        "e",
        Node::new("expression", [0.0; 2]).with_code("a * undefined_thing"),
    )
    .link(PortRef::new("e", "out"), PortRef::new(OUTPUT, "base"));
    let out = generate(&g, "Broken", Build::Editing).unwrap();
    let dir = std::env::temp_dir().join(format!("material_graph_error_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("broken.slang");
    std::fs::write(&file, &out.text).unwrap();
    let shaders = shader_dir();
    let options = material_type_options(&shaders, &file, "Broken");
    let found = compiler.check_file(&shaders.join("forward.slang"), &options);
    let error = found
        .iter()
        .find(|f| f.error && f.path.ends_with("broken.slang"))
        .unwrap_or_else(|| panic!("no error in the module: {found:?}"));
    let line = error.start.0.saturating_sub(1) as usize;
    assert_eq!(
        out.node_at(line),
        Some("e"),
        "{}: {}",
        error.message,
        out.text
    );
    assert!(
        error.message.contains("undefined_thing"),
        "{}",
        error.message
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_kind_and_model_compiles_inside_forward_slang() {
    let Some(mut compiler) = compiler() else {
        return;
    };
    let mut module = String::new();
    let mut types = Vec::new();
    let mut add = |graph: &Graph, name: &str, build: Build| {
        let generated = generate(graph, name, build).unwrap();
        module.push_str(&generated.text);
        module.push('\n');
        types.push(name.to_owned());
        generated
    };
    // A function's inputs and outputs are made links when it is called (the functions'
    // own test below).
    for kind in KINDS.iter().filter(|k| {
        !matches!(
            k.body,
            Body::Output | Body::FunctionInput(_) | Body::FunctionOutput(_)
        )
    }) {
        add(
            &one(kind, false),
            &format!("K_{}_scalar", kind.name),
            Build::Saved,
        );
        // Its node previews too (E10-4g).
        add(
            &one(kind, false),
            &format!("E_{}_scalar", kind.name),
            Build::Editing,
        );
        if kind.inputs.iter().any(|i| i.ty == PortType::Dynamic) {
            add(
                &one(kind, true),
                &format!("K_{}_vector", kind.name),
                Build::Saved,
            );
        }
    }
    for model in Model::ALL {
        let mut g = Graph::new(model);
        g.add(
            "n",
            Node::new("number", [0.0; 2])
                .with_value([0.4; 4])
                .named("shine"),
        )
        .link(
            PortRef::new("n", "value"),
            PortRef::new(OUTPUT, "roughness"),
        );
        add(&g, &format!("M_{}", model.name()), Build::Saved);
    }
    // A graph with a texture, a named number and an unnamed one, while it is open.
    let mut editing = Graph::new(Model::Lit);
    editing
        .add("t", Node::new("texture", [0.0; 2]).named("albedo"))
        .add(
            "r",
            Node::new("number", [0.0; 2])
                .with_value([0.6; 4])
                .named("roughness"),
        )
        .add("m", Node::new("number", [0.0; 2]).with_value([0.1; 4]))
        .link(PortRef::new("t", "rgba"), PortRef::new(OUTPUT, "base"))
        .link(
            PortRef::new("r", "value"),
            PortRef::new(OUTPUT, "roughness"),
        )
        .link(PortRef::new("m", "value"), PortRef::new(OUTPUT, "metallic"));
    let open = add(&editing, "Editing", Build::Editing);

    let dir = std::env::temp_dir().join(format!("material_graph_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("graphs.slang");
    std::fs::write(&file, &module).unwrap();
    let shaders = shader_dir();
    // The editing build's type as the one drawn: its module (among the others) turns on
    // `nodePreviewFragment`, which calls the drawn type's `node_preview` (E10-4g).
    let options = material_type_options(&shaders, &file, "Editing");
    let found = compiler.check_file(&shaders.join("forward.slang"), &options);
    let lines: Vec<&str> = module.lines().collect();
    let errors: Vec<String> = found
        .iter()
        .filter(|f| f.error)
        .map(|f| {
            let at = f.start.0.saturating_sub(1) as usize;
            format!(
                "{}:{}: {}\n    {}",
                f.path,
                f.start.0,
                f.message,
                lines.get(at).copied().unwrap_or("")
            )
        })
        .collect();
    assert!(
        errors.is_empty(),
        "{} types; errors:\n{}",
        types.len(),
        errors.join("\n")
    );
    // The editing build's parameters, as Slang lays them out: the named ones and the
    // unnamed number's own.
    let layout = reflect_material_params(&mut compiler, &shaders, &file, "Editing").unwrap();
    let names: Vec<&str> = layout.fields.iter().map(|f| f.name.as_str()).collect();
    let ours: Vec<&str> = open.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ours);
    assert_eq!(ours, ["albedo", "_m", "roughness"]);
    println!("{} types checked in one module", types.len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A project module with a function marked as a node (E10-3g-3), including the mark's
/// declaration as the engine's `nodes.slang` asks.
const MAPPING: &str = r#"#include "nodes.slang"

// A swirl of the colour about the middle of the surface's coordinates, and how near.
[Node("Swirl", "Mapping")]
float3 swirl(MaterialContext ctx, float3 color, float amount, out float mask)
{
    float2 uv = ctx.uv - 0.5;
    mask = saturate(1.0 - length(uv) * 2.0);
    return color * (0.5 + 0.5 * cos(amount * length(uv)));
}
"#;

/// The signature of a function as Slang's reflection reads it, in the graph's spelling.
fn reflected_signature(f: &engine_shader::MarkedFunction) -> String {
    use engine_shader::{Direction, FieldKind, ValueType};
    let ty = |t: &ValueType| match t {
        ValueType::Field(FieldKind::Float(1)) => String::from("float"),
        ValueType::Field(FieldKind::Float(n)) => format!("float{n}"),
        ValueType::Named(name) => name.clone(),
        other => format!("{other:?}"),
    };
    let parameters: Vec<String> = f
        .parameters
        .iter()
        .map(|p| {
            let direction = match p.direction {
                Direction::In => "",
                Direction::Out => "out ",
                Direction::InOut => "inout ",
            };
            format!("{direction}{} {}", ty(&p.ty), p.name)
        })
        .collect();
    format!("{}({})", ty(&f.result), parameters.join(", "))
}

/// E10-3g-3: a call of a project module's `[Node]` function, its signature as the graph keeps
/// it, compiles inside `forward.slang` (its out parameter linked to a term as an output);
/// Slang's reflection of the function gives back that signature; the graph's module
/// includes the mark's declaration before the module.
#[test]
fn a_call_of_a_node_function_compiles_and_its_signature_is_slangs() {
    use engine_material_graph::{CALL_KIND, Sources, generate_with};
    use engine_scene::{AssetId, AssetRef};
    let Some(mut compiler) = compiler() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("material_graph_call_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("shaders")).unwrap();
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::write(dir.join("shaders/mapping.slang"), MAPPING).unwrap();
    let signature = "float3(MaterialContext ctx, float3 color, float amount, out float mask)";
    let module = AssetRef::file(AssetId::from_u128(7));
    let mut call = Node::new(CALL_KIND, [200.0, 0.0]).with_call("swirl", signature);
    call.module = module;
    let mut g = Graph::new(Model::Lit);
    g.add(
        "c",
        Node::new("color", [0.0; 2]).with_value([0.8, 0.3, 0.1, 1.0]),
    )
    .add("sw", call)
    .link(PortRef::new("c", "rgba"), PortRef::new("sw", "color"))
    .link(PortRef::new("sw", "out"), PortRef::new(OUTPUT, "base"))
    .link(
        PortRef::new("sw", "mask"),
        PortRef::new(OUTPUT, "roughness"),
    );
    let modules = |m: AssetRef| (m == module).then(|| String::from("../shaders/mapping.slang"));
    let shaders = shader_dir();
    for build in [Build::Editing, Build::Saved] {
        let sources = Sources {
            modules: &modules,
            functions: &|_| None,
        };
        let out = generate_with(&g, "Swirled", build, &sources).unwrap();
        let first = out
            .text
            .lines()
            .position(|l| l == "#include \"nodes.slang\"");
        let then = out
            .text
            .lines()
            .position(|l| l == "#include \"../shaders/mapping.slang\"");
        assert!(first.is_some() && first < then, "{}", out.text);
        assert_eq!(out.node_at(then.unwrap()), Some("sw"));
        let file = dir.join("cache/swirled.slang");
        std::fs::write(&file, &out.text).unwrap();
        let options = material_type_options(&shaders, &file, "Swirled");
        let found = compiler.check_file(&shaders.join("forward.slang"), &options);
        let errors: Vec<_> = found.iter().filter(|f| f.error).collect();
        assert!(errors.is_empty(), "{build:?}: {errors:#?}\n{}", out.text);
        if build == Build::Saved {
            let names = engine_shader::marked_names(MAPPING, "Node");
            let functions = compiler
                .marked_functions(&shaders.join("forward.slang"), &options, &names, "Node")
                .unwrap();
            assert_eq!(functions.len(), 1, "{functions:#?}");
            assert_eq!(functions[0].arguments, ["Swirl", "Mapping"]);
            assert_eq!(reflected_signature(&functions[0]), signature);
        }
    }
    // A call whose module is not found is its node's error.
    let error = generate_with(&g, "Swirled", Build::Saved, &Sources::none()).unwrap_err();
    assert_eq!(error.node.as_deref(), Some("sw"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// E10-3g-2: a graph calling a function that calls another (its inputs linked and left at
/// their values, its outputs read by two terms) compiles inside `forward.slang` in both
/// builds; the calls' nodes are written as the calls'.
#[test]
fn a_call_of_a_function_calling_a_function_compiles() {
    use engine_material_graph::function::{FUNCTION_KIND, input_kind, output_kind};
    use engine_material_graph::{Shown, Sources, generate_shown, generate_with, signature_of};
    use engine_scene::{AssetId, AssetRef};
    let Some(mut compiler) = compiler() else {
        return;
    };
    // `scale`: its value times its factor.
    let mut scale = Graph::new(Model::Lit);
    scale.nodes.clear();
    scale
        .add("v", Node::new(input_kind(3), [0.0, 0.0]).named("value"))
        .add(
            "f",
            Node::new(input_kind(1), [0.0, 100.0])
                .named("factor")
                .with_value([0.5, 0.0, 0.0, 0.0]),
        )
        .add("m", Node::new("multiply", [150.0, 0.0]))
        .add("o", Node::new(output_kind(3), [300.0, 0.0]).named("scaled"))
        .link(PortRef::new("v", "value"), PortRef::new("m", "a"))
        .link(PortRef::new("f", "value"), PortRef::new("m", "b"))
        .link(PortRef::new("m", "out"), PortRef::new("o", "value"));
    let scale_file = AssetRef::file(AssetId::from_u128(11));
    let scale_signature = signature_of(&scale).unwrap().to_string();
    // `glow`: its colour scaled twice, and the once scaled colour's length.
    let mut glow = Graph::new(Model::Lit);
    glow.nodes.clear();
    let call = |at: f32| {
        let mut n = Node::new(FUNCTION_KIND, [at, 0.0]).with_call("scale", &scale_signature);
        n.module = scale_file;
        n
    };
    glow.add("c", Node::new(input_kind(3), [0.0, 0.0]).named("color"))
        .add("s1", call(150.0))
        .add("s2", call(300.0).with_input("factor", [2.0; 4]))
        .add("l", Node::new("length", [450.0, 100.0]))
        .add("o", Node::new(output_kind(3), [600.0, 0.0]).named("lit"))
        .add(
            "b",
            Node::new(output_kind(1), [600.0, 100.0]).named("brightness"),
        )
        .link(PortRef::new("c", "value"), PortRef::new("s1", "value"))
        .link(PortRef::new("s1", "scaled"), PortRef::new("s2", "value"))
        .link(PortRef::new("s2", "scaled"), PortRef::new("o", "value"))
        .link(PortRef::new("s1", "scaled"), PortRef::new("l", "in"))
        .link(PortRef::new("l", "out"), PortRef::new("b", "value"));
    let glow_file = AssetRef::file(AssetId::from_u128(12));
    let mut g = Graph::new(Model::Lit);
    let mut use_glow = Node::new(FUNCTION_KIND, [200.0, 0.0])
        .with_call("glow", &signature_of(&glow).unwrap().to_string());
    use_glow.module = glow_file;
    g.add(
        "col",
        Node::new("color", [0.0; 2]).with_value([0.8, 0.4, 0.2, 1.0]),
    )
    .add("gl", use_glow)
    .link(PortRef::new("col", "rgba"), PortRef::new("gl", "color"))
    .link(PortRef::new("gl", "lit"), PortRef::new(OUTPUT, "base"))
    .link(
        PortRef::new("gl", "brightness"),
        PortRef::new(OUTPUT, "roughness"),
    );
    let functions = |r: AssetRef| match r {
        r if r == scale_file => Some(scale.clone()),
        r if r == glow_file => Some(glow.clone()),
        _ => None,
    };
    let sources = Sources {
        modules: &|_| None,
        functions: &functions,
    };
    let dir = std::env::temp_dir().join(format!("material_graph_functions_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let shaders = shader_dir();
    for build in [Build::Editing, Build::Saved] {
        let out = generate_with(&g, "Glowing", build, &sources).unwrap();
        assert!(
            out.lines
                .iter()
                .flatten()
                .all(|n| n == "col" || n == "gl" || n == OUTPUT),
            "every line a node of the graph's own: {:?}",
            out.lines
        );
        let file = dir.join("glowing.slang");
        std::fs::write(&file, &out.text).unwrap();
        let options = material_type_options(&shaders, &file, "Glowing");
        let found = compiler.check_file(&shaders.join("forward.slang"), &options);
        let errors: Vec<_> = found.iter().filter(|f| f.error).collect();
        assert!(errors.is_empty(), "{build:?}: {errors:#?}\n{}", out.text);
    }
    // E10-4g-3: `glow`'s tab, its own nodes previewed through its preview's call of it (its
    // input, its two calls, the length and its two outputs).
    let mut tab = Graph::new(Model::Unlit);
    let mut call_glow = Node::new(FUNCTION_KIND, [0.0, 0.0])
        .with_call("glow", &signature_of(&glow).unwrap().to_string());
    call_glow.module = glow_file;
    tab.add("function", call_glow).link(
        PortRef::new("function", "lit"),
        PortRef::new(OUTPUT, "base"),
    );
    let out = generate_shown(
        &tab,
        "GlowTab",
        Build::Editing,
        &sources,
        Shown::Function("function"),
    )
    .unwrap();
    let mut shown: Vec<&str> = out.previews.iter().map(|p| p.node.as_str()).collect();
    shown.sort_unstable();
    assert_eq!(shown, ["b", "c", "l", "o", "s1", "s2"]);
    let file = dir.join("glowtab.slang");
    std::fs::write(&file, &out.text).unwrap();
    let options = material_type_options(&shaders, &file, "GlowTab");
    let found = compiler.check_file(&shaders.join("forward.slang"), &options);
    let errors: Vec<_> = found.iter().filter(|f| f.error).collect();
    assert!(errors.is_empty(), "the tab: {errors:#?}\n{}", out.text);
    let _ = std::fs::remove_dir_all(&dir);
}
