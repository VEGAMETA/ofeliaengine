//! Animation measurements (ADR 0068):
//!
//! 1. **Compression** of every clip of the skinned sample models: source keys, uniform
//!    `f32` samples, our format, its error, and, for comparison, a key-reduced format
//!    (16-bit range-reduced values, keys removed while linear interpolation stays within
//!    the bound locally; sampled with a binary search per track).
//! 2. **Sampling and blending** on the CPU: per character, two clips of the Fox sampled,
//!    blended, local to model, skinning matrices; one thread and the job system, 1 to
//!    10,000 characters; the stages apart; our format against raw floats and the
//!    key-reduced one.
//!
//! `cargo run -p engine_assets --release --example anim_bench -- [--assets DIR] [--frames N]`
//! (`DIR`: the test scenes, `ENGINE_TEST_ASSETS` by default).

#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::too_many_lines,
    clippy::needless_range_loop
)]

use engine_anim::*;
use engine_assets::animation::{SkinSkeleton, raw_clip, shell_distances, skin_skeleton};
use engine_assets::import_gltf;
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Quat, Vec3A, Vec4};
use std::path::PathBuf;
use std::time::Instant;

/// A clip with fewer keys: per track, the kept sample indices and their values (16-bit
/// range-reduced per component, dequantized here; the size counts 2 bytes per index and
/// per component).
struct Reduced {
    duration: f32,
    samples: usize,
    /// Per joint and channel: kept sample indices and values; empty: the rest pose.
    tracks: Vec<[(Vec<u16>, Vec<Vec4>); 3]>,
    bytes: usize,
}

fn quantize16(v: Vec4, min: Vec4, max: Vec4) -> Vec4 {
    let extent = (max - min).max(Vec4::splat(1e-20));
    let q = ((v - min) / extent * 65535.0).round();
    min + q / 65535.0 * extent
}

impl Reduced {
    /// Reduces `raw` resampled at `rate`: per track, keys added where the local error
    /// (moved virtual vertex at `shell`) exceeds `tolerance` (Douglas-Peucker).
    fn new(raw: &RawClip, skeleton: &Skeleton, rate: f32, shell: &[f32], tolerance: f32) -> Self {
        let samples = (raw.duration * rate).round().max(1.0) as usize + 1;
        let time = |s: usize| raw.duration * s as f32 / (samples - 1).max(1) as f32;
        let mut bytes = 0;
        let tracks = (0..skeleton.joint_count())
            .map(|j| {
                let rest = skeleton.rest().get(j);
                let values: Vec<Transform> = (0..samples)
                    .map(|s| raw.sample_joint(j, time(s), rest))
                    .collect();
                let mut channel = |c: usize| -> (Vec<u16>, Vec<Vec4>) {
                    let has = match c {
                        0 => raw.joints[j].rotation.is_some(),
                        1 => raw.joints[j].translation.is_some(),
                        _ => raw.joints[j].scale.is_some(),
                    };
                    if !has {
                        return (Vec::new(), Vec::new());
                    }
                    let mut v: Vec<Vec4> = values
                        .iter()
                        .map(|t| match c {
                            0 => Vec4::from(t.rotation),
                            1 => t.translation.extend(0.0),
                            _ => t.scale.extend(0.0),
                        })
                        .collect();
                    for s in 1..v.len() {
                        if c == 0 && v[s].dot(v[s - 1]) < 0.0 {
                            v[s] = -v[s];
                        }
                    }
                    let (mut min, mut max) = (Vec4::splat(f32::MAX), Vec4::splat(f32::MIN));
                    for x in &v {
                        min = min.min(*x);
                        max = max.max(*x);
                    }
                    let q: Vec<Vec4> = v.iter().map(|x| quantize16(*x, min, max)).collect();
                    let error = |a: Vec4, b: Vec4| match c {
                        0 => {
                            rotation_angle(
                                Quat::from_vec4(a).normalize(),
                                Quat::from_vec4(b).normalize(),
                            ) * shell[j]
                        }
                        1 => (a - b).truncate().length(),
                        _ => (a - b).truncate().abs().max_element() * shell[j],
                    };
                    let interpolate = |a: Vec4, b: Vec4, t: f32| {
                        if c == 0 {
                            Vec4::from(nlerp(Quat::from_vec4(a), Quat::from_vec4(b), t))
                        } else {
                            a.lerp(b, t)
                        }
                    };
                    let mut keep = vec![false; q.len()];
                    keep[0] = true;
                    *keep.last_mut().unwrap() = true;
                    let mut stack = vec![(0usize, q.len() - 1)];
                    while let Some((a, b)) = stack.pop() {
                        let (mut worst, mut at) = (0.0f32, a);
                        for s in a + 1..b {
                            let t = (s - a) as f32 / (b - a) as f32;
                            let e = error(interpolate(q[a], q[b], t), v[s]);
                            if e > worst {
                                worst = e;
                                at = s;
                            }
                        }
                        if worst > tolerance {
                            keep[at] = true;
                            stack.push((a, at));
                            stack.push((at, b));
                        }
                    }
                    let indices: Vec<u16> = (0..q.len())
                        .filter(|&s| keep[s])
                        .map(|s| s as u16)
                        .collect();
                    let kept: Vec<Vec4> = indices.iter().map(|&s| q[s as usize]).collect();
                    let components = 3;
                    bytes += indices.len() * (2 + 2 * components) + 32;
                    (indices, kept)
                };
                [channel(0), channel(1), channel(2)]
            })
            .collect();
        Self {
            duration: raw.duration,
            samples,
            tracks,
            bytes,
        }
    }

    fn sample(&self, time: f32, rest: &Pose, out: &mut Pose) {
        out.copy_from(rest);
        let last = (self.samples - 1) as f32;
        let f = (time / self.duration).clamp(0.0, 1.0) * last;
        for (j, channels) in self.tracks.iter().enumerate() {
            for (c, (indices, values)) in channels.iter().enumerate() {
                if indices.is_empty() {
                    continue;
                }
                let k = indices
                    .partition_point(|&s| f32::from(s) <= f)
                    .clamp(1, indices.len() - 1);
                let (a, b) = (f32::from(indices[k - 1]), f32::from(indices[k]));
                let t = ((f - a) / (b - a)).clamp(0.0, 1.0);
                let (va, vb) = (values[k - 1], values[k]);
                match c {
                    0 => out.rotations[j] = nlerp(Quat::from_vec4(va), Quat::from_vec4(vb), t),
                    1 => out.translations[j] = Vec3A::from(va.truncate().lerp(vb.truncate(), t)),
                    _ => out.scales[j] = Vec3A::from(va.truncate().lerp(vb.truncate(), t)),
                }
            }
        }
    }
}

/// Largest object-space error of `sample` against the source at every sample and
/// midpoint, on virtual vertices at `shell`.
fn object_error(
    raw: &RawClip,
    skeleton: &Skeleton,
    shell: &[f32],
    samples: usize,
    sample: &dyn Fn(f32, &mut Pose),
) -> f32 {
    let n = skeleton.joint_count();
    let (mut a, mut b) = (Pose::identity(n), Pose::identity(n));
    let (mut ma, mut mb) = (vec![Affine3A::IDENTITY; n], vec![Affine3A::IDENTITY; n]);
    let mut worst = 0.0f32;
    let checkpoints = 2 * samples - 1;
    for k in 0..checkpoints {
        let t = raw.duration * k as f32 / (checkpoints - 1).max(1) as f32;
        sample(t, &mut a);
        raw.sample(skeleton, t, &mut b);
        local_to_model(skeleton, &a, &Affine3A::IDENTITY, &mut ma);
        local_to_model(skeleton, &b, &Affine3A::IDENTITY, &mut mb);
        for j in 0..n {
            let d = shell[j];
            for v in [Vec3A::ZERO, Vec3A::X * d, Vec3A::Y * d, Vec3A::Z * d] {
                worst =
                    worst.max((ma[j].transform_point3a(v) - mb[j].transform_point3a(v)).length());
            }
        }
    }
    worst
}

struct Model {
    skin: SkinSkeleton,
    shell: Vec<f32>,
    raws: Vec<RawClip>,
    size: f32,
}

fn load(path: &PathBuf) -> Option<Model> {
    let bytes = std::fs::read(path).ok()?;
    let scene = import_gltf(&bytes, path.parent()).ok()?;
    let skin = skin_skeleton(&scene, 0).ok()?;
    let shell: Vec<f32> = shell_distances(&scene, 0, &skin)
        .into_iter()
        .map(|d| d.max(1e-3))
        .collect();
    let n = skin.skeleton.joint_count();
    let mut model = vec![Affine3A::IDENTITY; n];
    local_to_model(
        &skin.skeleton,
        skin.skeleton.rest(),
        &Affine3A::IDENTITY,
        &mut model,
    );
    let size = model
        .iter()
        .map(|m| m.translation.length())
        .fold(0.0f32, f32::max)
        .max(1e-3);
    let raws = (0..scene.animations.len())
        .filter_map(|a| raw_clip(&scene, a, &skin))
        .collect();
    Some(Model {
        skin,
        shell,
        raws,
        size,
    })
}

/// The per-character work of a frame.
struct Character {
    time: f32,
    a: Pose,
    b: Pose,
    pose: Pose,
    model: Vec<Affine3A>,
    palette: Vec<[f32; 12]>,
    blender: Blender,
}

#[derive(Clone, Copy)]
enum Stage {
    All,
    Sample,
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut assets = std::env::var_os("ENGINE_TEST_ASSETS")
        .map_or_else(|| PathBuf::from("test-assets"), PathBuf::from);
    let mut frames = 200usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--assets" => assets = PathBuf::from(args.next().unwrap()),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }

    println!("## Compression (bound: 0.01 % of the model's size, ACL's metric)\n");
    println!(
        "| model | clip | joints | s | Hz | source keys B | uniform f32 B | ours B | ratio vs uniform | bits | error / bound | key-reduced B | its error / bound | compress ms |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    let mut fox = None;
    for name in [
        "Fox/Fox.gltf",
        "CesiumMan/CesiumMan.gltf",
        "RiggedFigure/RiggedFigure.gltf",
        "RiggedSimple/RiggedSimple.gltf",
        "SimpleSkin/SimpleSkin.gltf",
    ] {
        let Some(model) = load(&assets.join(name)) else {
            println!("{name}: missing");
            continue;
        };
        let skeleton = &model.skin.skeleton;
        let bound = model.size * 1e-4;
        for raw in &model.raws {
            let settings = CompressSettings {
                error_threshold: bound,
                joint_shell_distances: model.shell.clone(),
                ..CompressSettings::default()
            };
            let start = Instant::now();
            let (clip, stats) = compress(raw, skeleton, &settings);
            let ms = start.elapsed().as_secs_f64() * 1e3;
            let shell: Vec<f32> = model
                .shell
                .iter()
                .map(|&d| d.max(settings.shell_distance))
                .collect();
            // Key reduction at the same rate: the local tolerance halved until the
            // hierarchy's error is within the bound too, measured like ours.
            let mut factor = 1.0;
            let (reduced, reduced_error) = loop {
                let reduced =
                    Reduced::new(raw, skeleton, stats.sample_rate, &shell, bound * factor);
                let error = object_error(raw, skeleton, &shell, reduced.samples, &|t, p| {
                    reduced.sample(t, skeleton.rest(), p)
                });
                if error <= bound || factor < 1e-3 {
                    break (reduced, error);
                }
                factor *= 0.5;
            };
            println!(
                "| {} | {} | {} | {:.2} | {:.0} | {} | {} | {} | {:.1}× | {:.1} | {:.2} | {} | {:.2} | {:.1} |",
                name.split('/').next().unwrap(),
                if raw.name.is_empty() {
                    "(unnamed)"
                } else {
                    &raw.name
                },
                skeleton.joint_count(),
                raw.duration,
                stats.sample_rate,
                stats.source_bytes,
                stats.uniform_bytes,
                stats.compressed_bytes,
                stats.uniform_bytes as f32 / stats.compressed_bytes as f32,
                stats.mean_bits,
                stats.max_error / bound,
                reduced.bytes,
                reduced_error / bound,
                ms
            );
            let _ = clip;
        }
        if name.starts_with("Fox") {
            fox = Some(model);
        }
    }

    let Some(fox) = fox else {
        println!("no Fox: skipping the sampling measurements");
        return;
    };
    let skeleton = &fox.skin.skeleton;
    let n = skeleton.joint_count();
    let bound = fox.size * 1e-4;
    let walk = fox.raws.iter().position(|r| r.name == "Walk").unwrap_or(0);
    let run = fox.raws.iter().position(|r| r.name == "Run").unwrap_or(0);
    let ours = |raw: &RawClip, threshold: f32| {
        compress(
            raw,
            skeleton,
            &CompressSettings {
                error_threshold: threshold,
                joint_shell_distances: fox.shell.clone(),
                sample_rate: Some(30.0),
                ..CompressSettings::default()
            },
        )
        .0
    };
    let clips = [ours(&fox.raws[walk], bound), ours(&fox.raws[run], bound)];
    // Every animated track raw (`f32`): what quantization saves in sampling time.
    let raw_clips = [ours(&fox.raws[walk], 0.0), ours(&fox.raws[run], 0.0)];
    let shell: Vec<f32> = fox.shell.iter().map(|&d| d.max(0.03)).collect();
    let reduced = [
        Reduced::new(&fox.raws[walk], skeleton, 30.0, &shell, bound * 0.25),
        Reduced::new(&fox.raws[run], skeleton, 30.0, &shell, bound * 0.25),
    ];
    let joints = &fox.skin.skin_joints;
    let inverse_bind = &fox.skin.inverse_bind;
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()) - 1;
    let pool = ThreadPool::builder().workers(workers).build().unwrap();
    pool.register_current_thread().unwrap();

    println!(
        "\n## Sampling and blending (Fox: {n} joints, {} skinned; Walk + Run at 30 Hz, blended, local to model, skinning matrices)\n",
        joints.len()
    );
    println!("| format | characters | threads | µs per character | µs per frame |");
    println!("|---|---:|---:|---:|---:|");
    for (label, format) in [("ours", 0), ("raw f32", 1), ("key-reduced", 2)] {
        for (count, threads) in [
            (1usize, 1usize),
            (100, 1),
            (1000, 1),
            (100, workers + 1),
            (1000, workers + 1),
            (10000, workers + 1),
        ] {
            if format != 0 && threads > 1 && count != 1000 {
                continue;
            }
            let mut characters: Vec<Character> = (0..count)
                .map(|i| Character {
                    time: (i as f32 * 0.137) % clips[0].duration(),
                    a: Pose::identity(n),
                    b: Pose::identity(n),
                    pose: Pose::identity(n),
                    model: vec![Affine3A::IDENTITY; n],
                    palette: vec![[0.0; 12]; joints.len()],
                    blender: Blender::new(n),
                })
                .collect();
            let update = |c: &mut Character, stage: Stage| {
                c.time = (c.time + 1.0 / 60.0) % clips[0].duration();
                let t1 = c.time / clips[0].duration() * clips[1].duration();
                match format {
                    0 => {
                        clips[0].sample(c.time, skeleton.rest(), &mut c.a);
                        clips[1].sample(t1, skeleton.rest(), &mut c.b);
                    }
                    1 => {
                        raw_clips[0].sample(c.time, skeleton.rest(), &mut c.a);
                        raw_clips[1].sample(t1, skeleton.rest(), &mut c.b);
                    }
                    _ => {
                        reduced[0].sample(c.time, skeleton.rest(), &mut c.a);
                        reduced[1].sample(t1, skeleton.rest(), &mut c.b);
                    }
                }
                if let Stage::Sample = stage {
                    return;
                }
                let layers = [
                    Layer {
                        pose: &c.a,
                        weight: 0.6,
                        mask: None,
                        mode: LayerMode::Normal,
                    },
                    Layer {
                        pose: &c.b,
                        weight: 0.4,
                        mask: None,
                        mode: LayerMode::Normal,
                    },
                ];
                c.blender.blend(skeleton.rest(), &layers, &mut c.pose);
                local_to_model(skeleton, &c.pose, &Affine3A::IDENTITY, &mut c.model);
                skinning_matrices(&c.model, joints, inverse_bind, &mut c.palette);
            };
            let mut run_frames = |stage: Stage| {
                let mut times = Vec::with_capacity(frames);
                for f in 0..frames + 20 {
                    let start = Instant::now();
                    if threads == 1 {
                        characters.iter_mut().for_each(|c| update(c, stage));
                    } else {
                        for_each_parallel(&pool, &mut characters, 16, |c| update(c, stage));
                    }
                    if f >= 20 {
                        times.push(start.elapsed().as_secs_f64() * 1e6);
                    }
                }
                median(times)
            };
            let frame = run_frames(Stage::All);
            println!(
                "| {label} | {count} | {threads} | {:.3} | {:.1} |",
                frame / count as f64,
                frame
            );
            if threads == 1 && count == 1000 {
                let sample = run_frames(Stage::Sample);
                println!(
                    "| {label}: sampling only (2 clips) | {count} | 1 | {:.3} | {:.1} |",
                    sample / count as f64,
                    sample
                );
            }
            std::hint::black_box(&characters);
        }
    }
}
