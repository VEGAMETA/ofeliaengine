//! engine_anim's clips against ACL 2.1 (ADR 0068): the same uniformly sampled input, the
//! same bound and shell distances; sizes, the error measured by one metric (ours: object
//! space, virtual vertices, at every sample and between samples, against the glTF source),
//! compression time, and the time to sample a whole pose.
//!
//! `ACL_DIR=… RTM_DIR=… cargo run --release -- [test-assets dir]`

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_anim::*;
use engine_assets::animation::{raw_clip, shell_distances, skin_skeleton};
use engine_assets::import_gltf;
use engine_core::math::{Affine3A, Quat, Vec3A};
use std::ffi::c_void;
use std::path::PathBuf;
use std::time::Instant;

unsafe extern "C" {
    fn acl_compress(
        joints: u32,
        parents: *const i32,
        samples: u32,
        rate: f32,
        data: *const f32,
        shell: *const f32,
        precision: f32,
        level: u32,
        size: *mut u32,
        milliseconds: *mut f64,
    ) -> *mut c_void;
    fn acl_sample(clip: *mut c_void, time: f32, out: *mut f32);
    fn acl_free(clip: *mut c_void);
}

fn error(
    raw: &RawClip,
    skeleton: &Skeleton,
    shell: &[f32],
    samples: u32,
    sample: &mut dyn FnMut(f32, &mut Pose),
) -> f32 {
    let n = skeleton.joint_count();
    let (mut a, mut b) = (Pose::identity(n), Pose::identity(n));
    let (mut ma, mut mb) = (vec![Affine3A::IDENTITY; n], vec![Affine3A::IDENTITY; n]);
    let checkpoints = 2 * samples as usize - 1;
    let mut worst = 0.0f32;
    for k in 0..checkpoints {
        let t = raw.duration * k as f32 / (checkpoints - 1).max(1) as f32;
        sample(t, &mut a);
        raw.sample(skeleton, t, &mut b);
        local_to_model(skeleton, &a, &Affine3A::IDENTITY, &mut ma);
        local_to_model(skeleton, &b, &Affine3A::IDENTITY, &mut mb);
        for j in 0..n {
            for v in [Vec3A::ZERO, Vec3A::X, Vec3A::Y, Vec3A::Z] {
                let v = v * shell[j];
                worst = worst.max((ma[j].transform_point3a(v) - mb[j].transform_point3a(v)).length());
            }
        }
    }
    worst
}

fn main() {
    let assets = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("../../test-assets"), PathBuf::from);
    println!("| model | clip | Hz | ours B | error/bound | ms | ACL medium B | error/bound | at equal error B | ms | ACL highest B | error/bound | at equal error B | ms | ours µs/pose | ACL µs/pose |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for name in ["Fox/Fox.gltf", "CesiumMan/CesiumMan.gltf", "RiggedSimple/RiggedSimple.gltf"] {
        let path = assets.join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            println!("{name}: missing");
            continue;
        };
        let scene = import_gltf(&bytes, path.parent()).unwrap();
        let skin = skin_skeleton(&scene, 0).unwrap();
        let skeleton = &skin.skeleton;
        let n = skeleton.joint_count();
        let mut bind = vec![Affine3A::IDENTITY; n];
        local_to_model(skeleton, skeleton.rest(), &Affine3A::IDENTITY, &mut bind);
        let size = bind.iter().map(|m| m.translation.length()).fold(0.0f32, f32::max).max(1e-3);
        let bound = size * 1e-4;
        let settings_shell = CompressSettings::default().shell_distance;
        let joint_shell = shell_distances(&scene, 0, &skin);
        let shell: Vec<f32> = joint_shell.iter().map(|&d| d.max(settings_shell)).collect();
        let parents: Vec<i32> = (0..n).map(|j| skeleton.parent(j).map_or(-1, |p| p as i32)).collect();
        for a in 0..scene.animations.len() {
            let Some(raw) = raw_clip(&scene, a, &skin) else {
                continue;
            };
            let settings = CompressSettings {
                error_threshold: bound,
                joint_shell_distances: joint_shell.clone(),
                ..CompressSettings::default()
            };
            let start = Instant::now();
            let (ours, stats) = compress(&raw, skeleton, &settings);
            let ours_ms = start.elapsed().as_secs_f64() * 1e3;
            let samples = ours.sample_count();
            let rate = ours.sample_rate();
            let mut data = Vec::with_capacity(samples as usize * n * 10);
            for s in 0..samples {
                let t = raw.duration * s as f32 / (samples - 1).max(1) as f32;
                for j in 0..n {
                    let x = raw.sample_joint(j, t, skeleton.rest().get(j));
                    data.extend(x.rotation.to_array());
                    data.extend(x.translation.to_array());
                    data.extend(x.scale.to_array());
                }
            }
            let mut ours_pose = Pose::identity(n);
            let ours_error = error(&raw, skeleton, &shell, samples, &mut |t, p| ours.sample(t, skeleton.rest(), p));
            // ACL at medium and highest, its precision lowered until its error by our
            // metric is within the bound too (equal quality).
            let mut acl = Vec::new();
            for level in [2u32, 4] {
                let mut precision = bound;
                let result = loop {
                    let (mut bytes, mut ms) = (0u32, 0.0f64);
                    // SAFETY: the arrays have the sizes the shim reads.
                    let clip = unsafe {
                        acl_compress(
                            n as u32,
                            parents.as_ptr(),
                            samples,
                            rate.max(1.0),
                            data.as_ptr(),
                            shell.as_ptr(),
                            precision,
                            level,
                            &mut bytes,
                            &mut ms,
                        )
                    };
                    assert!(!clip.is_null(), "ACL failed on {name}");
                    let mut out = vec![0.0f32; n * 10];
                    let mut decode = |t: f32, p: &mut Pose| {
                        // SAFETY: `out` holds 10 floats per joint.
                        unsafe { acl_sample(clip, t.min(raw.duration), out.as_mut_ptr()) };
                        for j in 0..n {
                            let v = &out[j * 10..];
                            p.rotations[j] = Quat::from_xyzw(v[0], v[1], v[2], v[3]);
                            p.translations[j] = Vec3A::new(v[4], v[5], v[6]);
                            p.scales[j] = Vec3A::new(v[7], v[8], v[9]);
                        }
                    };
                    let e = error(&raw, skeleton, &shell, samples, &mut decode);
                    let mut pose = Pose::identity(n);
                    let start = Instant::now();
                    for k in 0..20_000 {
                        decode((k as f32 * 0.618_034) % 1.0 * raw.duration, &mut pose);
                    }
                    let us = start.elapsed().as_secs_f64() * 1e6 / 20_000.0;
                    std::hint::black_box(&pose);
                    // SAFETY: made by acl_compress.
                    unsafe { acl_free(clip) };
                    if acl.len() % 2 == 0 {
                        // The first result at the plain bound.
                        acl.push((bytes, e, ms, us));
                    }
                    if e <= bound || precision < bound * 0.01 {
                        break (bytes, e, ms, us);
                    }
                    precision *= 0.8;
                };
                acl.push(result);
            }
            let start = Instant::now();
            for k in 0..20_000 {
                ours.sample((k as f32 * 0.618_034) % 1.0 * raw.duration, skeleton.rest(), &mut ours_pose);
            }
            let ours_us = start.elapsed().as_secs_f64() * 1e6 / 20_000.0;
            std::hint::black_box(&ours_pose);
            println!(
                "| {} | {} | {:.0} | {} | {:.2} | {:.1} | {} | {:.2} | {} | {:.1} | {} | {:.2} | {} | {:.1} | {:.3} | {:.3} |",
                name.split('/').next().unwrap(),
                if raw.name.is_empty() { "(unnamed)" } else { &raw.name },
                rate,
                stats.compressed_bytes,
                ours_error / bound,
                ours_ms,
                acl[0].0,
                acl[0].1 / bound,
                acl[1].0,
                acl[0].2,
                acl[2].0,
                acl[2].1 / bound,
                acl[3].0,
                acl[2].2,
                ours_us,
                acl[0].3,
            );
        }
    }
}
