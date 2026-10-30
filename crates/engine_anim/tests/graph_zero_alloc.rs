//! CLAUDE.md §5 and ADR 0124's E12-5b: an animation graph's ticks and frames allocate nothing
//! in steady state, with transitions, triggers, cross-fades, a 2D blend, a masked layer and
//! an additive one at work. One test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_anim::*;
use engine_core::math::{Quat, Vec3};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// A spine of four and two arms of three (11 joints).
fn skeleton() -> Skeleton {
    let mut joints = vec![JointDesc {
        name: "hips".into(),
        parent: None,
        rest: Transform::IDENTITY,
    }];
    let mut add = |name: String, parent: usize| {
        joints.push(JointDesc {
            name,
            parent: Some(parent),
            rest: Transform::new(Vec3::Y * 0.2, Quat::IDENTITY, Vec3::ONE),
        });
        joints.len() - 1
    };
    let mut spine = 0;
    for k in 0..4 {
        spine = add(format!("spine{k}"), spine);
    }
    for side in ["l", "r"] {
        let mut arm = spine;
        for k in 0..3 {
            arm = add(format!("arm_{side}{k}"), arm);
        }
    }
    Skeleton::new(&joints).unwrap().0
}

/// A second-long clip turning every joint about its own axis.
fn clip(skeleton: &Skeleton, seed: f32) -> CompressedClip {
    let times: Vec<f32> = (0..=10).map(|k| k as f32 / 10.0).collect();
    let mut raw = RawClip {
        name: format!("c{seed}"),
        duration: 1.0,
        joints: vec![Default::default(); skeleton.joint_count()],
    };
    for (j, tracks) in raw.joints.iter_mut().enumerate() {
        tracks.rotation = Some(RawTrack {
            interpolation: Interpolation::Linear,
            times: times.clone(),
            values: times
                .iter()
                .map(|t| {
                    Quat::from_rotation_z(0.3 * (std::f32::consts::TAU * t + seed + j as f32).sin())
                })
                .collect(),
        });
    }
    compress(&raw, skeleton, &CompressSettings::default()).0
}

#[test]
fn graph_ticks_and_frames_do_not_allocate() {
    let skeleton = skeleton();
    let names = ["idle", "jump", "wave", "lean", "m0", "m1", "m2", "m3"];
    let clips: Vec<CompressedClip> = (0..names.len())
        .map(|k| clip(&skeleton, k as f32))
        .collect();
    let desc = GraphDesc {
        params: vec![
            ParamDesc {
                name: "x".into(),
                ..ParamDesc::default()
            },
            ParamDesc {
                name: "y".into(),
                ..ParamDesc::default()
            },
            ParamDesc {
                name: "jump".into(),
                kind: GraphParamKind::Trigger,
                value: 0.0,
            },
        ],
        layers: vec![
            LayerDesc {
                name: "Base".into(),
                root: 0,
                weight: 1.0,
                ..LayerDesc::default()
            },
            LayerDesc {
                name: "Arms".into(),
                root: 4,
                weight: 0.7,
                mask: vec![("arm_l0".into(), 1.0)],
                ..LayerDesc::default()
            },
            LayerDesc {
                name: "Lean".into(),
                root: 5,
                additive: true,
                weight: 0.5,
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
                params: ["x".into(), "y".into()],
                points: vec![
                    ([0.0, 0.0], "m0".into()),
                    ([1.0, 0.0], "m1".into()),
                    ([0.0, 1.0], "m2".into()),
                    ([1.0, 1.0], "m3".into()),
                ],
                speed: 1.0,
            },
            NodeDesc::Clip {
                clip: "jump".into(),
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
            StateDesc {
                name: "Jump".into(),
                machine: 0,
                pose: 3,
            },
        ],
        transitions: vec![
            TransitionDesc {
                from: Some(0),
                to: 1,
                when: "x + y > 0.1".into(),
                blend: 0.2,
                ..TransitionDesc::default()
            },
            TransitionDesc {
                from: Some(1),
                to: 0,
                when: "x + y <= 0.1".into(),
                blend: 0.2,
                ..TransitionDesc::default()
            },
            TransitionDesc {
                from: None,
                to: 2,
                when: "jump".into(),
                blend: 0.1,
                ..TransitionDesc::default()
            },
            TransitionDesc {
                from: Some(2),
                to: 0,
                exit: 0.8,
                blend: 0.15,
                ..TransitionDesc::default()
            },
        ],
    };
    let info = |name: &str| {
        let k = names.iter().position(|n| *n == name)?;
        Some(ClipInfo {
            index: k as u16,
            length: 1.0,
            once: name == "jump",
        })
    };
    let (graph, problems) = AnimGraph::cook(&desc, info, &skeleton);
    assert!(problems.is_empty(), "{problems:?}");
    let (x, y, jump) = (
        graph.param("x").unwrap(),
        graph.param("y").unwrap(),
        graph.param("jump").unwrap(),
    );
    let mut states: Vec<GraphState> = (0..32).map(|_| GraphState::new(&graph)).collect();
    let mut poses: Vec<Pose> = (0..32)
        .map(|_| Pose::identity(skeleton.joint_count()))
        .collect();
    let mut work = GraphWork::new(skeleton.joint_count());
    let mut run = |frames: usize, states: &mut [GraphState], poses: &mut [Pose]| {
        let mut visited = 0usize;
        for frame in 0..frames {
            for (i, s) in states.iter_mut().enumerate() {
                // Wishes changing, a jump now and then.
                let t = (frame + i * 7) % 200;
                s.set(x, if t < 100 { (t % 30) as f32 / 30.0 } else { 0.0 });
                s.set(y, if t < 60 { 0.5 } else { 0.0 });
                if t == 150 {
                    s.fire(jump);
                }
                graph.tick(s, 1.0 / 64.0);
                if graph.state_name(s, 0) == Some("Jump") {
                    visited += 1;
                }
            }
            for (s, pose) in states.iter().zip(poses.iter_mut()) {
                graph.pose(
                    s,
                    (frame % 4) as f32 / 4.0,
                    |k| clips.get(usize::from(k)),
                    skeleton.rest(),
                    &mut work,
                    pose,
                );
            }
        }
        visited
    };
    run(400, &mut states, &mut poses);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let jumped = run(800, &mut states, &mut poses);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!(
        "{allocations} allocations in 800 ticks and frames of 32 characters; {jumped} jumping"
    );
    assert!(jumped > 0, "the trigger took them to the jump");
    assert_eq!(allocations, 0, "allocations in steady state");
}
