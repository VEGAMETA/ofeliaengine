//! `perf_gate [--update] [--strict] [--runs N] [--frames N] [--threshold PCT]
//! [--scene NAME]...`: GPU time of every pass of fixed scenes at 1920 × 1080, against this
//! GPU vendor's baseline in `tests/golden/perf/<vendor>.tsv` (ADR 0062, CLAUDE.md §5).
//!
//! - Each scene is rendered `--runs` times (3) from a fresh device, `--frames` timed
//!   frames each (100: the same medians as 200 within the runs' noise, in half the
//!   Radeon's time); a pass's time is the smallest of its per-run medians (interference
//!   from other work on the machine, and clocks that did not ramp up, only add time). The
//!   runs go a round at a time, the first of every scene, then the second: a slow spell
//!   of the machine (work on the CPU next to the Radeon iGPU, other work on a GPU) then
//!   slows one run of a few scenes instead of every run of one.
//! - A scene with a pass over the threshold is measured again, `--runs` more runs, and
//!   judged by those: a slow spell has passed by then, a regression has not.
//! - A pass regresses when it is slower than its baseline by more than `--threshold`
//!   percent (3) and by more than 5 µs or 0.5 % of its scene's frame, whichever is more
//!   (small passes are noisy, and a few µs of a 12 ms frame matter to nobody); the
//!   frame's total too, and `(gpu frame)`, the frame end to end on the GPU (with async
//!   compute, ADR 0065, passes overlap: an overlapped pass may read longer while the frame
//!   gets shorter).
//! - When every large pass of a scene moved by about the frame's percentage, the GPU ran
//!   at other clocks or shared with other work: reported as such, and not counted without
//!   `--strict`.
//! - Without `--strict` regressions are reported, not fatal: ref-1 is not a dedicated
//!   benchmark machine (docs/perf/reference-hw.md).
//! - `--update` writes the measured times as the new baseline.
//! - `--scene NAME` (repeatable) measures only those scenes (comparisons, A/B runs; not
//!   with `--update`).
//!
//! Run it in release with the implicit layers off (`VK_LOADER_LAYERS_DISABLE=~implicit~`,
//! overlays change timing) and `ENGINE_ADAPTER=radeon` for the iGPU.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use engine_core::math::Vec3;
use engine_frame::{Antialiasing, Msaa, Quality, Upscale};
use engine_golden::scene::{
    RenderOptions, Shot, Skip, Source, procedural_lights, render, sponza_lights, vendor_name,
};
use engine_render::ao::AoSettings;
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::reflections::SsrSettings;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

const TOTAL: &str = "(frame total)";
/// The frame end to end on the GPU (ADR 0065): with async compute, passes overlap and
/// their sum is not the frame's time.
const FRAME: &str = "(gpu frame)";
const FLOOR_US: f64 = 5.0;
const FLOOR_OF_FRAME: f64 = 0.005;
/// Passes of at least this share of the frame decide whether a scene moved as a whole.
const UNIFORM_SHARE: f64 = 0.05;
/// ... when each moved within this many percentage points of the frame.
const UNIFORM_SPREAD: f64 = 2.0;

fn shots() -> Vec<Shot> {
    vec![
        Shot {
            size: [1920, 1080],
            eye: Some(Vec3::new(0.4, 1.9, 4.8)),
            target: Some(Vec3::new(0.0, 1.0, -1.0)),
            lights: procedural_lights(),
            ..Shot::new("procedural", Source::Procedural)
        },
        Shot {
            size: [1920, 1080],
            ..Shot::new("sponza", Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")))
        },
        // Ambient occlusion, which runs on the async compute queue where that is the
        // default (ADR 0065).
        Shot {
            size: [1920, 1080],
            ao: Some(AoSettings::default()),
            ..Shot::new(
                "sponza_ao",
                Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
            )
        },
        // Shadows of local lights (ADR 0071): in steady state every shadow is cached, so
        // this measures what looking them up costs the shading.
        Shot {
            size: [1920, 1080],
            lights: sponza_lights(),
            local_shadows: true,
            ..Shot::new(
                "sponza_local_shadows",
                Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
            )
        },
        // The physically based sky and height fog (ADR 0069): the atmosphere's full-screen
        // pass and aerial perspective every frame (its tables only in the first).
        Shot {
            size: [1920, 1080],
            atmosphere: Some(Atmosphere::earth()),
            fog: Some(HeightFog::default()),
            ..Shot::new(
                "sponza_atmosphere",
                Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
            )
        },
        // Temporal antialiasing and upscaling through the frame renderer (ADR 0072): TAA at
        // the output size, TAAU and FSR 3.1 from 67 % (motion vectors, the resolve, the
        // upscaler's passes; the post chain at the output size).
        antialiased("sponza_taa", Antialiasing::TAAU, Quality::Native),
        antialiased("sponza_taau_67", Antialiasing::TAAU, Quality::Quality),
        antialiased("sponza_fsr_quality", Antialiasing::FSR, Quality::Quality),
        // The other antialiasing and scaling modes (ADR 0075): MSAA 4× (the depth prepass,
        // multisampled shading, the resolves), the filters after tone mapping (FXAA, SMAA
        // 1x, SMAA T2x with its temporal resolve), FSR 1 from 67 %.
        antialiased(
            "sponza_msaa4",
            Antialiasing::msaa(Msaa::X4),
            Quality::Native,
        ),
        antialiased("sponza_fxaa", Antialiasing::FXAA, Quality::Native),
        antialiased("sponza_smaa", Antialiasing::SMAA_1X, Quality::Native),
        antialiased("sponza_smaa_t2x", Antialiasing::SMAA_T2X, Quality::Native),
        antialiased(
            "sponza_fsr1_67",
            Antialiasing::spatial(Upscale::Fsr1),
            Quality::Quality,
        ),
        // Local reflections (ADR 0073): six cached probes over the atrium and screen-space
        // reflections at half resolution (their passes every frame, no captures).
        Shot {
            size: [1920, 1080],
            probe_grid: Some([3, 1, 2]),
            ssr: Some(SsrSettings::default()),
            ..Shot::new(
                "sponza_reflections",
                Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
            )
        },
        // Textures streamed by texture feedback within an ample budget (ADR 0076): the
        // same frame as `sponza` plus the feedback pass (a quarter of the commands each
        // frame) and the streamer's steady state.
        Shot {
            size: [1920, 1080],
            texture_budget: Some(4 << 30),
            ..Shot::new(
                "sponza_streamed",
                Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
            )
        },
    ]
}

fn antialiased(name: &'static str, antialiasing: Antialiasing, quality: Quality) -> Shot {
    Shot {
        size: [1920, 1080],
        antialiasing,
        quality,
        ..Shot::new(name, Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")))
    }
}

fn baseline_path(vendor: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("perf")
        .join(format!("{vendor}.tsv"))
}

/// `scene \t pass \t µs` lines.
fn read_baseline(path: &Path) -> BTreeMap<(String, String), f64> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (scene, pass, us) = (f.next()?, f.next()?, f.next()?);
            Some(((scene.to_owned(), pass.to_owned()), us.trim().parse().ok()?))
        })
        .collect()
}

/// µs per (scene, pass).
type Times = BTreeMap<(String, String), f64>;

/// One run of a scene: each pass's median over the timed frames, their sum and the frame
/// end to end.
type Run = BTreeMap<String, f64>;

/// The adapter the runs measured, and how many runs there were.
#[derive(Default)]
struct Measured {
    vendor: Option<String>,
    adapter: String,
    runs: usize,
}

/// Renders `shots`, `runs` times each from a fresh device, a round at a time (the
/// module's documentation says why). Returns each shot's runs, none for a skipped shot;
/// `None` without a GPU.
fn measure(
    shots: &[&Shot],
    runs: usize,
    options: RenderOptions,
    out: &mut Measured,
) -> Option<Vec<Vec<Run>>> {
    let mut per_shot: Vec<Vec<Run>> = vec![Vec::new(); shots.len()];
    let mut skipped = vec![false; shots.len()];
    for _ in 0..runs.max(1) {
        for (i, shot) in shots.iter().enumerate() {
            if skipped[i] {
                continue;
            }
            let why = match render(shot, options) {
                Ok(r) => {
                    out.vendor = Some(vendor_name(r.vendor_id));
                    out.adapter.clone_from(&r.adapter);
                    out.runs += 1;
                    let mut run: Run = r
                        .pass_us
                        .into_iter()
                        .map(|(name, us)| (name.to_owned(), us))
                        .collect();
                    let total: f64 = run.values().sum();
                    run.insert(TOTAL.to_owned(), total);
                    if r.frame_us > 0.0 {
                        run.insert(FRAME.to_owned(), r.frame_us);
                    }
                    per_shot[i].push(run);
                    continue;
                }
                Err(Skip::NoGpu(e)) => {
                    eprintln!("no GPU: {e}");
                    return None;
                }
                Err(Skip::NoScene(path)) => format!("{} missing", path.display()),
                Err(Skip::Unsupported(e)) => e,
                Err(Skip::NoDlss(e)) => format!("no DLSS ({e})"),
            };
            eprintln!("{}: skipped, {why}", shot.name);
            skipped[i] = true;
        }
    }
    Some(per_shot)
}

/// Each pass's fastest time over `runs` of `scene`, into `times`.
fn fastest(scene: &str, runs: &[Run], times: &mut Times) {
    for run in runs {
        for (pass, &us) in run {
            let t = times
                .entry((scene.to_owned(), pass.clone()))
                .or_insert(f64::INFINITY);
            *t = t.min(us);
        }
    }
}

/// What a measurement says against the baseline.
struct Verdict {
    /// Passes slower than their baseline by more than the threshold and the floor.
    slower: BTreeSet<(String, String)>,
    /// Scenes whose every large pass moved by about the same percentage as the frame:
    /// the GPU ran at other clocks, or shared with other work, rather than a pass
    /// regressing.
    uniform: BTreeSet<String>,
}

fn judge(measured: &Times, baseline: &Times, threshold: f64) -> Verdict {
    let change_of = |key: &(String, String)| -> Option<f64> {
        let before = *baseline.get(key)?;
        let now = *measured.get(key)?;
        (before > 0.0).then(|| (now / before - 1.0) * 100.0)
    };
    let slower = measured
        .iter()
        .filter(|&(key, &now)| {
            let frame = baseline
                .get(&(key.0.clone(), TOTAL.to_owned()))
                .copied()
                .unwrap_or(0.0);
            change_of(key).is_some_and(|change| change > threshold)
                && now - baseline[key] > FLOOR_US.max(frame * FLOOR_OF_FRAME)
        })
        .map(|(key, _)| key.clone())
        .collect();
    let uniform = measured
        .keys()
        .map(|(scene, _)| scene.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|scene| {
            let total_key = (scene.clone(), TOTAL.to_owned());
            let (Some(frame_change), Some(&frame)) =
                (change_of(&total_key), baseline.get(&total_key))
            else {
                return false;
            };
            let large: Vec<f64> = baseline
                .iter()
                .filter(|((s, p), us)| {
                    s == scene && p != TOTAL && p != FRAME && **us >= frame * UNIFORM_SHARE
                })
                .filter_map(|(key, _)| change_of(key))
                .collect();
            frame_change > threshold
                && large.len() >= 3
                && large
                    .iter()
                    .all(|c| (c - frame_change).abs() <= UNIFORM_SPREAD)
        })
        .collect();
    Verdict { slower, uniform }
}

fn main() -> ExitCode {
    // The test bed's frame (`scene::render`) keeps large state on the stack; Windows gives
    // the main thread 1 MiB.
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(run)
        .ok()
        .and_then(|t| t.join().ok())
        .unwrap_or(ExitCode::FAILURE)
}

fn run() -> ExitCode {
    let mut update = false;
    let mut strict = false;
    let mut runs = 3usize;
    let mut frames = 100usize;
    let mut threshold = 3.0f64;
    let mut only: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--update" => update = true,
            "--strict" => strict = true,
            "--runs" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(runs),
            "--frames" => frames = args.next().and_then(|v| v.parse().ok()).unwrap_or(frames),
            "--scene" => only.extend(args.next()),
            "--threshold" => {
                threshold = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(threshold);
            }
            other => {
                eprintln!("unknown argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    if update && !only.is_empty() {
        eprintln!("--update writes the whole baseline: measure every scene");
        return ExitCode::from(2);
    }
    let options = RenderOptions {
        validation: false,
        timed_frames: frames,
    };
    let started = Instant::now();
    let all = shots();
    let shots: Vec<&Shot> = all
        .iter()
        .filter(|s| only.is_empty() || only.iter().any(|n| n == s.name))
        .collect();
    let mut gpu = Measured::default();
    let Some(runs_of) = measure(&shots, runs, options, &mut gpu) else {
        return ExitCode::FAILURE;
    };
    let mut measured = Times::new();
    for (shot, shot_runs) in shots.iter().zip(&runs_of) {
        fastest(shot.name, shot_runs, &mut measured);
    }
    let Some(vendor) = gpu.vendor.clone() else {
        eprintln!("nothing measured");
        return ExitCode::FAILURE;
    };
    let path = baseline_path(&vendor);
    println!("{}; baseline {}", gpu.adapter, path.display());
    if update {
        let mut text = format!(
            "# GPU µs per pass (fastest of {runs} runs' medians over {frames} frames, a round \
             at a time), {}\n# scene\tpass\tµs\n",
            gpu.adapter
        );
        for ((scene, pass), us) in &measured {
            let _ = writeln!(text, "{scene}\t{pass}\t{us:.1}");
        }
        if let Err(e) = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))
            .and_then(|()| std::fs::write(&path, text))
        {
            eprintln!("{}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!(
            "baseline written ({} passes; {} runs in {:.0} s)",
            measured.len(),
            gpu.runs,
            started.elapsed().as_secs_f64()
        );
        return ExitCode::SUCCESS;
    }
    let baseline = read_baseline(&path);
    let mut verdict = judge(&measured, &baseline, threshold);
    // A scene with a slower pass is measured again and judged by its new runs (see the
    // module's documentation).
    let again: Vec<&Shot> = shots
        .iter()
        .copied()
        .filter(|s| verdict.slower.iter().any(|(scene, _)| scene == s.name))
        .collect();
    if !again.is_empty() {
        for key in &verdict.slower {
            println!(
                "first runs: {} | {} {:+.1} %",
                key.0,
                key.1,
                (measured[key] / baseline[key] - 1.0) * 100.0
            );
        }
        let Some(runs_again) = measure(&again, runs, options, &mut gpu) else {
            return ExitCode::FAILURE;
        };
        for (shot, shot_runs) in again.iter().zip(&runs_again) {
            if !shot_runs.is_empty() {
                measured.retain(|(scene, _), _| scene != shot.name);
                fastest(shot.name, shot_runs, &mut measured);
            }
        }
        verdict = judge(&measured, &baseline, threshold);
        println!(
            "measured again, judged by the new runs: {}\n",
            again.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
        );
    }
    let mut regressions = 0;
    println!("| scene | pass | baseline µs | now µs | change |");
    println!("|---|---|---:|---:|---:|");
    for (key, &now) in &measured {
        let (scene, pass) = key;
        let Some(&before) = baseline.get(key) else {
            println!("| {scene} | {pass} | — | {now:.1} | new |");
            continue;
        };
        let change = if before > 0.0 {
            (now / before - 1.0) * 100.0
        } else {
            0.0
        };
        let regressed =
            verdict.slower.contains(key) && (strict || !verdict.uniform.contains(scene));
        regressions += usize::from(regressed);
        println!(
            "| {scene} | {pass} | {before:.1} | {now:.1} | {change:+.1} %{} |",
            if regressed { " **slower**" } else { "" }
        );
    }
    for (scene, pass) in baseline
        .keys()
        .filter(|k| !measured.contains_key(*k) && (only.is_empty() || only.contains(&k.0)))
    {
        println!(
            "| {scene} | {pass} | {:.1} | — | gone |",
            baseline[&(scene.clone(), pass.clone())]
        );
    }
    for scene in &verdict.uniform {
        println!(
            "\n{scene}: every large pass moved by about the frame's change: other clocks or \
             other work on the GPU, not a pass; measure again on an idle machine"
        );
    }
    println!(
        "\n{} runs in {:.0} s",
        gpu.runs,
        started.elapsed().as_secs_f64()
    );
    if regressions > 0 {
        println!(
            "\n{regressions} pass(es) slower than the baseline by more than {threshold} % \
             (and {FLOOR_US} µs or {} % of the frame)",
            FLOOR_OF_FRAME * 100.0
        );
        if strict {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
