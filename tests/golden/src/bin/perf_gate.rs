//! `perf_gate [--update] [--strict] [--runs N] [--frames N] [--threshold PCT]
//! [--scene NAME]...`: GPU time of every pass of fixed scenes at 1920 × 1080, against this
//! GPU vendor's baseline in `tests/golden/perf/<vendor>.tsv` (ADR 0062, CLAUDE.md §5).
//!
//! - Each scene is rendered `--runs` times (3) from a fresh device, `--frames` timed
//!   frames each (100: the same medians as 200 within the runs' noise, in half the
//!   Radeon's time); a check takes each pass's smallest per-run median (interference from
//!   other work on the machine, and clocks that did not ramp up, only add time). The
//!   runs go a round at a time, the first of every scene, then the second: a slow spell
//!   of the machine (work on the CPU next to the Radeon iGPU, other work on a GPU) then
//!   slows one run of a few scenes instead of every run of one.
//! - A pass regresses when it is slower than its baseline by more than `--threshold`
//!   percent and by more than 5 µs or a share of its scene's frame, whichever is more
//!   (small passes are noisy, and a few µs of a 12 ms frame matter to nobody); the
//!   frame's total too, and `(gpu frame)`, the frame end to end on the GPU (with async
//!   compute, ADR 0065, passes overlap: an overlapped pass may read longer while the frame
//!   gets shorter). On NVIDIA: 3 % and 0.5 % of the frame. On AMD: 10 % and 2 % of the
//!   frame: on an idle machine the Radeon iGPU's frame moves by up to ~8 % and its
//!   memory-bound passes (bloom, shadows) by up to ~35 % for minutes at a time, so it
//!   reports large regressions only (owner, 2026-10-10).
//! - A scene whose GPU ran slower as a whole is reported as such, and not counted without
//!   `--strict`: every large pass moved by about the frame's percentage (other clocks,
//!   other work on the GPU), or the frame and at least two thirds of its large passes
//!   are slower by more than half the threshold (the Radeon's slow state slows the
//!   memory-bound passes most). A regression of the code mostly moves one pass.
//! - A scene with a counted slower pass is measured again, `--runs` more runs, and
//!   judged by those: a slow spell has passed by then, a regression has not.
//! - Without `--strict` regressions are reported, not fatal: ref-1 is not a dedicated
//!   benchmark machine (docs/perf/reference-hw.md).
//! - `--update` writes the measured times as the new baseline, each pass's median run:
//!   one lucky run (two queues overlapping favourably, a faster memory placement on the
//!   Radeon) must not set the bar that a check's fastest run is held to.
//! - `--scene NAME` (repeatable) measures only those scenes (comparisons, A/B runs; with
//!   `--update` their rows replace theirs in the baseline and the others stay: a new
//!   scene's first baseline).
//!
//! `perf_gate --registers [--update] [--scene NAME]...` is the register check instead
//! ([`engine_golden::registers`], ADR 0062's addendum of 2026-10-15): every scene rendered
//! once with the driver's pipeline statistics on, and the vector registers and spills of
//! every pipeline they made against `tests/golden/perf/<vendor>.registers.tsv`. A pipeline
//! using more is reported (`**more registers**`), never fatal. `--update` writes the
//! baseline (with `--scene`, those scenes' pipelines replace theirs).
//!
//! Run it in release with the implicit layers off (`VK_LOADER_LAYERS_DISABLE=~implicit~`,
//! overlays change timing; elevated, also each overlay's own switch, as `tools\check.ps1`
//! sets them) and `ENGINE_ADAPTER=radeon` for the iGPU.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use engine_core::math::Vec3;
use engine_frame::{Antialiasing, DeviceClass, Msaa, Quality, RenderDefaults, Upscale, ViewKind};
use engine_golden::registers::{self, Registers};
use engine_golden::scene::{
    RenderOptions, Shot, Skip, Source, procedural_lights, render, sponza_lights, vendor_name,
};
use engine_render::ao::AoSettings;
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::dof::DofSettings;
use engine_render::grading::ColorGrading;
use engine_render::motion_blur::MotionBlurSettings;
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
/// Passes of at least this share of the frame decide whether a scene moved as a whole.
const UNIFORM_SHARE: f64 = 0.05;
/// ... when each moved within this many percentage points of the frame.
const UNIFORM_SPREAD: f64 = 2.0;

/// What a pass must lose to regress on this GPU vendor (see the module's documentation).
#[derive(Clone, Copy)]
struct Limits {
    /// Percent of the pass's baseline.
    threshold: f64,
    /// Share of the scene's frame (and at least [`FLOOR_US`]).
    floor_of_frame: f64,
}

fn limits(vendor: &str) -> Limits {
    if vendor == "amd" {
        Limits {
            threshold: 10.0,
            floor_of_frame: 0.02,
        }
    } else {
        Limits {
            threshold: 3.0,
            floor_of_frame: 0.005,
        }
    }
}

/// Intel's Sponza (2022), the dense scene of ADR 0085: not in the default test assets
/// (`tools/fetch-test-assets.ps1 -Only IntelSponza`), its shots skipped where it is missing.
const INTEL_SPONZA: &str = "IntelSponza/main_sponza/NewSponza_Main_glTF_003.gltf";

fn shots() -> Vec<Shot> {
    let mut shots = vec![
        Shot {
            size: [1920, 1080],
            eye: Some(Vec3::new(0.4, 1.9, 4.8)),
            target: Some(Vec3::new(0.0, 1.0, -1.0)),
            lights: procedural_lights(),
            ..Shot::new("procedural", Source::Procedural)
        },
        // Decals (ADR 0082): the golden shot's five, through the decal buffer (the depth
        // prepass, the decal tiles and pass, the shading's decal variant).
        Shot {
            size: [1920, 1080],
            eye: Some(Vec3::new(0.4, 1.9, 4.8)),
            target: Some(Vec3::new(0.0, 1.0, -1.0)),
            lights: procedural_lights(),
            decals: true,
            ..Shot::new("procedural_decals", Source::Procedural)
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
        // Color grading (ADR 0094): the tone mapper's graded entry point, its LUT built in
        // the first frame.
        Shot {
            size: [1920, 1080],
            grading: ColorGrading {
                white_balance: [0.35, -0.05],
                contrast: 1.2,
                saturation: 0.9,
                ..ColorGrading::default()
            },
            ..Shot::new(
                "sponza_graded",
                Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
            )
        },
        // Depth of field (ADR 0095), strong: the curtains in front and the far end of the
        // atrium blurred, its passes after antialiasing.
        Shot {
            size: [1920, 1080],
            depth_of_field: Some(DofSettings {
                focus_distance: 3.0,
                f_number: 1.4,
                sensor_height: 0.1,
                ..DofSettings::default()
            }),
            ..Shot::new(
                "sponza_dof",
                Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
            )
        },
        // Motion blur on a cinematic view (ADR 0096): the camera walking forward 7 cm a
        // frame, so the whole image moves (radially); the default settings.
        Shot {
            size: [1920, 1080],
            camera_motion: Vec3::new(0.07, 0.0, 0.0),
            kind: ViewKind::Cinematic {
                motion_blur: Some(MotionBlurSettings::default()),
            },
            ..Shot::new(
                "sponza_motion_blur",
                Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")),
            )
        },
        // Temporal antialiasing and upscaling through the frame renderer (ADR 0072): TAA at
        // the output size, TAAU and FSR 3.1 from 67 % (motion vectors, the resolve, the
        // upscaler's passes; the post chain at the output size).
        antialiased("sponza_taa", Antialiasing::TAAU, Quality::Native),
        antialiased("sponza_taau_67", Antialiasing::TAAU, Quality::Quality),
        antialiased("sponza_fsr_quality", Antialiasing::FSR, Quality::Quality),
        // TAA with ambient occlusion, so the depth prepass draws and writes the motion
        // vectors, and the shading after it draws without them.
        Shot {
            ao: Some(AoSettings::default()),
            ..antialiased("sponza_taa_ao", Antialiasing::TAAU, Quality::Native)
        },
        // Each device class's defaults as a game starts with them (`RenderDefaults`, ADR
        // 0072): with meshlet culling (ADR 0085), its culls and the prepass, shading and
        // shadows drawn by meshlet; the integrated class's from 67 % with lighter ambient
        // occlusion.
        class_defaults("sponza_defaults_discrete", SPONZA, DeviceClass::Discrete),
        class_defaults(
            "sponza_defaults_integrated",
            SPONZA,
            DeviceClass::Integrated,
        ),
        // Intel's Sponza with each class's defaults: 3.7 M triangles with 4K textures, where
        // the shadows, the prepass and meshlet culling carry the frame.
        class_defaults(
            "intel_sponza_defaults_discrete",
            INTEL_SPONZA,
            DeviceClass::Discrete,
        ),
        class_defaults(
            "intel_sponza_defaults_integrated",
            INTEL_SPONZA,
            DeviceClass::Integrated,
        ),
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
        // Catmull-Rom and Lanczos-3 from 67 %: resampling in the tone mapper, Lanczos-3's
        // rows in a pass of their own.
        antialiased(
            "sponza_catmull_rom_67",
            Antialiasing::spatial(Upscale::CatmullRom),
            Quality::Quality,
        ),
        antialiased(
            "sponza_lanczos3_67",
            Antialiasing::spatial(Upscale::Lanczos3),
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
        // Probe GI (ADR 0088), through the runtime: the volume converged before the timed
        // frames, which resolve it (one sample per 2 × 2 pixels on discrete GPUs, 4 × 4 on
        // integrated ones) and shade with it; the ray scene kept (nothing moves).
        Shot {
            size: [1920, 1080],
            runtime: true,
            gi: Some(1.0),
            ..Shot::new(
                "sponza_gi",
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
    ];
    // The still shots draw the sun's cascades whole every frame, as they did before their
    // cache (ADR 0093): their passes stay measured. A still camera's cached cascades
    // draw nothing.
    for shot in &mut shots {
        shot.cached_shadows = false;
    }
    // The cache's own cost: the camera walking (5 cm a frame) through Intel's Sponza with
    // each class's defaults, the cascades drawing the strips it exposes.
    for (name, class) in [
        ("intel_sponza_walk_discrete", DeviceClass::Discrete),
        ("intel_sponza_walk_integrated", DeviceClass::Integrated),
    ] {
        shots.push(Shot {
            camera_motion: Vec3::new(0.05, 0.0, 0.0),
            ..class_defaults(name, INTEL_SPONZA, class)
        });
    }
    shots
}

const SPONZA: &str = "Sponza/Sponza.gltf";

/// The glTF `scene` with `class`'s defaults (`engine_frame::RenderDefaults`), cooked as a
/// game's pack is by default: with the meshes' detail levels. Without them, the view and
/// every cascade drew Intel's Sponza (3.7 M triangles) at full detail: its shadows took
/// 21.3 ms on the Radeon instead of 4.1, and the integrated class's frame 57.6 ms instead
/// of 33.8.
fn class_defaults(name: &'static str, scene: &str, class: DeviceClass) -> Shot {
    let defaults = RenderDefaults::for_class(class);
    Shot {
        size: [1920, 1080],
        ao: defaults.ambient_occlusion,
        antialiasing: defaults.antialiasing,
        quality: defaults.quality,
        meshlets: defaults.meshlet_culling,
        lods: true,
        ..Shot::new(name, Source::Gltf(PathBuf::from(scene)))
    }
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

/// The register check (`--registers`, the module's documentation): renders `shots` once
/// each with the driver's statistics on, and compares the registers of every pipeline
/// they made with this vendor's baseline, or with `update` writes it (`some`: only some
/// scenes ran, so the baseline's other pipelines stay, and none is reported gone).
fn check_registers(shots: &[&Shot], update: bool, some: bool) -> ExitCode {
    let started = Instant::now();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let shaders = registers::shader_names(&root);
    let options = RenderOptions {
        validation: false,
        timed_frames: 0,
    };
    let mut measured = Registers::new();
    let mut gpu: Option<(String, String)> = None;
    for shot in shots {
        let why = match render(shot, options) {
            Ok(r) => {
                gpu = Some((vendor_name(r.vendor_id), r.adapter));
                None
            }
            Err(Skip::NoGpu(e)) => {
                eprintln!("no GPU: {e}");
                return ExitCode::FAILURE;
            }
            Err(Skip::NoScene(path)) => Some(format!("{} missing", path.display())),
            Err(Skip::Unsupported(e)) => Some(e),
            Err(Skip::NoDlss(e)) => Some(format!("no DLSS ({e})")),
        };
        if let Some(why) = why {
            eprintln!("{}: skipped, {why}", shot.name);
        }
        registers::add(
            &mut measured,
            &engine_rhi::take_pipeline_records(),
            &shaders,
        );
    }
    let Some((vendor, adapter)) = gpu else {
        eprintln!("nothing rendered");
        return ExitCode::FAILURE;
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("perf")
        .join(format!("{vendor}.registers.tsv"));
    println!("registers: {adapter}; baseline {}", path.display());
    if measured.is_empty() {
        println!("registers: the driver reports no pipeline statistics; nothing to check");
        return ExitCode::SUCCESS;
    }
    if update {
        let mut rows = if some {
            registers::read(&path)
        } else {
            Registers::new()
        };
        rows.extend(measured);
        let header = format!(
            "Vector registers and spills of every pipeline the perf gate's scenes make, as \
             the driver reports them\n(perf_gate --registers, ADR 0062), {adapter}"
        );
        if let Err(e) = std::fs::write(&path, registers::format(&rows, &header)) {
            eprintln!("{}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!(
            "register baseline written ({} pipelines, {:.0} s)",
            rows.len(),
            started.elapsed().as_secs_f64()
        );
        return ExitCode::SUCCESS;
    }
    let baseline = registers::read(&path);
    let changes = registers::compare(&baseline, &measured);
    if !changes.grew.is_empty() || !changes.shrank.is_empty() {
        println!("| pipeline | stage | statistic | baseline | now |");
        println!("|---|---|---|---:|---:|");
        for (change, more) in changes
            .grew
            .iter()
            .map(|c| (c, true))
            .chain(changes.shrank.iter().map(|c| (c, false)))
        {
            println!(
                "| {} | {} | {} | {} | {}{} |",
                change.pipeline,
                change.stage,
                change.statistic,
                change.before,
                change.now,
                if more { " **more registers**" } else { "" }
            );
        }
    }
    for pipeline in &changes.new {
        println!("registers: new pipeline {pipeline}");
    }
    if !some {
        for pipeline in &changes.gone {
            println!("registers: gone {pipeline}");
        }
    }
    let grew: BTreeSet<&str> = changes.grew.iter().map(|c| c.pipeline.as_str()).collect();
    println!(
        "registers: {} pipelines, {} use more than the baseline (a warning), {} fewer; {} new, \
         {} gone ({:.0} s)",
        measured.len(),
        grew.len(),
        changes
            .shrank
            .iter()
            .map(|c| c.pipeline.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        changes.new.len(),
        if some { 0 } else { changes.gone.len() },
        started.elapsed().as_secs_f64()
    );
    ExitCode::SUCCESS
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

/// Each pass's time over `runs` of `scene`, into `times`: its fastest run, or with
/// `typical` its median run (a baseline; see the module's documentation).
fn summarize(scene: &str, runs: &[Run], typical: bool, times: &mut Times) {
    let mut per_pass: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for run in runs {
        for (pass, &us) in run {
            per_pass.entry(pass).or_default().push(us);
        }
    }
    for (pass, mut values) in per_pass {
        values.sort_by(f64::total_cmp);
        let us = if typical {
            values[(values.len() - 1) / 2]
        } else {
            values[0]
        };
        times.insert((scene.to_owned(), pass.to_owned()), us);
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
    /// Scenes whose frame and most large passes slowed together, each by more than half
    /// the threshold: the GPU ran slower as a whole.
    broad: BTreeSet<String>,
}

impl Verdict {
    /// Whether `scene`'s GPU ran slower as a whole (not counted without `--strict`).
    fn shifted(&self, scene: &str) -> bool {
        self.uniform.contains(scene) || self.broad.contains(scene)
    }
}

fn judge(measured: &Times, baseline: &Times, limits: Limits) -> Verdict {
    let Limits {
        threshold,
        floor_of_frame,
    } = limits;
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
                && now - baseline[key] > FLOOR_US.max(frame * floor_of_frame)
        })
        .map(|(key, _)| key.clone())
        .collect();
    let mut uniform = BTreeSet::new();
    let mut broad = BTreeSet::new();
    let scenes: BTreeSet<&String> = measured.keys().map(|(scene, _)| scene).collect();
    for scene in scenes {
        let total_key = (scene.clone(), TOTAL.to_owned());
        let (Some(frame_change), Some(&frame)) = (change_of(&total_key), baseline.get(&total_key))
        else {
            continue;
        };
        // How each pass of at least `UNIFORM_SHARE` of the frame moved.
        let large: Vec<f64> = baseline
            .iter()
            .filter(|((s, p), us)| {
                s == scene && p != TOTAL && p != FRAME && **us >= frame * UNIFORM_SHARE
            })
            .filter_map(|(key, _)| change_of(key))
            .collect();
        if large.len() < 3 {
            continue;
        }
        if frame_change > threshold
            && large
                .iter()
                .all(|c| (c - frame_change).abs() <= UNIFORM_SPREAD)
        {
            uniform.insert(scene.clone());
        } else if frame_change > threshold / 2.0
            && 3 * large.iter().filter(|&&c| c > threshold / 2.0).count() >= 2 * large.len()
        {
            broad.insert(scene.clone());
        }
    }
    Verdict {
        slower,
        uniform,
        broad,
    }
}

fn main() -> ExitCode {
    let mut update = false;
    let mut strict = false;
    let mut runs = 3usize;
    let mut frames = 100usize;
    let mut threshold: Option<f64> = None;
    let mut only: Vec<String> = Vec::new();
    let mut check_registers_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--update" => update = true,
            "--registers" => check_registers_only = true,
            "--strict" => strict = true,
            "--runs" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(runs),
            "--frames" => frames = args.next().and_then(|v| v.parse().ok()).unwrap_or(frames),
            "--scene" => only.extend(args.next()),
            "--threshold" => {
                threshold = args.next().and_then(|v| v.parse().ok()).or(threshold);
            }
            other => {
                eprintln!("unknown argument {other}");
                return ExitCode::from(2);
            }
        }
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
    if check_registers_only {
        // SAFETY: no other thread runs yet; each device reads it when it is created.
        unsafe { std::env::set_var("ENGINE_PIPELINE_STATS", "1") };
        return check_registers(&shots, update, !only.is_empty());
    }
    let mut gpu = Measured::default();
    let Some(runs_of) = measure(&shots, runs, options, &mut gpu) else {
        return ExitCode::FAILURE;
    };
    let mut measured = Times::new();
    for (shot, shot_runs) in shots.iter().zip(&runs_of) {
        summarize(shot.name, shot_runs, update, &mut measured);
    }
    let Some(vendor) = gpu.vendor.clone() else {
        eprintln!("nothing measured");
        return ExitCode::FAILURE;
    };
    let path = baseline_path(&vendor);
    let mut limits = limits(&vendor);
    if let Some(threshold) = threshold {
        limits.threshold = threshold;
    }
    let threshold = limits.threshold;
    println!(
        "{}; baseline {}; threshold {threshold} % (and {} % of the frame)",
        gpu.adapter,
        path.display(),
        limits.floor_of_frame * 100.0
    );
    if update {
        // With `--scene`, the other scenes keep their rows.
        let mut rows = if only.is_empty() {
            Times::new()
        } else {
            read_baseline(&path)
        };
        rows.retain(|(scene, _), _| !shots.iter().any(|s| s.name == scene));
        rows.extend(measured.iter().map(|(key, us)| (key.clone(), *us)));
        let mut text = format!(
            "# GPU µs per pass (median of {runs} runs' medians over {frames} frames, a round \
             at a time), {}\n# scene\tpass\tµs\n",
            gpu.adapter
        );
        for ((scene, pass), us) in &rows {
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
            rows.len(),
            gpu.runs,
            started.elapsed().as_secs_f64()
        );
        return ExitCode::SUCCESS;
    }
    let baseline = read_baseline(&path);
    let mut verdict = judge(&measured, &baseline, limits);
    // A scene with a counted slower pass is measured again and judged by its new runs
    // (see the module's documentation); one whose GPU ran slower as a whole is not.
    let counted = |verdict: &Verdict, scene: &str| strict || !verdict.shifted(scene);
    let again: Vec<&Shot> = shots
        .iter()
        .copied()
        .filter(|s| {
            counted(&verdict, s.name) && verdict.slower.iter().any(|(scene, _)| scene == s.name)
        })
        .collect();
    if !again.is_empty() {
        for key in verdict
            .slower
            .iter()
            .filter(|(scene, _)| again.iter().any(|s| s.name == scene))
        {
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
                summarize(shot.name, shot_runs, false, &mut measured);
            }
        }
        verdict = judge(&measured, &baseline, limits);
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
        let regressed = verdict.slower.contains(key) && counted(&verdict, scene);
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
    for scene in &verdict.broad {
        println!(
            "\n{scene}: the frame and most of its large passes slowed together: the GPU ran \
             slower as a whole (clocks, the iGPU's memory state, other work), not one pass"
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
            limits.floor_of_frame * 100.0
        );
        if strict {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
