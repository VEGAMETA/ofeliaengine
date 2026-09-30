//! `perf_gate [--update] [--strict] [--runs N] [--threshold PCT]`: GPU time of every pass
//! of fixed scenes at 1920 × 1080, against this GPU vendor's baseline in
//! `tests/golden/perf/<vendor>.tsv` (ADR 0062, CLAUDE.md §5).
//!
//! - Each scene is rendered `--runs` times (3) from a fresh device, 200 timed frames each;
//!   a pass's time is the smallest of its per-run medians (interference from other work
//!   on the machine, and clocks that did not ramp up, only add time).
//! - A pass regresses when it is slower than its baseline by more than `--threshold`
//!   percent (3) and by more than 5 µs or 0.5 % of its scene's frame, whichever is more
//!   (small passes are noisy, and a few µs of a 12 ms frame matter to nobody); the
//!   frame's total too.
//! - When every large pass of a scene moved by about the frame's percentage, the GPU ran
//!   at other clocks or shared with other work: reported as such, and not counted without
//!   `--strict`.
//! - Without `--strict` regressions are reported, not fatal: ref-1 is not a dedicated
//!   benchmark machine (docs/perf/reference-hw.md).
//! - `--update` writes the measured times as the new baseline.
//!
//! Run it in release with the implicit layers off (`VK_LOADER_LAYERS_DISABLE=~implicit~`,
//! overlays change timing) and `ENGINE_ADAPTER=radeon` for the iGPU.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use engine_core::math::Vec3;
use engine_golden::scene::{
    RenderOptions, Shot, Skip, Source, procedural_lights, render, vendor_name,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const TOTAL: &str = "(frame total)";
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
            ..Shot::new(
                "sponza",
                Source::Gltf(PathBuf::from("test-assets/Sponza/Sponza.gltf")),
            )
        },
    ]
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

fn main() -> ExitCode {
    let mut update = false;
    let mut strict = false;
    let mut runs = 3usize;
    let mut threshold = 3.0f64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--update" => update = true,
            "--strict" => strict = true,
            "--runs" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(runs),
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
    let options = RenderOptions {
        validation: false,
        timed_frames: 200,
    };
    let mut measured: BTreeMap<(String, String), f64> = BTreeMap::new();
    let mut vendor = None;
    let mut adapter = String::new();
    for shot in shots() {
        let mut per_pass: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for _ in 0..runs.max(1) {
            match render(&shot, options) {
                Ok(r) => {
                    vendor = Some(vendor_name(r.vendor_id));
                    adapter.clone_from(&r.adapter);
                    let total: f64 = r.pass_us.values().sum();
                    for (name, us) in r.pass_us {
                        per_pass.entry(name.to_owned()).or_default().push(us);
                    }
                    per_pass.entry(TOTAL.to_owned()).or_default().push(total);
                }
                Err(Skip::NoGpu(e)) => {
                    eprintln!("no GPU: {e}");
                    return ExitCode::FAILURE;
                }
                Err(Skip::NoScene(path)) => {
                    eprintln!("{}: skipped, {} missing", shot.name, path.display());
                    break;
                }
            }
        }
        for (pass, values) in per_pass {
            let fastest = values.iter().copied().fold(f64::INFINITY, f64::min);
            measured.insert((shot.name.to_owned(), pass), fastest);
        }
    }
    let Some(vendor) = vendor else {
        eprintln!("nothing measured");
        return ExitCode::FAILURE;
    };
    let path = baseline_path(&vendor);
    println!("{adapter}; baseline {}", path.display());
    if update {
        let mut text = format!(
            "# GPU µs per pass (fastest of the runs' medians), {adapter}\n# scene\tpass\tµs\n"
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
        println!("baseline written ({} passes)", measured.len());
        return ExitCode::SUCCESS;
    }
    let baseline = read_baseline(&path);
    let change_of = |key: &(String, String)| -> Option<f64> {
        let before = *baseline.get(key)?;
        let now = *measured.get(key)?;
        (before > 0.0).then(|| (now / before - 1.0) * 100.0)
    };
    // Scenes whose every large pass moved by about the same percentage as the frame: the
    // GPU ran at other clocks, or shared with other work, rather than a pass regressing.
    let uniform: BTreeSet<String> = measured
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
                .filter(|((s, p), us)| s == scene && p != TOTAL && **us >= frame * UNIFORM_SHARE)
                .filter_map(|(key, _)| change_of(key))
                .collect();
            frame_change > threshold
                && large.len() >= 3
                && large
                    .iter()
                    .all(|c| (c - frame_change).abs() <= UNIFORM_SPREAD)
        })
        .collect();
    let mut regressions = 0;
    println!("| scene | pass | baseline µs | now µs | change |");
    println!("|---|---|---:|---:|---:|");
    for ((scene, pass), &now) in &measured {
        let Some(&before) = baseline.get(&(scene.clone(), pass.clone())) else {
            println!("| {scene} | {pass} | — | {now:.1} | new |");
            continue;
        };
        let change = if before > 0.0 {
            (now / before - 1.0) * 100.0
        } else {
            0.0
        };
        let frame = baseline
            .get(&(scene.clone(), TOTAL.to_owned()))
            .copied()
            .unwrap_or(0.0);
        let regressed = change > threshold
            && now - before > FLOOR_US.max(frame * FLOOR_OF_FRAME)
            && (strict || !uniform.contains(scene));
        regressions += usize::from(regressed);
        println!(
            "| {scene} | {pass} | {before:.1} | {now:.1} | {change:+.1} %{} |",
            if regressed { " **slower**" } else { "" }
        );
    }
    for (scene, pass) in baseline.keys().filter(|k| !measured.contains_key(*k)) {
        println!(
            "| {scene} | {pass} | {:.1} | — | gone |",
            baseline[&(scene.clone(), pass.clone())]
        );
    }
    for scene in &uniform {
        println!(
            "\n{scene}: every large pass moved by about the frame's change: other clocks or \
             other work on the GPU, not a pass; measure again on an idle machine"
        );
    }
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
