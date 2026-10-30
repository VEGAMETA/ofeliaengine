//! Animation graphs measured (ADR 0124's E12-5b): a locomotion graph on a 52-joint humanoid
//! of synthetic clips (every joint turning in every clip, so nothing compresses away). An
//! idle state and a moving state blending nine clips over two parameters, cross-faded both
//! ways; an upper-body layer masked to the spine's subtree; an additive lean. 100, 1,000
//! and 10,000 characters, their parameters changed every quarter second so a quarter of
//! them is in a cross-fade at any time (and some fades are cut short: those go on as
//! inertializations): the ticks (64 a second), the captures after them and the frames
//! apart, the frames on one thread and on the job system. `--inertialize` makes the two
//! transitions inertialized (ADR 0124's E12-5b-2).
//!
//! `cargo run -p engine_anim --release --example graph_bench -- [--frames N] [--workers N]
//! [--inertialize] [--unprepared]`

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::cast_precision_loss)]

use engine_anim::*;
use engine_core::jobs::ThreadPool;
use engine_core::math::{Quat, Vec3};
use std::cell::RefCell;
use std::time::Instant;

/// The engine's allocator, as the player has it (ADR 0086).
#[global_allocator]
static ALLOCATOR: engine_core::alloc::EngineAllocator = engine_core::alloc::EngineAllocator::new();

/// The humanoid: a root, a spine of three with a neck and head, two arms of four with five
/// fingers of three, two legs of four.
fn humanoid() -> Skeleton {
    let mut joints = vec![JointDesc {
        name: "root".into(),
        parent: None,
        rest: Transform::IDENTITY,
    }];
    let mut add = |name: String, parent: usize, offset: Vec3| {
        joints.push(JointDesc {
            name,
            parent: Some(parent),
            rest: Transform::new(offset, Quat::IDENTITY, Vec3::ONE),
        });
        joints.len() - 1
    };
    let mut spine = 0;
    for k in 0..3 {
        spine = add(format!("spine{k}"), spine, Vec3::Y * 0.15);
    }
    let neck = add("neck".into(), spine, Vec3::Y * 0.1);
    add("head".into(), neck, Vec3::Y * 0.1);
    for side in ["l", "r"] {
        let mut arm = spine;
        for k in 0..4 {
            arm = add(format!("arm_{side}{k}"), arm, Vec3::X * 0.12);
        }
        for f in 0..5 {
            let mut finger = arm;
            for k in 0..3 {
                finger = add(format!("finger_{side}{f}{k}"), finger, Vec3::X * 0.02);
            }
        }
    }
    for side in ["l", "r"] {
        let mut leg = 0;
        for k in 0..4 {
            leg = add(format!("leg_{side}{k}"), leg, -Vec3::Y * 0.2);
        }
    }
    Skeleton::new(&joints).unwrap().0
}

/// A second-long clip turning every joint about its own axis at its own pace.
fn clip(skeleton: &Skeleton, seed: f32) -> CompressedClip {
    let n = skeleton.joint_count();
    let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0).collect();
    let mut raw = RawClip {
        name: format!("clip{seed}"),
        duration: 1.0,
        joints: vec![Default::default(); n],
    };
    for (j, tracks) in raw.joints.iter_mut().enumerate() {
        let axis = Vec3::new(
            (j as f32 * 0.7 + seed).sin(),
            (j as f32 * 1.3 + seed).cos(),
            0.5,
        )
        .normalize();
        tracks.rotation = Some(RawTrack {
            interpolation: Interpolation::Linear,
            times: times.clone(),
            values: times
                .iter()
                .map(|t| {
                    let angle = 0.4 * (std::f32::consts::TAU * t + seed + j as f32).sin();
                    Quat::from_axis_angle(axis, angle)
                })
                .collect(),
        });
    }
    compress(&raw, skeleton, &CompressSettings::default()).0
}

/// The locomotion graph: idle and a 3×3 blend over (across, ahead), an upper-body layer, an
/// additive lean; its transitions inertialized or cross-faded.
fn locomotion(inertialize: bool) -> GraphDesc {
    let mut points = Vec::new();
    for (row, y) in [0.0, 1.0, 2.0].into_iter().enumerate() {
        for (col, x) in [-1.0, 0.0, 1.0].into_iter().enumerate() {
            points.push(([x, y], format!("move{}", row * 3 + col)));
        }
    }
    GraphDesc {
        params: ["across", "ahead", "wave", "lean"]
            .into_iter()
            .map(|name| ParamDesc {
                name: name.into(),
                ..ParamDesc::default()
            })
            .collect(),
        layers: vec![
            LayerDesc {
                name: "Base".into(),
                root: 0,
                weight: 1.0,
                ..LayerDesc::default()
            },
            LayerDesc {
                name: "Upper".into(),
                root: 3,
                weight: 1.0,
                weight_param: Some("wave".into()),
                mask: vec![("spine2".into(), 1.0)],
                ..LayerDesc::default()
            },
            LayerDesc {
                name: "Lean".into(),
                root: 4,
                additive: true,
                weight: 1.0,
                weight_param: Some("lean".into()),
                ..LayerDesc::default()
            },
        ],
        nodes: vec![
            NodeDesc::Machine { entry: 0 },
            NodeDesc::Clip {
                clip: "idle".into(),
                speed: 1.0,
            },
            NodeDesc::Blend2d {
                params: ["across".into(), "ahead".into()],
                points,
                speed: 1.0,
            },
            NodeDesc::Clip {
                clip: "wave".into(),
                speed: 1.0,
            },
            NodeDesc::Clip {
                clip: "lean".into(),
                speed: 1.0,
            },
        ],
        states: vec![
            StateDesc {
                name: "Idle".into(),
                machine: 0,
                pose: 1,
            },
            StateDesc {
                name: "Move".into(),
                machine: 0,
                pose: 2,
            },
        ],
        transitions: vec![
            TransitionDesc {
                from: Some(0),
                to: 1,
                when: "ahead > 0.1 or across * across > 0.01".into(),
                blend: 0.25,
                inertialize,
                ..TransitionDesc::default()
            },
            TransitionDesc {
                from: Some(1),
                to: 0,
                when: "ahead <= 0.1 and across * across <= 0.01".into(),
                blend: 0.25,
                inertialize,
                ..TransitionDesc::default()
            },
        ],
    }
}

/// One character: its state, its pose.
struct Character {
    state: GraphState,
    pose: Pose,
}

thread_local! {
    static WORK: RefCell<Option<GraphWork>> = const { RefCell::new(None) };
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let value = |name: &str, default: usize| {
        args.iter()
            .position(|a| a == name)
            .and_then(|k| args.get(k + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let frames = value("--frames", 120);
    let workers = value("--workers", 16);
    let skeleton = humanoid();
    let names: Vec<String> = ["idle", "wave", "lean"]
        .into_iter()
        .map(String::from)
        .chain((0..9).map(|k| format!("move{k}")))
        .collect();
    let clips: Vec<CompressedClip> = names
        .iter()
        .enumerate()
        .map(|(k, _)| clip(&skeleton, k as f32 * 0.37))
        .collect();
    let info = |name: &str| {
        let k = names.iter().position(|n| n == name)?;
        Some(ClipInfo {
            index: k as u16,
            length: 1.0,
            once: false,
        })
    };
    let inertialize = args.iter().any(|a| a == "--inertialize");
    let (mut graph, problems) = AnimGraph::cook(&locomotion(inertialize), info, &skeleton);
    assert!(problems.is_empty(), "{problems:?}");
    if !args.iter().any(|a| a == "--unprepared") {
        graph.prepare(|k| clips.get(usize::from(k)), skeleton.rest());
    }
    let slot = |name: &str| graph.param(name).unwrap();
    let (across, ahead, wave, lean) = (slot("across"), slot("ahead"), slot("wave"), slot("lean"));
    let pool = ThreadPool::builder().workers(workers).build().unwrap();
    println!(
        "{} joints, {} clips; {frames} frames, ticks at 64 Hz, {workers} workers; {}; {} bytes \
         a character's state",
        skeleton.joint_count(),
        clips.len(),
        if inertialize {
            "inertialized transitions"
        } else {
            "cross-faded transitions"
        },
        GraphState::new(&graph).size_bytes(),
    );
    println!(
        "characters | tick µs/char | capture µs/char | µs/capture | frame µs/char (1 thread) | \
         frame ms ({workers} workers)"
    );
    for n in [100usize, 1_000, 10_000] {
        let mut characters: Vec<Character> = (0..n)
            .map(|i| {
                let mut state = GraphState::new(&graph);
                state.set(wave, if i % 3 == 0 { 1.0 } else { 0.0 });
                state.set(lean, 0.5);
                Character {
                    state,
                    pose: Pose::identity(skeleton.joint_count()),
                }
            })
            .collect();
        let dt = 1.0 / 64.0;
        let (mut tick_time, mut ticks) = (0.0f64, 0usize);
        let (mut capture_time, mut captures) = (0.0f64, 0usize);
        let (mut frame_time, mut parallel_time) = (0.0f64, 0.0f64);
        let mut work = GraphWork::new(skeleton.joint_count());
        for frame in 0..frames {
            // A quarter of a second each: every character's wish changes in turn.
            if frame % 16 == 0 {
                for (i, c) in characters.iter_mut().enumerate() {
                    let phase = (i + frame / 16) % 4;
                    let moving = phase != 0;
                    c.state
                        .set(ahead, if moving { (i % 7) as f32 / 3.0 } else { 0.0 });
                    c.state.set(
                        across,
                        if moving {
                            ((i % 5) as f32 - 2.0) / 2.0
                        } else {
                            0.0
                        },
                    );
                }
            }
            if frame % 15 != 14 || frame == 0 {
                let started = Instant::now();
                for c in &mut characters {
                    graph.tick(&mut c.state, dt);
                }
                tick_time += started.elapsed().as_secs_f64();
                ticks += 1;
                let started = Instant::now();
                for c in &mut characters {
                    let clip = |k: u16| clips.get(usize::from(k));
                    if graph.capture(&mut c.state, clip, skeleton.rest(), &mut work) {
                        captures += 1;
                    }
                }
                capture_time += started.elapsed().as_secs_f64();
            }
            let alpha = (frame % 4) as f32 / 4.0;
            let started = Instant::now();
            for c in &mut characters {
                graph.pose(
                    &c.state,
                    alpha,
                    |k| clips.get(usize::from(k)),
                    skeleton.rest(),
                    &mut work,
                    &mut c.pose,
                );
            }
            frame_time += started.elapsed().as_secs_f64();
            let started = Instant::now();
            for_each_parallel(&pool, &mut characters, 64, |c| {
                WORK.with(|w| {
                    let mut w = w.borrow_mut();
                    let work = w.get_or_insert_with(|| GraphWork::new(skeleton.joint_count()));
                    graph.pose(
                        &c.state,
                        alpha,
                        |k| clips.get(usize::from(k)),
                        skeleton.rest(),
                        work,
                        &mut c.pose,
                    );
                });
            });
            parallel_time += started.elapsed().as_secs_f64();
        }
        let fading = characters
            .iter()
            .filter(|c| graph.state_name(&c.state, 0) == Some("Move"))
            .count();
        println!(
            "{n:>10} | {:>12.3} | {:>15.3} | {:>10.2} | {:>24.3} | {:>12.3}   ({fading} moving, \
             {captures} captures)",
            tick_time / ticks as f64 / n as f64 * 1e6,
            capture_time / ticks as f64 / n as f64 * 1e6,
            capture_time / captures.max(1) as f64 * 1e6,
            frame_time / frames as f64 / n as f64 * 1e6,
            parallel_time / frames as f64 * 1e3,
        );
    }
}
