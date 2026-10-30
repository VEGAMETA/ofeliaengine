//! Known answers: compression within its bound, sampling against the source, additive
//! round trips, IK reaching or clamping, root motion.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_anim::*;
use engine_core::math::{Affine3A, Quat, Vec3, Vec3A};

/// Small deterministic generator (xorshift64*).
struct Rng(u64);
impl Rng {
    fn unit(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// A chain of `n` joints, each 0.3 above its parent, with a branch at joint 2.
fn chain(n: usize) -> Skeleton {
    let joints: Vec<JointDesc> = (0..n)
        .map(|j| JointDesc {
            name: format!("j{j}"),
            parent: match j {
                0 => None,
                // Joints past 5 hang off joint 2 (a second branch).
                6.. if j == 6 => Some(2),
                _ => Some(j - 1),
            },
            rest: Transform::new(
                Vec3::new(0.0, if j == 0 { 0.0 } else { 0.3 }, 0.0),
                Quat::from_rotation_z(0.1 * j as f32),
                Vec3::ONE,
            ),
        })
        .collect();
    Skeleton::new(&joints).unwrap().0
}

/// A clip moving every joint: smooth rotations, a translating root, a scaling joint, a
/// constant track and untouched (default) tracks; keys at 30 Hz.
fn wave(skeleton: &Skeleton, seconds: f32, interpolation: Interpolation) -> RawClip {
    let n = skeleton.joint_count();
    let keys = (seconds * 30.0).round() as usize + 1;
    let times: Vec<f32> = (0..keys).map(|k| k as f32 / 30.0).collect();
    let mut rng = Rng(42);
    let mut joints = vec![RawJointTracks::default(); n];
    let per_key = |values: Vec<Vec3>| -> Vec<Vec3> {
        if interpolation == Interpolation::CubicSpline {
            values
                .iter()
                .flat_map(|&v| [Vec3::ZERO, v, Vec3::ZERO])
                .collect()
        } else {
            values
        }
    };
    for (j, tracks) in joints.iter_mut().enumerate() {
        let (fa, fb, phase) = (1.0 + rng.unit() * 2.0, 0.5 + rng.unit(), rng.unit() * 6.0);
        let rotations: Vec<Quat> = times
            .iter()
            .map(|&t| {
                Quat::from_euler(
                    engine_core::math::EulerRot::XYZ,
                    0.6 * (fa * t + phase).sin(),
                    0.4 * (fb * t).cos(),
                    0.3 * (t * 2.0 + phase).sin(),
                )
            })
            .collect();
        tracks.rotation = Some(RawTrack {
            interpolation,
            times: times.clone(),
            values: if interpolation == Interpolation::CubicSpline {
                rotations
                    .iter()
                    .flat_map(|&q| {
                        [
                            Quat::from_xyzw(0.0, 0.0, 0.0, 0.0),
                            q,
                            Quat::from_xyzw(0.0, 0.0, 0.0, 0.0),
                        ]
                    })
                    .collect()
            } else {
                rotations
            },
        });
        if j == 0 {
            tracks.translation = Some(RawTrack {
                interpolation,
                times: times.clone(),
                values: per_key(
                    times
                        .iter()
                        .map(|&t| Vec3::new(t * 1.5, 0.1 * (t * 9.0).sin(), 0.0))
                        .collect(),
                ),
            });
        }
        if j == 3 {
            tracks.scale = Some(RawTrack {
                interpolation,
                times: times.clone(),
                values: per_key(
                    times
                        .iter()
                        .map(|&t| Vec3::splat(1.0 + 0.2 * (t * 3.0).sin()))
                        .collect(),
                ),
            });
        }
        if j == 1 {
            // Constant, but not the rest translation.
            tracks.translation = Some(RawTrack {
                interpolation: Interpolation::Linear,
                times: vec![0.0, seconds],
                values: vec![Vec3::new(0.05, 0.3, 0.0); 2],
            });
        }
    }
    RawClip {
        name: "wave".into(),
        duration: seconds,
        joints,
    }
}

/// Object-space positions of every joint's virtual vertices (origin and three axis
/// points at `shell`).
fn vertices(skeleton: &Skeleton, pose: &Pose, shell: f32) -> Vec<Vec3A> {
    let mut model = vec![Affine3A::IDENTITY; skeleton.joint_count()];
    local_to_model(skeleton, pose, &Affine3A::IDENTITY, &mut model);
    model
        .iter()
        .flat_map(|m| {
            [
                Vec3A::ZERO,
                Vec3A::X * shell,
                Vec3A::Y * shell,
                Vec3A::Z * shell,
            ]
            .map(|v| m.transform_point3a(v))
        })
        .collect()
}

fn max_distance(a: &[Vec3A], b: &[Vec3A]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(a, b)| (*a - *b).length())
        .fold(0.0, f32::max)
}

#[test]
fn compressed_clips_stay_within_their_error_bound() {
    for interpolation in [Interpolation::Linear, Interpolation::CubicSpline] {
        let skeleton = chain(9);
        let raw = wave(&skeleton, 2.0, interpolation);
        for threshold in [1e-3f32, 1e-4, 1e-5] {
            let settings = CompressSettings {
                error_threshold: threshold,
                // The bones' length: the test measures every joint at it.
                shell_distance: 0.3,
                ..CompressSettings::default()
            };
            let (clip, stats) = compress(&raw, &skeleton, &settings);
            println!(
                "{interpolation:?} at {threshold}: {} -> {} bytes ({:.1}x of uniform f32), {:.2} bits mean, error {:.2e}, {} Hz, tracks d/c/a {}/{}/{}",
                stats.uniform_bytes,
                stats.compressed_bytes,
                stats.uniform_bytes as f32 / stats.compressed_bytes as f32,
                stats.mean_bits,
                stats.max_error,
                stats.sample_rate,
                stats.default_tracks,
                stats.constant_tracks,
                stats.animated_tracks
            );
            assert!(
                stats.max_error <= threshold,
                "{} > {threshold}",
                stats.max_error
            );
            assert_eq!(stats.constant_tracks, 1, "joint 1's translation");
            // Every scale but joint 3's, and translations of joints 2.. are default.
            assert_eq!(stats.default_tracks, 8 + 7);
            // Independently: the runtime sampler against the source at every sample and
            // between samples (shell distance 0.3, the bones' length here).
            let mut pose = Pose::identity(skeleton.joint_count());
            let mut source = Pose::identity(skeleton.joint_count());
            let samples = clip.sample_count();
            let mut worst = 0.0f32;
            for k in 0..2 * samples - 1 {
                let t = clip.duration() * k as f32 / (2 * samples - 2) as f32;
                clip.sample(t, skeleton.rest(), &mut pose);
                raw.sample(&skeleton, t, &mut source);
                worst = worst.max(max_distance(
                    &vertices(&skeleton, &pose, 0.3),
                    &vertices(&skeleton, &source, 0.3),
                ));
            }
            assert!(worst <= threshold * 1.05, "{worst} > {threshold}");
            // Smaller thresholds cost more bits.
            assert!(clip.size_bytes() < stats.uniform_bytes);
            // The cooked bytes load back to the same clip.
            let back = CompressedClip::from_bytes(&clip.to_bytes()).unwrap();
            assert_eq!(back, clip);
        }
    }
}

#[test]
fn damaged_clips_are_refused() {
    let skeleton = chain(4);
    let (clip, _) = compress(
        &wave(&skeleton, 1.0, Interpolation::Linear),
        &skeleton,
        &CompressSettings::default(),
    );
    let bytes = clip.to_bytes();
    assert!(CompressedClip::from_bytes(&bytes[..bytes.len() - 9]).is_err());
    assert!(CompressedClip::from_bytes(&bytes[4..]).is_err());
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(CompressedClip::from_bytes(&extra).is_err());
}

#[test]
fn sampling_matches_the_source_at_its_keys() {
    // Keys at 10 Hz, linear: the compressed clip samples at the keys exactly (up to
    // quantization at 1e-6).
    let skeleton = chain(3);
    let mut raw = RawClip {
        name: "keys".into(),
        duration: 1.0,
        joints: vec![RawJointTracks::default(); 3],
    };
    let times: Vec<f32> = (0..=10).map(|k| k as f32 / 10.0).collect();
    raw.joints[2].rotation = Some(RawTrack {
        interpolation: Interpolation::Linear,
        times: times.clone(),
        values: times
            .iter()
            .map(|&t| Quat::from_rotation_x(t * t * 3.0))
            .collect(),
    });
    // 10 µm at a shell distance of 1: rotations within 1e-5 radians.
    let settings = CompressSettings {
        error_threshold: 1e-5,
        shell_distance: 1.0,
        ..CompressSettings::default()
    };
    let (clip, stats) = compress(&raw, &skeleton, &settings);
    // The key rate, or a multiple where interpolating between keys (nlerp against the
    // source's slerp) missed: the keys stay samples.
    assert_eq!(stats.sample_rate % 10.0, 0.0, "{}", stats.sample_rate);
    for &t in &times {
        let q = clip.sample_joint(2, t, skeleton.rest().get(2)).rotation;
        assert!(
            rotation_angle(q, Quat::from_rotation_x(t * t * 3.0)) < 1.1e-5,
            "at {t}"
        );
    }
    // Between keys: nlerp of the neighbours (slerp in the source differs by far less
    // than a microradian at these steps).
    let q = clip.sample_joint(2, 0.55, skeleton.rest().get(2)).rotation;
    let expected = Quat::from_rotation_x(0.25 * 3.0).slerp(Quat::from_rotation_x(0.36 * 3.0), 0.5);
    assert!(rotation_angle(q, expected) < 1e-4);
    // Joints without tracks keep the rest pose; past the end, the last sample.
    assert_eq!(
        clip.sample_joint(1, 0.3, skeleton.rest().get(1)),
        skeleton.rest().get(1)
    );
    let end = clip.sample_joint(2, 5.0, skeleton.rest().get(2)).rotation;
    assert!(rotation_angle(end, Quat::from_rotation_x(3.0)) < 1e-5);
}

#[test]
fn additive_clips_add_back_their_motion() {
    let skeleton = chain(6);
    let raw = wave(&skeleton, 1.0, Interpolation::Linear);
    let settings = CompressSettings {
        additive: Some(AdditiveBase::FirstSample),
        // Measured below at 0.3 from every joint.
        shell_distance: 0.3,
        ..CompressSettings::default()
    };
    let (additive, stats) = compress(&raw, &skeleton, &settings);
    assert!(additive.is_additive());
    assert!(stats.max_error <= settings.error_threshold);
    // The first sample as the base, the additive clip on top at weight 1: the source.
    let n = skeleton.joint_count();
    let (mut base, mut delta, mut out, mut source) = (
        Pose::identity(n),
        Pose::identity(n),
        Pose::identity(n),
        Pose::identity(n),
    );
    raw.sample(&skeleton, 0.0, &mut base);
    let mut blender = Blender::new(n);
    for t in [0.0, 0.37, 0.8] {
        additive.sample(t, skeleton.rest(), &mut delta);
        let layers = [
            Layer {
                pose: &base,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Normal,
            },
            Layer {
                pose: &delta,
                weight: 1.0,
                mask: None,
                mode: LayerMode::Additive,
            },
        ];
        blender.blend(skeleton.rest(), &layers, &mut out);
        raw.sample(&skeleton, t, &mut source);
        let error = max_distance(
            &vertices(&skeleton, &out, 0.3),
            &vertices(&skeleton, &source, 0.3),
        );
        assert!(error < 2e-4, "at {t}: {error}");
    }
}

/// A leg: hip at the origin, knee 1 below, ankle 1 below that.
fn leg() -> Skeleton {
    let down = |name: &str, parent| JointDesc {
        name: name.into(),
        parent,
        rest: Transform::new(Vec3::new(0.0, -1.0, 0.0), Quat::IDENTITY, Vec3::ONE),
    };
    let mut joints = vec![JointDesc {
        name: "hip".into(),
        parent: None,
        rest: Transform::new(
            Vec3::new(0.0, 2.0, 0.0),
            Quat::from_rotation_y(0.3),
            Vec3::splat(1.0),
        ),
    }];
    joints.push(down("knee", Some(0)));
    joints.push(down("ankle", Some(1)));
    joints.push(down("toe", Some(2)));
    Skeleton::new(&joints).unwrap().0
}

fn model_of(skeleton: &Skeleton, pose: &Pose, root: &Affine3A) -> Vec<Affine3A> {
    let mut model = vec![Affine3A::IDENTITY; skeleton.joint_count()];
    local_to_model(skeleton, pose, root, &mut model);
    model
}

#[test]
fn two_bone_ik_reaches_or_clamps() {
    let skeleton = leg();
    let root = Affine3A::from_translation(Vec3::new(5.0, 0.0, 0.0));
    for (target, pole) in [
        (Vec3::new(5.5, 1.0, 0.3), Vec3::new(5.0, 1.0, 3.0)),
        (Vec3::new(4.2, 1.6, -0.4), Vec3::new(5.0, 1.0, 3.0)),
        (Vec3::new(5.0, 0.5, 0.0), Vec3::new(5.0, 1.0, -3.0)),
    ] {
        let mut pose = skeleton.rest().clone();
        let mut model = model_of(&skeleton, &pose, &root);
        let ik = TwoBoneIk {
            start: 0,
            mid: 1,
            end: 2,
            target,
            pole,
            weight: 1.0,
        };
        let outcome = ik.solve(&skeleton, &mut pose, &mut model, &root);
        assert!(outcome.reached);
        assert!(outcome.distance < 1e-5, "{}", outcome.distance);
        // The pose and the model transforms agree, bones keep their lengths, the knee
        // bends towards the pole.
        let again = model_of(&skeleton, &pose, &root);
        let p = |m: &[Affine3A], j: usize| Vec3::from(m[j].translation);
        assert!((p(&again, 2) - target).length() < 1e-5);
        assert!(((p(&again, 1) - p(&again, 0)).length() - 1.0).abs() < 1e-5);
        assert!(((p(&again, 2) - p(&again, 1)).length() - 1.0).abs() < 1e-5);
        let hip = p(&again, 0);
        let axis = (target - hip).normalize();
        let knee_side = (p(&again, 1) - hip).reject_from_normalized(axis);
        let pole_side = (pole - hip).reject_from_normalized(axis);
        assert!(knee_side.dot(pole_side) > 0.0);
    }
    // Out of reach: the leg straight towards the target.
    let mut pose = skeleton.rest().clone();
    let mut model = model_of(&skeleton, &pose, &root);
    let target = Vec3::new(9.0, 2.0, 0.0);
    let ik = TwoBoneIk {
        start: 0,
        mid: 1,
        end: 2,
        target,
        pole: Vec3::new(5.0, 2.0, 3.0),
        weight: 1.0,
    };
    let outcome = ik.solve(&skeleton, &mut pose, &mut model, &root);
    assert!(!outcome.reached);
    let hip = Vec3::from(model[0].translation);
    let ankle = Vec3::from(model[2].translation);
    assert!((ankle - (hip + (target - hip).normalize() * 2.0)).length() < 1e-5);
    assert!((outcome.distance - ((target - hip).length() - 2.0)).abs() < 1e-5);
    // Weight 0 changes nothing.
    let mut pose = skeleton.rest().clone();
    let mut model = model_of(&skeleton, &pose, &root);
    let before = model.clone();
    TwoBoneIk { weight: 0.0, ..ik }.solve(&skeleton, &mut pose, &mut model, &root);
    assert!(
        model
            .iter()
            .zip(&before)
            .all(|(a, b)| a.abs_diff_eq(*b, 1e-6))
    );
}

#[test]
fn fabrik_reaches_or_points_at_the_target() {
    let skeleton = chain(6);
    let joints: Vec<usize> = (0..6).collect();
    let root = Affine3A::IDENTITY;
    let mut fabrik = Fabrik::new();
    let mut pose = skeleton.rest().clone();
    let mut model = model_of(&skeleton, &pose, &root);
    let target = Vec3::new(0.6, 0.9, 0.4);
    let outcome = fabrik.solve(
        &skeleton, &mut pose, &mut model, &root, &joints, target, 1e-4, 64,
    );
    assert!(outcome.reached, "{outcome:?}");
    let again = model_of(&skeleton, &pose, &root);
    assert!((Vec3::from(again[5].translation) - target).length() < 1.1e-4);
    for j in 1..6 {
        let bone = (again[j].translation - again[j - 1].translation).length();
        assert!((bone - 0.3).abs() < 1e-5);
    }
    // Out of reach (the chain is 1.5 long): straight at the target.
    let mut pose = skeleton.rest().clone();
    let mut model = model_of(&skeleton, &pose, &root);
    let target = Vec3::new(3.0, 0.0, 0.0);
    let outcome = fabrik.solve(
        &skeleton, &mut pose, &mut model, &root, &joints, target, 1e-4, 64,
    );
    assert!(!outcome.reached);
    assert!((Vec3::from(model[5].translation) - Vec3::new(1.5, 0.0, 0.0)).length() < 1e-4);
}

#[test]
fn look_at_aims_within_its_limit() {
    let skeleton = leg();
    let root = Affine3A::IDENTITY;
    let mut pose = skeleton.rest().clone();
    let mut model = model_of(&skeleton, &pose, &root);
    let look = LookAt {
        joint: 1,
        forward: Vec3::Z,
        target: Vec3::new(3.0, 0.0, 2.0),
        weight: 1.0,
        max_angle: 3.0,
    };
    let left = look.solve(&skeleton, &mut pose, &mut model, &root);
    assert!(left < 1e-3, "{left}");
    let again = model_of(&skeleton, &pose, &root);
    let forward = again[1].transform_vector3(Vec3::Z);
    assert!(forward.angle_between(look.target - Vec3::from(again[1].translation)) < 1e-3);
    // Limited: at most 0.2 radians of turn.
    let mut pose = skeleton.rest().clone();
    let mut model = model_of(&skeleton, &pose, &root);
    let before = model[1].transform_vector3(Vec3::Z);
    LookAt {
        max_angle: 0.2,
        ..look
    }
    .solve(&skeleton, &mut pose, &mut model, &root);
    let after = model[1].transform_vector3(Vec3::Z);
    assert!((before.angle_between(after) - 0.2).abs() < 1e-3);
}

#[test]
fn root_motion_moves_the_character_not_the_pose() {
    let skeleton = chain(3);
    let mut raw = RawClip {
        name: "walk".into(),
        duration: 2.0,
        joints: vec![RawJointTracks::default(); 3],
    };
    // 1.5 units per second along +X, a quarter turn of yaw over the clip, a bob in y.
    let times: Vec<f32> = (0..=60).map(|k| k as f32 / 30.0).collect();
    raw.joints[0].translation = Some(RawTrack {
        interpolation: Interpolation::Linear,
        times: times.clone(),
        values: times
            .iter()
            .map(|&t| Vec3::new(1.5 * t, 0.1 * (t * 6.0).sin(), 0.0))
            .collect(),
    });
    raw.joints[0].rotation = Some(RawTrack {
        interpolation: Interpolation::Linear,
        times: times.clone(),
        values: times
            .iter()
            .map(|&t| Quat::from_rotation_y(t * core::f32::consts::FRAC_PI_4))
            .collect(),
    });
    let (clip, _) = compress(&raw, &skeleton, &CompressSettings::default());
    let mode = RootMotionMode::default();
    let m = root_motion(&clip, &skeleton, 0, 0.0, 1.0, false, mode);
    // In the frame at t = 0 (no yaw yet): 1.5 along x, a yaw of π/4.
    assert!(
        (m.translation - Vec3::new(1.5, 0.0, 0.0)).length() < 1e-3,
        "{m:?}"
    );
    assert!(
        rotation_angle(
            m.rotation,
            Quat::from_rotation_y(core::f32::consts::FRAC_PI_4)
        ) < 1e-3
    );
    // From 1 to 2: 1.5 more, seen from the character turned by π/4.
    let m2 = root_motion(&clip, &skeleton, 0, 1.0, 2.0, false, mode);
    let expected = Quat::from_rotation_y(-core::f32::consts::FRAC_PI_4) * Vec3::new(1.5, 0.0, 0.0);
    assert!((m2.translation - expected).length() < 1e-3, "{m2:?}");
    // Composed, the halves make the whole.
    let whole = root_motion(&clip, &skeleton, 0, 0.0, 2.0, false, mode);
    let composed = m.then(m2);
    assert!((whole.translation - composed.translation).length() < 1e-3);
    // Looping from 1.5 past the end to 0.5: the last half second plus the first.
    let wrapped = root_motion(&clip, &skeleton, 0, 1.5, 0.5, true, mode);
    let expected = root_motion(&clip, &skeleton, 0, 1.5, 2.0, false, mode)
        .then(root_motion(&clip, &skeleton, 0, 0.0, 0.5, false, mode));
    assert!((wrapped.translation - expected.translation).length() < 1e-5);
    // Removed from the pose: x, z and yaw back to the start, the bob kept.
    let mut pose = Pose::identity(3);
    clip.sample(1.3, skeleton.rest(), &mut pose);
    remove_root_motion(&mut pose, &clip, &skeleton, 0, mode);
    let t = pose.get(0);
    assert!(
        t.translation.x.abs() < 1e-3 && t.translation.z.abs() < 1e-3,
        "{t:?}"
    );
    assert!((t.translation.y - 0.1 * (1.3f32 * 6.0).sin()).abs() < 1e-3);
    assert!(rotation_angle(yaw_of(t.rotation), Quat::IDENTITY) < 1e-3);
    // Over a span of play (ADR 0124's E12-2c): wrapping as often as it must, backwards
    // undoing the forward motion.
    let over = root_motion_over(&clip, &skeleton, 0, 1.5, 2.5, mode);
    assert!((over.translation - wrapped.translation).length() < 1e-5);
    let twice = root_motion_over(&clip, &skeleton, 0, 0.0, 4.0, mode);
    assert!((twice.translation - whole.then(whole).translation).length() < 1e-3);
    let back = root_motion_over(&clip, &skeleton, 0, 2.5, 1.5, mode);
    let undone = over.then(back);
    assert!(undone.translation.length() < 1e-4, "{undone:?}");
    assert!(rotation_angle(undone.rotation, Quat::IDENTITY) < 1e-4);
    assert_eq!(
        root_motion_over(&clip, &skeleton, 0, 1.0, 1.0, mode),
        RootMotion::NONE
    );
    // Blended by direction and length; the turn read about the up.
    let side = RootMotion {
        translation: Vec3::new(0.0, 0.0, 3.0),
        rotation: Quat::from_rotation_y(-0.5),
    };
    let half = RootMotion::blend(m, side, 0.5);
    let direction = (Vec3::X + Vec3::Z).normalize();
    assert!(
        (half.translation - direction * 2.25).length() < 1e-3,
        "{half:?}"
    );
    assert!((m.angle_about(Vec3::Y) - core::f32::consts::FRAC_PI_4).abs() < 1e-3);
    assert!((side.angle_about(Vec3::Y) + 0.5).abs() < 1e-5);
    let none = RootMotion::blend(m, side, 0.0);
    assert!(
        (none.translation - m.translation).length() < 1e-5
            && rotation_angle(none.rotation, m.rotation) < 1e-5
    );
}

/// E12-2c: a root under an armature standing Z up: its ground is x and y, its turn about z.
#[test]
fn root_motion_follows_the_characters_up() {
    let skeleton = chain(2);
    let mut raw = RawClip {
        name: "walk".into(),
        duration: 1.0,
        joints: vec![RawJointTracks::default(); 2],
    };
    let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0).collect();
    raw.joints[0].translation = Some(RawTrack {
        interpolation: Interpolation::Linear,
        times: times.clone(),
        values: times
            .iter()
            .map(|&t| Vec3::new(0.0, 2.0 * t, 0.1 * (t * 6.0).sin()))
            .collect(),
    });
    raw.joints[0].rotation = Some(RawTrack {
        interpolation: Interpolation::Linear,
        times: times.clone(),
        values: times
            .iter()
            .map(|&t| Quat::from_rotation_z(0.5 * t))
            .collect(),
    });
    let (clip, _) = compress(&raw, &skeleton, &CompressSettings::default());
    let mode = RootMotionMode {
        up: Vec3::Z,
        ..RootMotionMode::default()
    };
    let m = root_motion(&clip, &skeleton, 0, 0.0, 1.0, false, mode);
    assert!(
        (m.translation - Vec3::new(0.0, 2.0, 0.0)).length() < 1e-3,
        "{m:?}"
    );
    assert!((m.angle_about(Vec3::Z) - 0.5).abs() < 1e-3);
    // Read with +Y up, the same clip crosses no ground.
    let flat = root_motion(
        &clip,
        &skeleton,
        0,
        0.0,
        1.0,
        false,
        RootMotionMode::default(),
    );
    assert!(flat.translation.y.abs() < 1e-3, "{flat:?}");
    // Taken out of the pose: x and y back at the start, the bob along z kept.
    let mut pose = Pose::identity(2);
    clip.sample(0.7, skeleton.rest(), &mut pose);
    remove_root_motion(&mut pose, &clip, &skeleton, 0, mode);
    let t = pose.get(0);
    assert!(
        t.translation.x.abs() < 1e-3 && t.translation.y.abs() < 1e-3,
        "{t:?}"
    );
    assert!((t.translation.z - 0.1 * (0.7f32 * 6.0).sin()).abs() < 1e-3);
    assert!(rotation_angle(twist_of(t.rotation, Vec3::Z), Quat::IDENTITY) < 1e-3);
}
