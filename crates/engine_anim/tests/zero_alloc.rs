//! CLAUDE.md §5: zero heap allocations per frame in steady state. A frame of many
//! characters on the job system: two clips sampled and blended with a masked additive
//! layer, root motion taken out, local to model, two-bone IK, FABRIK and look-at, the
//! skinning matrices. One test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_anim::*;
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Quat, Vec3};
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

/// A spine of 6 joints with two legs of 3 joints each (12 joints).
fn skeleton() -> Skeleton {
    let mut joints = Vec::new();
    let mut add = |name: &str, parent: Option<usize>, offset: Vec3| {
        joints.push(JointDesc {
            name: name.into(),
            parent,
            rest: Transform::new(offset, Quat::IDENTITY, Vec3::ONE),
        });
        joints.len() - 1
    };
    let mut spine = add("hips", None, Vec3::new(0.0, 1.0, 0.0));
    let hips = spine;
    for k in 0..5 {
        spine = add(&format!("spine{k}"), Some(spine), Vec3::new(0.0, 0.15, 0.0));
    }
    for side in [-1.0f32, 1.0] {
        let thigh = add("thigh", Some(hips), Vec3::new(0.1 * side, -0.05, 0.0));
        let knee = add("knee", Some(thigh), Vec3::new(0.0, -0.45, 0.0));
        add("ankle", Some(knee), Vec3::new(0.0, -0.45, 0.0));
    }
    Skeleton::new(&joints).unwrap().0
}

fn clip(skeleton: &Skeleton, speed: f32) -> CompressedClip {
    let n = skeleton.joint_count();
    let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0).collect();
    let mut raw = RawClip {
        name: "cycle".into(),
        duration: 1.0,
        joints: vec![RawJointTracks::default(); n],
    };
    for (j, tracks) in raw.joints.iter_mut().enumerate() {
        tracks.rotation = Some(RawTrack {
            interpolation: Interpolation::Linear,
            times: times.clone(),
            values: times
                .iter()
                .map(|&t| {
                    Quat::from_rotation_x(
                        0.4 * (speed * t * core::f32::consts::TAU + j as f32).sin(),
                    )
                })
                .collect(),
        });
    }
    raw.joints[0].translation = Some(RawTrack {
        interpolation: Interpolation::Linear,
        times: times.clone(),
        values: times
            .iter()
            .map(|&t| Vec3::new(0.0, 1.0, speed * t))
            .collect(),
    });
    compress(&raw, skeleton, &CompressSettings::default()).0
}

/// One character's state and scratch.
struct Character {
    time: f32,
    walk: Pose,
    run: Pose,
    lean: Pose,
    pose: Pose,
    model: Vec<Affine3A>,
    palette: Vec<[f32; 12]>,
    blender: Blender,
    fabrik: Fabrik,
    motion: RootMotion,
}

#[test]
fn steady_state_animation_frames_do_not_allocate() {
    let skeleton = skeleton();
    let n = skeleton.joint_count();
    let (walk, run) = (clip(&skeleton, 1.0), clip(&skeleton, 2.0));
    let lean = compress(
        &RawClip {
            name: "lean".into(),
            duration: 1.0,
            joints: (0..n)
                .map(|_| RawJointTracks {
                    rotation: Some(RawTrack {
                        interpolation: Interpolation::Linear,
                        times: vec![0.0, 1.0],
                        values: vec![Quat::IDENTITY, Quat::from_rotation_z(0.3)],
                    }),
                    ..RawJointTracks::default()
                })
                .collect(),
        },
        &skeleton,
        &CompressSettings {
            additive: Some(AdditiveBase::FirstSample),
            ..CompressSettings::default()
        },
    )
    .0;
    let joints: Vec<u16> = (0..n as u16).collect();
    let inverse_bind = vec![Affine3A::IDENTITY; n];
    let upper_body: Vec<f32> = (0..n).map(|j| if j < 6 { 1.0 } else { 0.0 }).collect();
    let mut characters: Vec<Character> = (0..256)
        .map(|i| Character {
            time: i as f32 * 0.01,
            walk: Pose::identity(n),
            run: Pose::identity(n),
            lean: Pose::identity(n),
            pose: Pose::identity(n),
            model: vec![Affine3A::IDENTITY; n],
            palette: vec![[0.0; 12]; n],
            blender: Blender::new(n),
            fabrik: Fabrik::new(),
            motion: RootMotion::NONE,
        })
        .collect();
    let pool = ThreadPool::builder().workers(3).build().unwrap();
    pool.register_current_thread().unwrap();
    let spine: Vec<usize> = (1..6).collect();
    let frame = |characters: &mut [Character]| {
        for_each_parallel(&pool, characters, 16, |c| {
            let previous = c.time;
            c.time = (c.time + 1.0 / 60.0) % walk.duration();
            walk.sample(c.time, skeleton.rest(), &mut c.walk);
            run.sample(c.time, skeleton.rest(), &mut c.run);
            lean.sample(c.time, skeleton.rest(), &mut c.lean);
            let layers = [
                Layer {
                    pose: &c.walk,
                    weight: 0.7,
                    mask: None,
                    mode: LayerMode::Normal,
                },
                Layer {
                    pose: &c.run,
                    weight: 0.3,
                    mask: None,
                    mode: LayerMode::Normal,
                },
                Layer {
                    pose: &c.lean,
                    weight: 1.0,
                    mask: Some(&upper_body),
                    mode: LayerMode::Additive,
                },
            ];
            c.blender.blend(skeleton.rest(), &layers, &mut c.pose);
            c.motion = root_motion(
                &walk,
                &skeleton,
                0,
                previous,
                c.time,
                true,
                RootMotionMode::default(),
            );
            remove_root_motion(&mut c.pose, &walk, &skeleton, 0, RootMotionMode::default());
            let root = Affine3A::IDENTITY;
            local_to_model(&skeleton, &c.pose, &root, &mut c.model);
            let foot = Vec3::new(0.1, 0.1, 0.2);
            TwoBoneIk {
                start: 6,
                mid: 7,
                end: 8,
                target: foot,
                pole: Vec3::new(0.1, 0.5, 1.0),
                weight: 1.0,
            }
            .solve(&skeleton, &mut c.pose, &mut c.model, &root);
            c.fabrik.solve(
                &skeleton,
                &mut c.pose,
                &mut c.model,
                &root,
                &spine,
                Vec3::new(0.2, 1.6, 0.3),
                1e-3,
                8,
            );
            LookAt {
                joint: 5,
                forward: Vec3::Z,
                target: Vec3::new(0.0, 1.7, 3.0),
                weight: 0.5,
                max_angle: 1.0,
            }
            .solve(&skeleton, &mut c.pose, &mut c.model, &root);
            skinning_matrices(&c.model, &joints, &inverse_bind, &mut c.palette);
        });
    };
    for _ in 0..50 {
        frame(&mut characters);
    }
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    for _ in 0..200 {
        frame(&mut characters);
    }
    let allocations = ALLOCATIONS.load(Ordering::SeqCst) - before;
    println!(
        "200 frames of {} characters: {allocations} heap allocations",
        characters.len()
    );
    assert_eq!(allocations, 0);
    assert!(characters.iter().all(|c| c.palette[0][0].is_finite()));
}
