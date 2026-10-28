//! The project's Slang functions marked as nodes (ADR 0123's E10-3g-3b): found on a thread
//! of their own with a compiler of its own, so a scan never holds up a frame, and offered by
//! the graph canvas's node search.
//!
//! A module is scanned when the first graph is shown and again when its file changes. Its
//! text gives the names after `[Node` marks (`engine_shader::marked_names`, a text scan, so
//! a module without marks costs a read). One with marks is checked by Slang's front end
//! inside the engine's `forward.slang`, through a wrapper in `.ofelia/check/` that includes
//! the mark's declaration, the module and an empty graph's type, and its functions are read
//! from Slang's reflection: each a title and a group from its mark, and its signature as a
//! call keeps it, or why it cannot be a node.

use engine_material_graph::call::{CALL_CATEGORY, Direction as Passed, Parameter};
use engine_material_graph::{Build, Graph, Model, Signature};
use engine_scene::AssetId;
use engine_shader::material_types::material_type_options;
use engine_shader::{
    Compiler, Direction, FieldKind, MarkedFunction, SlangLibrary, ValueType, marked_names,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

/// A function a project module marks as a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeFunction {
    /// Its name in Slang.
    pub(crate) name: String,
    /// The node's title and the node search's group (the mark's arguments).
    pub(crate) title: String,
    pub(crate) category: String,
    /// Its signature as a call keeps it, or why it cannot be a node.
    pub(crate) signature: Result<String, String>,
}

/// What a scan of a module found: its node functions (none where it marks none), or why it
/// could not be read.
#[derive(Debug)]
pub(crate) struct Scanned {
    /// The module.
    pub(crate) module: AssetId,
    /// Its functions, or why not.
    pub(crate) found: Result<Vec<NodeFunction>, String>,
}

/// The scans' thread.
pub(crate) struct NodeScanner {
    jobs: mpsc::Sender<(AssetId, PathBuf)>,
    done: mpsc::Receiver<Scanned>,
    _thread: JoinHandle<()>,
}

impl NodeScanner {
    /// The thread scanning the modules of the project in `root`; its compiler made at its
    /// first scan. `None` when the thread cannot be started.
    pub(crate) fn start(root: &Path) -> Option<Self> {
        let (jobs, inbox) = mpsc::channel::<(AssetId, PathBuf)>();
        let (outbox, done) = mpsc::channel();
        let check = root.join(crate::session::DIR).join("check");
        let thread = std::thread::Builder::new()
            .name(String::from("node functions"))
            .spawn(move || {
                let mut compiler: Option<Result<Compiler, String>> = None;
                while let Ok((module, path)) = inbox.recv() {
                    engine_profiler::zone!("editor::node_functions::scan");
                    let compiler = compiler.get_or_insert_with(|| {
                        SlangLibrary::load()
                            .and_then(Compiler::new)
                            .map_err(|e| e.to_string())
                    });
                    let found = match compiler {
                        Ok(compiler) => scan(compiler, &check, module, &path),
                        Err(why) => Err(why.clone()),
                    };
                    if outbox.send(Scanned { module, found }).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            jobs,
            done,
            _thread: thread,
        })
    }

    /// Module `module`, the file at `path`, to be scanned.
    pub(crate) fn scan(&self, module: AssetId, path: PathBuf) {
        let _ = self.jobs.send((module, path));
    }

    /// The scans finished since, into `out` (cleared; nothing allocated when there are
    /// none).
    pub(crate) fn take(&self, out: &mut Vec<Scanned>) {
        out.clear();
        while let Ok(scanned) = self.done.try_recv() {
            out.push(scanned);
        }
    }
}

/// The node functions of the module at `path`: none without marks; else read through
/// `compiler` inside `forward.slang`, from a wrapper written in `check`.
fn scan(
    compiler: &mut Compiler,
    check: &Path,
    module: AssetId,
    path: &Path,
) -> Result<Vec<NodeFunction>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let names = marked_names(&text, "Node");
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let shaders = PathBuf::from(engine_render::forward::ENGINE_SHADER_DIR);
    let empty =
        engine_material_graph::generate(&Graph::new(Model::Unlit), "NodeScan", Build::Saved)
            .map_err(|e| e.to_string())?;
    let wrapper = check.join(format!("nodes-{module}.slang"));
    let text = format!(
        "#include \"nodes.slang\"\n#include \"{}\"\n{}",
        path.to_string_lossy().replace('\\', "/"),
        empty.text
    );
    std::fs::create_dir_all(check)
        .and_then(|()| std::fs::write(&wrapper, text))
        .map_err(|e| format!("{}: {e}", wrapper.display()))?;
    let options = material_type_options(&shaders, &wrapper, "NodeScan");
    let found = compiler
        .marked_functions(&shaders.join("forward.slang"), &options, &names, "Node")
        .map_err(|e| e.to_string())?;
    Ok(found.iter().map(node_function).collect())
}

/// A marked function as the node search shows it: the mark's title and group (its name and
/// [`CALL_CATEGORY`] without them), and its signature in a call's spelling or why it cannot
/// be a call.
pub(crate) fn node_function(f: &MarkedFunction) -> NodeFunction {
    NodeFunction {
        name: f.name.clone(),
        title: f
            .arguments
            .first()
            .cloned()
            .unwrap_or_else(|| f.name.clone()),
        category: f
            .arguments
            .get(1)
            .cloned()
            .unwrap_or_else(|| String::from(CALL_CATEGORY)),
        signature: signature(f),
    }
}

/// A Slang type as Slang spells it.
fn spelled(t: &ValueType) -> String {
    match t {
        ValueType::Field(FieldKind::Float(1)) => String::from("float"),
        ValueType::Field(FieldKind::Float(n)) => format!("float{n}"),
        ValueType::Field(FieldKind::Uint(1)) => String::from("uint"),
        ValueType::Field(FieldKind::Uint(n)) => format!("uint{n}"),
        ValueType::Field(FieldKind::Int(1)) => String::from("int"),
        ValueType::Field(FieldKind::Int(n)) => format!("int{n}"),
        ValueType::Field(FieldKind::Texture) => String::from("TextureRef"),
        ValueType::Named(name) => name.clone(),
    }
}

/// `f`'s signature as a call keeps it ([`Signature`]), or why a node cannot call it.
fn signature(f: &MarkedFunction) -> Result<String, String> {
    let width = |t: &ValueType| match t {
        ValueType::Field(FieldKind::Float(n)) => Some(*n),
        _ => None,
    };
    let result = match &f.result {
        ValueType::Named(name) if name == "void" => None,
        t => Some(width(t).ok_or_else(|| {
            format!(
                "it returns a `{}`: a node's outputs are float to float4",
                spelled(t)
            )
        })?),
    };
    let mut context = false;
    let mut parameters = Vec::new();
    for (i, p) in f.parameters.iter().enumerate() {
        if i == 0
            && p.direction == Direction::In
            && matches!(&p.ty, ValueType::Named(name) if name == "MaterialContext")
        {
            context = true;
            continue;
        }
        let w = width(&p.ty).ok_or_else(|| {
            format!(
                "`{}` is a `{}`: a node's ports are float to float4 (and a first \
                 MaterialContext)",
                p.name,
                spelled(&p.ty)
            )
        })?;
        parameters.push(Parameter {
            name: p.name.clone(),
            width: w,
            direction: match p.direction {
                Direction::In => Passed::In,
                Direction::Out => Passed::Out,
                Direction::InOut => Passed::InOut,
            },
        });
    }
    let text = Signature {
        result,
        context,
        parameters,
    }
    .to_string();
    // What a call refuses besides (a port named `out`, nothing given back).
    Signature::parse(&text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_shader::Parameter as Reflected;

    fn parameter(name: &str, ty: ValueType, direction: Direction) -> Reflected {
        Reflected {
            name: name.to_owned(),
            ty,
            direction,
        }
    }

    /// A marked function's node: the mark's title and group, its signature in a call's
    /// spelling with a first `MaterialContext` the fragment's; one a call cannot make says
    /// why (an int port, a texture, a returned int, nothing given back).
    #[test]
    fn a_marked_function_is_a_node_with_its_calls_signature() {
        let context = ValueType::Named(String::from("MaterialContext"));
        let f = MarkedFunction {
            name: String::from("swirl"),
            arguments: vec![String::from("Swirl"), String::from("Mapping")],
            parameters: vec![
                parameter("ctx", context.clone(), Direction::In),
                parameter(
                    "color",
                    ValueType::Field(FieldKind::Float(3)),
                    Direction::In,
                ),
                parameter(
                    "mask",
                    ValueType::Field(FieldKind::Float(1)),
                    Direction::Out,
                ),
            ],
            result: ValueType::Field(FieldKind::Float(3)),
        };
        let node = node_function(&f);
        assert_eq!(node.title, "Swirl");
        assert_eq!(node.category, "Mapping");
        assert_eq!(
            node.signature.as_deref(),
            Ok("float3(MaterialContext ctx, float3 color, out float mask)")
        );
        let bad = |parameters: Vec<Reflected>, result: ValueType| {
            node_function(&MarkedFunction {
                name: String::from("f"),
                arguments: Vec::new(),
                parameters,
                result,
            })
        };
        let float = || ValueType::Field(FieldKind::Float(1));
        let int = bad(
            vec![parameter(
                "n",
                ValueType::Field(FieldKind::Int(1)),
                Direction::In,
            )],
            float(),
        );
        assert!(int.signature.unwrap_err().contains("`n` is a `int`"));
        assert_eq!(int.title, "f");
        assert_eq!(int.category, CALL_CATEGORY);
        let texture = bad(
            vec![parameter(
                "t",
                ValueType::Field(FieldKind::Texture),
                Direction::In,
            )],
            float(),
        );
        assert!(texture.signature.unwrap_err().contains("TextureRef"));
        let returns = bad(Vec::new(), ValueType::Field(FieldKind::Int(2)));
        assert!(returns.signature.unwrap_err().contains("returns a `int2`"));
        let nothing = bad(
            vec![parameter("a", float(), Direction::In)],
            ValueType::Named(String::from("void")),
        );
        assert!(nothing.signature.unwrap_err().contains("nothing back"));
        // A context not first is a port of a type a node has not.
        let late = bad(
            vec![
                parameter("a", float(), Direction::In),
                parameter("ctx", context, Direction::In),
            ],
            float(),
        );
        assert!(late.signature.unwrap_err().contains("MaterialContext"));
    }
}
