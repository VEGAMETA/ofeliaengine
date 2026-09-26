//! "Compile All Shaders" and hot reload (see `common` for when they run).

#![allow(clippy::print_stdout, clippy::unwrap_used)]

mod common;

use common::{slang, temp_dir};
use engine_core::jobs::ThreadPool;
use engine_shader::{
    CacheOutcome, CompileOptions, CompilerPool, HotReloadConfig, ShaderCache, ShaderEvent,
    ShaderHotReload, ShaderSet, compile_all, find_shader_sources,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn compute(body: &str) -> String {
    format!(
        "RWStructuredBuffer<uint> output;\n[shader(\"compute\")]\n[numthreads(1,1,1)]\nvoid main() {{ {body} }}\n"
    )
}

fn importing(module: &str, body: &str) -> String {
    format!("import {module};\n{}", compute(body))
}

fn module(name: &str, value: u32) -> String {
    format!("module {name};\npublic uint value() {{ return {value}; }}\n")
}

/// Rewrites a file so that it is newer (some file systems have coarse timestamps; the
/// cache uses content hashes, so only the bytes matter).
fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

fn names(events: &[ShaderEvent]) -> Vec<String> {
    let mut names: Vec<String> = events
        .iter()
        .map(|e| {
            let (kind, path) = match e {
                ShaderEvent::Compiled { path, .. } => ("compiled", path),
                ShaderEvent::Failed { path, .. } => ("failed", path),
                ShaderEvent::Removed { path } => ("removed", path),
            };
            format!("{kind} {}", path.file_name().unwrap().to_string_lossy())
        })
        .collect();
    names.sort();
    names
}

#[test]
fn compile_all_reports_counts_errors_and_cache_hits() {
    let Some(library) = slang() else { return };
    let dir = temp_dir("build");
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("post")).unwrap();
    write(&src.join("common.slang"), &module("common", 1));
    write(
        &src.join("a.slang"),
        &importing("common", "output[0] = value();"),
    );
    write(
        &src.join("post").join("b.slang"),
        &compute("output[0] = 2;"),
    );
    write(&src.join("broken.slang"), &compute("output[0] = nope;"));

    let jobs = ThreadPool::builder().workers(3).build().unwrap();
    let compilers = CompilerPool::new(library);
    let cache = ShaderCache::new(dir.join("cache")).unwrap();
    let sources = find_shader_sources(&[src]).unwrap();
    assert_eq!(sources.len(), 3, "{sources:?}");
    let options = CompileOptions::default();

    let report = compile_all(&jobs, &compilers, &cache, &sources, &options, 8);
    println!("{report}");
    assert_eq!(report.units.len(), 3);
    assert_eq!(report.entry_points(), 2);
    assert_eq!(report.compiled(), 2);
    assert_eq!(report.cache_hits(), 0);
    let failures: Vec<_> = report.failures().collect();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].0.ends_with("broken.slang"));
    let message = report.to_string();
    assert!(message.contains("broken.slang:4"), "{message}");
    assert!(!report.is_success());
    // Results keep the order of `sources`.
    for (unit, source) in report.units.iter().zip(&sources) {
        assert_eq!(&unit.path, source);
    }
    // No more sessions than files compiled concurrently.
    assert!(compilers.idle_count() <= report.parallelism);

    let again = compile_all(&jobs, &compilers, &cache, &sources, &options, 8);
    println!("{again}");
    assert_eq!(again.cache_hits(), 2);
    assert_eq!(again.compiled(), 0);
}

#[test]
fn shader_set_rebuilds_exactly_what_changed() {
    let Some(library) = slang() else { return };
    let dir = temp_dir("set");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let common = src.join("common.slang");
    let a = src.join("a.slang");
    let b = src.join("b.slang");
    write(&common, &module("common", 1));
    write(&a, &importing("common", "output[0] = value();"));
    write(&b, &compute("output[0] = 2;"));

    let jobs = ThreadPool::builder().workers(2).build().unwrap();
    let compilers = CompilerPool::new(library);
    let cache = ShaderCache::new(dir.join("cache")).unwrap();
    let mut set = ShaderSet::new(vec![src.clone()], CompileOptions::default());
    let rebuild = |set: &mut ShaderSet| set.rebuild(&jobs, &compilers, &cache, 4);

    assert_eq!(
        names(&rebuild(&mut set)),
        ["compiled a.slang", "compiled b.slang"]
    );
    assert_eq!(set.len(), 2);
    assert!(!set.has_pending_work());
    assert!(rebuild(&mut set).is_empty());

    // An imported module: only its importer is rebuilt.
    write(&common, &module("common", 5));
    set.changed(&common);
    assert_eq!(names(&rebuild(&mut set)), ["compiled a.slang"]);

    // Saving without changes produces no event.
    write(&b, &compute("output[0] = 2;"));
    set.changed(&b);
    assert!(rebuild(&mut set).is_empty());

    // A broken edit keeps the last good output.
    let good = Arc::clone(set.output(&a).unwrap());
    write(&a, &importing("common", "output[0] = value() +;"));
    set.changed(&a);
    let events = rebuild(&mut set);
    assert_eq!(names(&events), ["failed a.slang"]);
    if let ShaderEvent::Failed { error, .. } = &events[0] {
        println!("{error}");
    }
    assert!(Arc::ptr_eq(set.output(&a).unwrap(), &good));

    // Fixing it back to the same code still reports it (the error must be cleared).
    write(&a, &importing("common", "output[0] = value();"));
    set.changed(&a);
    let events = rebuild(&mut set);
    assert_eq!(names(&events), ["compiled a.slang"]);
    if let ShaderEvent::Compiled { outcome, .. } = &events[0] {
        assert_eq!(*outcome, CacheOutcome::Hit);
    }

    // A shader importing a module that does not exist yet recovers once it appears.
    let c = src.join("c.slang");
    write(&c, &importing("extra", "output[0] = value();"));
    set.changed(&c);
    assert_eq!(names(&rebuild(&mut set)), ["failed c.slang"]);
    let extra = src.join("extra.slang");
    write(&extra, &module("extra", 3));
    set.changed(&extra);
    assert_eq!(names(&rebuild(&mut set)), ["compiled c.slang"]);

    // Deleting a shader, and a shader losing its entry points.
    std::fs::remove_file(&b).unwrap();
    set.changed(&b);
    write(&c, &module("c", 1));
    set.changed(&c);
    assert_eq!(
        names(&rebuild(&mut set)),
        ["removed b.slang", "removed c.slang"]
    );
    assert_eq!(set.len(), 1);

    // A new directory with shaders in it is found by the rescan it triggers.
    let nested = src.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    write(&nested.join("d.slang"), &compute("output[0] = 4;"));
    set.changed(&nested);
    assert_eq!(names(&rebuild(&mut set)), ["compiled d.slang"]);

    // Files outside the roots are ignored.
    let outside = dir.join("outside.slang");
    write(&outside, &compute("output[0] = 9;"));
    set.changed(&outside);
    assert!(rebuild(&mut set).is_empty());
}

fn wait_for(
    reload: &ShaderHotReload,
    what: &str,
    mut done: impl FnMut(&ShaderEvent) -> bool,
) -> Duration {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if let Some(event) = reload.next_timeout(Duration::from_millis(50)) {
            if done(&event) {
                return start.elapsed();
            }
            println!("(other event: {})", names(std::slice::from_ref(&event))[0]);
        }
    }
    panic!("timed out waiting for {what}");
}

fn is_compiled(path: &Path) -> impl FnMut(&ShaderEvent) -> bool + '_ {
    move |e| matches!(e, ShaderEvent::Compiled { path: p, .. } if p.ends_with(path.file_name().unwrap()))
}

#[test]
fn hot_reload_thread_rebuilds_on_change() {
    let Some(library) = slang() else { return };
    let dir = temp_dir("hot");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let common = src.join("common.slang");
    let a = src.join("a.slang");
    write(&common, &module("common", 1));
    write(&a, &importing("common", "output[0] = value();"));

    // OS watchers on Windows; elsewhere the test reports changes itself.
    let watch = cfg!(windows);
    let jobs = Arc::new(ThreadPool::builder().workers(2).build().unwrap());
    let mut config = HotReloadConfig::new(vec![src.clone()], dir.join("cache"));
    config.watch = watch;
    let reload = ShaderHotReload::start(library, jobs, config).unwrap();
    let notify = |path: &PathBuf| {
        if !watch {
            reload.notify_changed(path);
        }
    };

    let initial = wait_for(&reload, "initial build", is_compiled(&a));
    println!("initial build: {initial:?}");

    let mut latencies = Vec::new();
    for value in 2..7 {
        let start = Instant::now();
        write(&common, &module("common", value));
        notify(&common);
        wait_for(&reload, "rebuild after edit", is_compiled(&a));
        latencies.push(start.elapsed());
    }
    println!("edit → new SPIR-V (watch={watch}): {latencies:?}");

    write(&a, &importing("common", "output[0] = value() +;"));
    notify(&a);
    wait_for(&reload, "failure", |e| {
        matches!(e, ShaderEvent::Failed { .. })
    });

    let b = src.join("b.slang");
    write(&b, &compute("output[0] = 1;"));
    notify(&b);
    wait_for(&reload, "new shader", is_compiled(&b));
    drop(reload);
    let _ = std::fs::remove_dir_all(&dir);
}
