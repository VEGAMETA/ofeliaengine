//! The `engine` command line (CLAUDE.md §9 "one button", §2 `/tools`).
//!
//! ```text
//! engine shaders build [OPTIONS] <ROOT>...   compile every shader under the roots
//! engine shaders watch [OPTIONS] <ROOT>...   rebuild on every change (Windows)
//! ```
//!
//! Run `engine help` for the options.

// A command-line tool reports on stdout/stderr.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use engine_core::jobs::ThreadPool;
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
  engine shaders build [OPTIONS] <ROOT>...   compile every shader under the roots
  engine shaders watch [OPTIONS] <ROOT>...   compile, then rebuild on every change (Windows)

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

exit status: 0 success, 1 shader errors, 2 usage or setup error";

struct Args {
    command: String,
    roots: Vec<PathBuf>,
    cache: PathBuf,
    options: CompileOptions,
    parallel: usize,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    match args.next().as_deref() {
        Some("shaders") => {}
        Some("help" | "-h" | "--help") | None => return Err(String::new()),
        Some(other) => return Err(format!("unknown command `{other}`")),
    }
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

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("error: {message}\n");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
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
        parse(line.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn parses_build_options() {
        let parsed = args("shaders build -O -g -D FANCY -D N=3 -I inc -j 2 --cache c a b")
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
