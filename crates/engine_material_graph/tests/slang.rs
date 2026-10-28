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
    for kind in KINDS.iter().filter(|k| k.body != Body::Output) {
        add(
            &one(kind, false),
            &format!("K_{}_scalar", kind.name),
            Build::Saved,
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
    let options = material_type_options(&shaders, &file, &types[0]);
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
