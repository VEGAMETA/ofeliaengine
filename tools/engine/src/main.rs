//! The `ofeliaengine` command line (CLAUDE.md §9 "one button", §2 `/tools`).
//!
//! ```text
//! ofeliaengine shaders build [OPTIONS] <ROOT>...   compile every shader under the roots
//! ofeliaengine shaders watch [OPTIONS] <ROOT>...   rebuild on every change (Windows)
//! ofeliaengine cook <SCENE.gltf> -o <OUT.pack>     cook a scene into a pack (ADR 0056), its
//!                                                  `<SCENE>.materials.ron` and `<SCENE>.pso.ron`
//!                                                  applied (ADR 0092), its colliders baked
//!                                                  into Jolt's binary form (ADR 0101)
//! ```
//!
//! Run `ofeliaengine help` for the options.

// A command-line tool reports on stdout/stderr.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use engine_assets::gltf_models::apply_gltf_models;
use engine_assets::material_file::{MaterialMap, SlangMaterialTypes, apply_material_map};
use engine_assets::pso_record::PsoRecord;
use engine_assets::{CookSettings, DerivedDataCache, cook_scene, import_gltf, write_pack};
use engine_core::jobs::ThreadPool;
use engine_physics::PhysicsSettings;
use engine_physics_jolt::JoltBackend;
use engine_shader::{
    CompileOptions, CompilerPool, HotReloadConfig, ShaderCache, ShaderEvent, ShaderHotReload,
    SlangLibrary, compile_all, find_shader_sources,
};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

const USAGE: &str = "\
usage:
  ofeliaengine shaders build [OPTIONS] <ROOT>...   compile every shader under the roots
  ofeliaengine shaders watch [OPTIONS] <ROOT>...   compile, then rebuild on every change (Windows)
  ofeliaengine cook <SCENE.gltf> -o <OUT.pack> [--ddc <DIR>] [--no-lods] [--no-meshlets]
                    [--no-colliders] [--level <N>] cook a scene into a pack: textures compressed
                                                   with mips (through the derived data cache,
                                                   default <scene dir>/../.ddc), meshes with
                                                   detail levels and meshlets, entries zstd
                                                   level N (19);
                                                   <SCENE>.materials.ron (or --materials <MAP>)
                                                   replaces glTF materials by material files,
                                                   their types compiled into the pack
                                                   (--engine-shaders <DIR>: forward.slang);
                                                   <SCENE>.pso.ron (or --pso <RECORD>): a PSO
                                                   record, only the entry points playtests
                                                   drew compiled for the types it names;
                                                   colliders from its meshes and glTF physics,
                                                   baked for Jolt (--no-colliders: none)

A shader is a .slang file with at least one [shader(\"<stage>\")] entry point; other
.slang files are modules, compiled through the shaders that import them.

options:
  --cache <DIR>        shader cache directory (default: target/shader-cache)
  -O, --optimize       optimize the SPIR-V
  -g, --debug-info     emit debug info (Slang source in RenderDoc/Nsight)
  -D <NAME[=VALUE]>    preprocessor define (repeatable)
  -I <DIR>             import/include directory (repeatable)
  -j <N>               most files compiled at once; each needs a ~150 MB Slang session
                       (default: logical CPUs, at most 8)
  --out <DIR>          build: also write <file>.<entry>.spv per entry point to DIR

exit status: 0 success, 1 shader errors, 2 usage or setup error";

enum Command {
    Shaders(Args),
    Cook(CookArgs),
}

struct CookArgs {
    scene: PathBuf,
    out: PathBuf,
    ddc: Option<PathBuf>,
    settings: CookSettings,
    materials: Option<PathBuf>,
    engine_shaders: PathBuf,
    pso: Option<PathBuf>,
}

/// The engine's shader folder in this repository (`forward.slang`, which material types
/// compile from).
const ENGINE_SHADERS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/engine_render/shaders"
);

fn parse_command(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    match args.next().as_deref() {
        Some("shaders") => parse(args).map(Command::Shaders),
        Some("cook") => parse_cook(args).map(Command::Cook),
        Some("help" | "-h" | "--help") | None => Err(String::new()),
        Some(other) => Err(format!("unknown command `{other}`")),
    }
}

fn parse_cook(mut args: impl Iterator<Item = String>) -> Result<CookArgs, String> {
    let (mut scene, mut out, mut ddc, mut materials, mut pso) = (None, None, None, None, None);
    let mut engine_shaders = PathBuf::from(ENGINE_SHADERS);
    let mut settings = CookSettings::default();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-o" | "--out" => out = Some(PathBuf::from(value("-o")?)),
            "--ddc" => ddc = Some(PathBuf::from(value("--ddc")?)),
            "--materials" => materials = Some(PathBuf::from(value("--materials")?)),
            "--engine-shaders" => engine_shaders = PathBuf::from(value("--engine-shaders")?),
            "--pso" => pso = Some(PathBuf::from(value("--pso")?)),
            "--no-lods" => settings.lods = false,
            "--no-meshlets" => settings.meshlets = false,
            "--no-colliders" => settings.colliders = false,
            "--level" => {
                let level = value("--level")?;
                settings.compression_level = level
                    .parse()
                    .ok()
                    .filter(|l| (0..=22).contains(l))
                    .ok_or_else(|| format!("--level {level}: 0 to 22"))?;
            }
            other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
            path if scene.is_none() => scene = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected `{extra}`")),
        }
    }
    Ok(CookArgs {
        scene: scene.ok_or("no scene given")?,
        out: out.ok_or("no output given (-o <OUT.pack>)")?,
        ddc,
        settings,
        materials,
        engine_shaders,
        pso,
    })
}

fn cook(args: &CookArgs) -> Result<ExitCode, String> {
    let start = Instant::now();
    let bytes = std::fs::read(&args.scene).map_err(|e| format!("{}: {e}", args.scene.display()))?;
    let dir = args.scene.parent();
    let mut scene =
        import_gltf(&bytes, dir).map_err(|e| format!("{}: {e}", args.scene.display()))?;
    // Material files (ADR 0092): the map next to the scene, or the one given.
    let map = match &args.materials {
        Some(path) => {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            MaterialMap::parse(&text, path.parent().unwrap_or(std::path::Path::new("")))
                .map_err(|e| format!("{}: {e}", path.display()))?
        }
        None => MaterialMap::for_scene(&args.scene).map_err(|e| e.to_string())?,
    };
    // glTF's unlit, clear coat and sheen: materials of the engine's built-in types.
    let layered = scene.materials.iter().any(|m| m.layers.any());
    if !map.files.is_empty() || layered {
        let types_start = Instant::now();
        let mut builder = SlangMaterialTypes::new(&args.engine_shaders)?;
        // A PSO record (ADR 0092): only the entry points playtests drew, for the types it
        // names. The one given, or the one next to the scene.
        let pso_path = args
            .pso
            .clone()
            .unwrap_or_else(|| PsoRecord::path_for(&args.scene));
        let record = PsoRecord::load(&pso_path).map_err(|e| e.to_string())?;
        if record.is_empty() {
            if args.pso.is_some() {
                return Err(format!("{}: no PSO record there", pso_path.display()));
            }
        } else {
            println!(
                "{}: a PSO record of {} entry points of {} types",
                pso_path.display(),
                record.len(),
                record.types.len()
            );
            builder = builder.with_pso_record(record);
        }
        if !map.files.is_empty() {
            let warnings =
                apply_material_map(&mut scene, &map, &mut builder).map_err(|e| e.to_string())?;
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
        }
        let from_gltf = apply_gltf_models(&mut scene, &mut builder).map_err(|e| e.to_string())?;
        println!(
            "{}: {} materials of {} types from material files and glTF's extensions ({from_gltf} from glTF's; {} entry points), compiled in {:.1} s",
            args.scene.display(),
            scene.typed_materials.len(),
            scene.material_types.len(),
            scene
                .material_types
                .iter()
                .map(|t| t.entries.len())
                .sum::<usize>(),
            types_start.elapsed().as_secs_f64(),
        );
    }
    let ddc_dir = args.ddc.clone().unwrap_or_else(|| {
        dir.and_then(std::path::Path::parent)
            .unwrap_or(std::path::Path::new("."))
            .join(".ddc")
    });
    let cache = DerivedDataCache::new(&ddc_dir)
        .map_err(|e| format!("derived data cache {}: {e}", ddc_dir.display()))?;
    let jobs = ThreadPool::builder()
        .thread_name("engine-cook")
        .build()
        .map_err(|e| format!("cannot start worker threads: {e:?}"))?;
    let (mut cooked, stats) = cook_scene(&scene, dir, Some(&cache), &jobs, &args.settings)
        .map_err(|e| format!("{}: {e}", args.scene.display()))?;
    // Colliders in Jolt's binary form: restoring them at load is a copy (ADR 0101).
    let bake_start = Instant::now();
    {
        // In parallel, a physics world per collider (shapes do not need one; Jolt's shape
        // building does not share state between worlds).
        let baked: Vec<
            std::sync::OnceLock<Result<Option<engine_physics::colliders::BakedCollider>, String>>,
        > = cooked
            .colliders
            .iter()
            .map(|_| std::sync::OnceLock::new())
            .collect();
        let colliders = &cooked.colliders;
        jobs.parallel_for(colliders.len(), 1, |_, range| {
            for k in range {
                let result = JoltBackend::new(&PhysicsSettings::default())
                    .map_err(|e| e.to_string())
                    .and_then(|mut physics| {
                        engine_physics::colliders::bake(&mut physics, &colliders[k].collider)
                            .map_err(|e| e.to_string())
                    });
                let _ = baked[k].set(result);
            }
        });
        for (c, b) in cooked.colliders.iter_mut().zip(baked) {
            let b = b
                .into_inner()
                .unwrap_or_else(|| Err("not baked".to_owned()))
                .map_err(|e| format!("{}: a collider: {e}", args.scene.display()))?;
            c.baked = b.map(|b| engine_assets::colliders::BakedCollider {
                format: b.format,
                bytes: b.bytes,
                castable: b.castable,
            });
        }
    }
    let baked_in = bake_start.elapsed();
    let cooked_at = start.elapsed();
    let written = write_pack(&args.out, &cooked, &jobs, args.settings.compression_level)
        .map_err(|e| format!("{}: {e}", args.out.display()))?;
    println!(
        "{}: {} textures ({} from the derived data cache), {} meshes ({} meshlets), {} materials, {} instances; cooked in {:.1} s",
        args.scene.display(),
        cooked.textures.len(),
        stats.cache_hits,
        cooked.meshes.len(),
        stats.meshlets,
        cooked.materials.len(),
        cooked.instances.len(),
        cooked_at.as_secs_f64(),
    );
    if !cooked.colliders.is_empty() {
        println!(
            "{}: {} colliders, {} bodies; made in {:.2} s ({} meshes in convex parts, {} from the cache), baked in {:.2} s",
            args.scene.display(),
            cooked.colliders.len(),
            cooked.bodies.len(),
            stats.collider_seconds,
            stats.colliders.decomposed,
            stats.colliders.cached,
            baked_in.as_secs_f64(),
        );
    }
    println!(
        "{}: {} entries, {:.1} MB ({:.1} MB raw, {:.0}%), written in {:.1} s",
        args.out.display(),
        written.entries,
        written.file_bytes as f64 / 1e6,
        written.bytes as f64 / 1e6,
        100.0 * written.stored_bytes as f64 / written.bytes.max(1) as f64,
        (start.elapsed() - cooked_at).as_secs_f64(),
    );
    Ok(ExitCode::SUCCESS)
}

struct Args {
    command: String,
    roots: Vec<PathBuf>,
    cache: PathBuf,
    options: CompileOptions,
    parallel: usize,
    out: Option<PathBuf>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let command = match args.next().as_deref() {
        Some(c @ ("build" | "watch")) => c.to_owned(),
        Some(other) => return Err(format!("unknown shaders command `{other}`")),
        None => return Err(String::from("missing `build` or `watch`")),
    };
    let default_parallel = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(8);
    let mut parsed = Args {
        command,
        roots: Vec::new(),
        cache: PathBuf::from("target/shader-cache"),
        options: CompileOptions::default(),
        parallel: default_parallel,
        out: None,
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--cache" => parsed.cache = value("--cache")?.into(),
            "-O" | "--optimize" => parsed.options.optimize = true,
            "-g" | "--debug-info" => parsed.options.debug_info = true,
            "-D" => {
                let define = value("-D")?;
                let (name, val) = define.split_once('=').unwrap_or((&define, "1"));
                parsed
                    .options
                    .defines
                    .push((name.to_owned(), val.to_owned()));
            }
            "-I" => parsed.options.include_dirs.push(value("-I")?.into()),
            "--out" => parsed.out = Some(value("--out")?.into()),
            "-j" => {
                parsed.parallel = value("-j")?
                    .parse::<usize>()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or("-j needs a positive number")?;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option `{flag}`")),
            root => parsed.roots.push(root.into()),
        }
    }
    if parsed.roots.is_empty() {
        return Err(String::from("no shader directory given"));
    }
    Ok(parsed)
}

/// The engine's global allocator (ADR 0086).
#[global_allocator]
static ALLOCATOR: engine_core::alloc::EngineAllocator = engine_core::alloc::EngineAllocator::new();

fn main() -> ExitCode {
    // The crash monitor is this executable started by `engine_crash::install`.
    if engine_crash::run_monitor_if_requested() {
        return ExitCode::SUCCESS;
    }
    // Log records of the engine crates (warnings and errors unless ENGINE_LOG says
    // otherwise) go to stderr; crashes and panics leave a minidump and a report (ADR 0061).
    let _log = engine_log::init(&engine_log::LogConfig {
        filter: Some(std::env::var("ENGINE_LOG").unwrap_or_else(|_| String::from("warn"))),
        ..engine_log::LogConfig::default()
    });
    let _crash = engine_crash::install(&engine_crash::CrashConfig::new("ofeliaengine"))
        .inspect_err(|e| eprintln!("warning: {e}"));
    let command = match parse_command(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("error: {message}\n");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match &command {
        Command::Shaders(args) => run(args),
        Command::Cook(args) => cook(args),
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &Args) -> Result<ExitCode, String> {
    if args.command == "watch" && !cfg!(windows) {
        return Err(String::from(
            "watching needs Windows directory notifications; use `build` here",
        ));
    }
    let library = SlangLibrary::load().map_err(|e| e.to_string())?;
    println!("Slang {} ({})", library.version(), library.path().display());
    let jobs = ThreadPool::builder()
        .thread_name("engine-shader-build")
        .build()
        .map_err(|e| format!("cannot start worker threads: {e:?}"))?;
    match args.command.as_str() {
        "watch" => watch(args, library, jobs),
        _ => build(args, library, &jobs),
    }
}

fn build(args: &Args, library: Arc<SlangLibrary>, jobs: &ThreadPool) -> Result<ExitCode, String> {
    let start = Instant::now();
    let sources = find_shader_sources(&args.roots).map_err(|e| e.to_string())?;
    let cache = ShaderCache::new(&args.cache).map_err(|e| e.to_string())?;
    let compilers = CompilerPool::new(library);
    let report = compile_all(
        jobs,
        &compilers,
        &cache,
        &sources,
        &args.options,
        args.parallel,
    );
    for unit in &report.units {
        let status = match &unit.result {
            Ok((output, outcome)) => format!("{} entry points, {outcome:?}", output.entries.len()),
            Err(_) => String::from("FAILED"),
        };
        println!(
            "  {:>8.1} ms  {}  ({status})",
            unit.time.as_secs_f64() * 1e3,
            unit.path.display()
        );
    }
    if let Some(out) = &args.out {
        std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
        for unit in &report.units {
            let Ok((output, _)) = &unit.result else {
                continue;
            };
            let stem = unit
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            for entry in &output.entries {
                let bytes: Vec<u8> = entry.spirv.iter().flat_map(|w| w.to_le_bytes()).collect();
                let file = out.join(format!("{stem}.{}.spv", entry.name));
                std::fs::write(&file, bytes).map_err(|e| format!("{}: {e}", file.display()))?;
                println!("  wrote {}", file.display());
            }
        }
    }
    println!("{report}");
    println!("total {:.2} s", start.elapsed().as_secs_f64());
    Ok(if report.is_success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn watch(args: &Args, library: Arc<SlangLibrary>, jobs: ThreadPool) -> Result<ExitCode, String> {
    let mut config = HotReloadConfig::new(args.roots.clone(), &args.cache);
    config.options = args.options.clone();
    config.max_parallel = args.parallel;
    let reload =
        ShaderHotReload::start(library, Arc::new(jobs), config).map_err(|e| e.to_string())?;
    println!("watching {:?}; Ctrl+C to stop", args.roots);
    loop {
        let Some(event) = reload.next_timeout(std::time::Duration::from_secs(3600)) else {
            continue;
        };
        match event {
            ShaderEvent::Compiled {
                path,
                output,
                outcome,
                time,
            } => println!(
                "compiled {} ({} entry points, {outcome:?}) in {:.1} ms",
                path.display(),
                output.entries.len(),
                time.as_secs_f64() * 1e3
            ),
            ShaderEvent::Failed { error, time, .. } => {
                println!("{error}\n(failed in {:.1} ms)", time.as_secs_f64() * 1e3);
            }
            ShaderEvent::Removed { path } => println!("removed {}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Result<Args, String> {
        match parse_command(line.split_whitespace().map(str::to_owned))? {
            Command::Shaders(args) => Ok(args),
            Command::Cook(_) => Err(String::from("not a shaders command")),
        }
    }

    fn cook_args(line: &str) -> Result<CookArgs, String> {
        match parse_command(line.split_whitespace().map(str::to_owned))? {
            Command::Cook(args) => Ok(args),
            Command::Shaders(_) => Err(String::from("not a cook command")),
        }
    }

    #[test]
    fn parses_cook_options() {
        let parsed =
            cook_args("cook a/scene.gltf -o out.pack --ddc d --no-lods --no-meshlets --level 3")
                .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(parsed.scene, PathBuf::from("a/scene.gltf"));
        assert_eq!(parsed.out, PathBuf::from("out.pack"));
        assert_eq!(parsed.ddc, Some(PathBuf::from("d")));
        assert!(!parsed.settings.lods && !parsed.settings.meshlets);
        assert_eq!(parsed.settings.compression_level, 3);
        let defaults = cook_args("cook s.gltf -o o.pack").unwrap_or_else(|e| panic!("{e}"));
        assert!(defaults.settings.lods && defaults.settings.meshlets);
        assert!(defaults.settings.block_compression);
        assert_eq!(defaults.settings.compression_level, 19);
        assert!(cook_args("cook s.gltf").is_err());
        assert!(cook_args("cook -o o.pack").is_err());
        assert!(cook_args("cook s.gltf -o o.pack --level 30").is_err());
        assert!(cook_args("cook s.gltf -o o.pack extra").is_err());
    }

    #[test]
    fn parses_build_options() {
        let parsed = args("shaders build -O -g -D FANCY -D N=3 -I inc -j 2 --cache c --out o a b")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(parsed.command, "build");
        assert_eq!(parsed.roots, [PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(parsed.cache, PathBuf::from("c"));
        assert!(parsed.options.optimize && parsed.options.debug_info);
        assert_eq!(
            parsed.options.defines,
            [
                ("FANCY".to_owned(), "1".to_owned()),
                ("N".to_owned(), "3".to_owned())
            ]
        );
        assert_eq!(parsed.options.include_dirs, [PathBuf::from("inc")]);
        assert_eq!(parsed.parallel, 2);
        assert_eq!(parsed.out, Some(PathBuf::from("o")));
    }

    #[test]
    fn rejects_bad_usage() {
        assert!(args("").is_err());
        assert!(args("shaders").is_err());
        assert!(args("shaders build").is_err());
        assert!(args("shaders build -j 0 a").is_err());
        assert!(args("shaders build --bogus a").is_err());
        assert!(args("shaders compile a").is_err());
        assert!(args("shaders build -D").is_err());
    }
}
