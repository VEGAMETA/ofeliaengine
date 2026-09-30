//! Shader build and hot reload benchmark (ADR 0009). Prints Markdown.
//!
//! ```text
//! cargo run --release -p engine_shader --example shader_bench [-- <shader count>]
//! ```
//!
//! Generates a corpus of shaders that share one imported module, then measures Slang
//! session creation, a single compile, "Compile All" at several parallelism levels
//! (cold cache, sessions included), an all-hits rebuild, and hot reload latency from a
//! file write to the new SPIR-V (through the OS watcher on Windows).

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use engine_core::jobs::ThreadPool;
use engine_shader::{
    CompileOptions, Compiler, CompilerPool, HotReloadConfig, ShaderCache, ShaderEvent,
    ShaderHotReload, SlangLibrary, compile_all, find_shader_sources,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn common(variant: u32) -> String {
    format!(
        r#"module common;

public static const float PI = 3.14159265;

public float3 fresnelSchlick(float cosTheta, float3 f0) {{
    return f0 + (1.0 - f0) * pow(clamp(1.0 - cosTheta, 0.0, 1.0), 5.0);
}}

public float distributionGGX(float3 n, float3 h, float roughness) {{
    float a = roughness * roughness;
    float a2 = a * a;
    float d = max(dot(n, h), 0.0);
    float denom = d * d * (a2 - 1.0) + 1.0;
    return a2 / (PI * denom * denom);
}}

public float3 acesTonemap(float3 x) {{
    return saturate((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14)) * {scale}.0;
}}

public float hash(float2 p) {{
    return frac(sin(dot(p, float2(12.9898, 78.233))) * 43758.5453);
}}
"#,
        scale = variant
    )
}

fn shader(index: usize, variant: u32) -> String {
    let lights = 2 + index % 6;
    format!(
        r#"import common;

struct VertexOut {{
    float4 position : SV_Position;
    float3 normal : NORMAL;
    float2 uv : TEXCOORD0;
}};

struct Constants {{
    float4x4 viewProjection;
    float3 cameraPosition;
    float time;
}};

ConstantBuffer<Constants> constants;
Texture2D albedoMap;
SamplerState linearSampler;

[shader("vertex")]
VertexOut vertexMain(float3 position : POSITION, float3 normal : NORMAL, float2 uv : TEXCOORD0) {{
    VertexOut o;
    o.position = mul(constants.viewProjection, float4(position, 1.0));
    o.normal = normal;
    o.uv = uv * {variant}.0;
    return o;
}}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target {{
    float3 n = normalize(input.normal);
    float3 albedo = albedoMap.Sample(linearSampler, input.uv).rgb;
    float3 color = 0;
    for (int i = 0; i < {lights}; i++) {{
        float angle = constants.time + i * 1.7 + {index}.0;
        float3 l = normalize(float3(cos(angle), 1.0, sin(angle)));
        float3 v = normalize(constants.cameraPosition);
        float3 h = normalize(l + v);
        float3 f = fresnelSchlick(max(dot(h, v), 0.0), float3(0.04));
        float d = distributionGGX(n, h, 0.5);
        color += (albedo / PI + f * d * 0.25) * max(dot(n, l), 0.0);
    }}
    color += hash(input.uv) * 0.01;
    return float4(acesTonemap(color), 1.0);
}}
"#
    )
}

fn stats(mut samples: Vec<Duration>) -> String {
    samples.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let p90 = samples[(samples.len() * 9 / 10).min(samples.len() - 1)];
    format!(
        "{:.1} | {:.1} | {:.1}",
        ms(samples[samples.len() / 2]),
        ms(p90),
        ms(*samples.last().unwrap())
    )
}

/// Waits until every file in `remaining` has been compiled; returns the time to the first
/// and to the last of them.
fn wait_compiled(reload: &ShaderHotReload, mut remaining: Vec<PathBuf>) -> (Duration, Duration) {
    let start = Instant::now();
    let mut first = None;
    while !remaining.is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out; still waiting for {} shaders",
            remaining.len()
        );
        match reload.next_timeout(Duration::from_millis(100)) {
            Some(ShaderEvent::Compiled { path, .. }) => {
                first.get_or_insert(start.elapsed());
                remaining.retain(|r| !path.ends_with(r.file_name().unwrap()));
            }
            Some(ShaderEvent::Failed { error, .. }) => panic!("{error}"),
            _ => {}
        }
    }
    (first.unwrap_or_default(), start.elapsed())
}

fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(48);
    let library = SlangLibrary::load().expect("Slang library");
    let dir = std::env::temp_dir().join(format!("engine_shader_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("common.slang"), common(1)).unwrap();
    for i in 0..count {
        std::fs::write(src.join(format!("shader_{i:03}.slang")), shader(i, 1)).unwrap();
    }
    let sources = find_shader_sources(std::slice::from_ref(&src)).unwrap();
    assert_eq!(sources.len(), count);
    let logical = std::thread::available_parallelism().map_or(1, |n| n.get());
    let jobs = Arc::new(ThreadPool::builder().build().unwrap());

    println!("### Shader compilation (Slang {})\n", library.version());
    println!(
        "Corpus: {count} generated shaders (vertex + fragment each) importing one shared module; \
         {logical} logical CPUs. Times in ms.\n"
    );

    // Session creation and one compile in a warm session.
    let mut sessions = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        drop(Compiler::new(Arc::clone(&library)).unwrap());
        sessions.push(start.elapsed());
    }
    let mut compiler = Compiler::new(Arc::clone(&library)).unwrap();
    let mut single = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        compiler
            .compile_file(&sources[0], &[], &CompileOptions::default())
            .unwrap();
        single.push(start.elapsed());
    }
    drop(compiler);
    println!("| Operation | median | p90 | max |\n|---|---|---|---|");
    println!("| create a Slang session | {} |", stats(sessions));
    println!(
        "| compile one shader (warm session, no cache) | {} |",
        stats(single)
    );

    // Compile All with a cold cache and new sessions, as a fresh `ofeliaengine shaders build`.
    println!(
        "\n| Compile All, cold cache (sessions included) | wall | per shader | speed-up |\n|---|---|---|---|"
    );
    let mut levels = vec![1, 2, 4, 8, logical];
    levels.retain(|&j| j <= logical);
    levels.dedup();
    let mut base = None;
    let options = CompileOptions::default();
    for &parallel in &levels {
        let cache = ShaderCache::new(dir.join(format!("cache_j{parallel}"))).unwrap();
        let compilers = CompilerPool::new(Arc::clone(&library));
        let report = compile_all(&jobs, &compilers, &cache, &sources, &options, parallel);
        assert!(report.is_success(), "{report}");
        let wall = report.elapsed.as_secs_f64() * 1e3;
        let base = *base.get_or_insert(wall);
        println!(
            "| {} in parallel | {wall:.0} | {:.1} | {:.2}x |",
            report.parallelism,
            wall / count as f64,
            base / wall
        );
    }
    let cache_dir = dir.join(format!("cache_j{}", levels[0]));
    let cache = ShaderCache::new(&cache_dir).unwrap();
    let compilers = CompilerPool::new(Arc::clone(&library));
    let hits = compile_all(&jobs, &compilers, &cache, &sources, &options, logical);
    assert_eq!(hits.cache_hits(), count);
    println!(
        "| all cache hits ({} in parallel) | {:.1} | {:.2} | – |",
        hits.parallelism,
        hits.elapsed.as_secs_f64() * 1e3,
        hits.elapsed.as_secs_f64() * 1e3 / count as f64
    );

    // Hot reload: edit → new SPIR-V.
    let watch = cfg!(windows);
    let mut config = HotReloadConfig::new(vec![src.clone()], dir.join("cache_reload"));
    config.watch = watch;
    config.max_parallel = logical.min(8);
    let reload = ShaderHotReload::start(Arc::clone(&library), Arc::clone(&jobs), config).unwrap();
    let (_, initial) = wait_compiled(&reload, sources.clone());
    // Let the background thread settle (trim, warm-up) before timing edits.
    let _ = reload.next_timeout(Duration::from_millis(300));

    let edit = |path: &Path, text: String| {
        std::fs::write(path, text).unwrap();
        if !watch {
            reload.notify_changed(path);
        }
    };
    let mut one = Vec::new();
    for round in 0..20u32 {
        let index = round as usize % count;
        let start = Instant::now();
        edit(&sources[index], shader(index, round + 2));
        wait_compiled(&reload, vec![sources[index].clone()]);
        one.push(start.elapsed());
    }
    let (mut shared_first, mut shared_all) = (Vec::new(), Vec::new());
    for round in 0..3u32 {
        let start = Instant::now();
        edit(&src.join("common.slang"), common(round + 2));
        let (first, _) = wait_compiled(&reload, sources.clone());
        shared_first.push(first);
        shared_all.push(start.elapsed());
    }
    println!(
        "\n| Hot reload ({}) | median | p90 | max |\n|---|---|---|---|",
        if watch {
            "file write → OS watcher → new SPIR-V"
        } else {
            "notify → new SPIR-V; no OS watcher on this host"
        }
    );
    println!(
        "| initial build of {count} shaders at start | {:.0} | – | – |",
        initial.as_secs_f64() * 1e3
    );
    println!("| edit one shader | {} |", stats(one));
    println!(
        "| edit the shared module: first of {count} shaders ready | {} |",
        stats(shared_first)
    );
    println!(
        "| edit the shared module: all {count} shaders ready | {} |",
        stats(shared_all)
    );
    println!("\nBudget (CLAUDE.md §5): shader edit → on screen ≤ 500 ms.");
    drop(reload);
    let _ = std::fs::remove_dir_all(&dir);
}
