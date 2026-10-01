//! Scratch.
#![allow(clippy::unwrap_used, clippy::print_stdout)]
use engine_anim::*;
use engine_assets::animation::{raw_clip, shell_distances, skin_skeleton};
use engine_core::math::{Affine3A, Vec3A};
fn main() {
    let path = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let bytes = std::fs::read(&path).unwrap();
    let scene = engine_assets::import_gltf(&bytes, path.parent()).unwrap();
    let sk = skin_skeleton(&scene, 0).unwrap();
    let shell = shell_distances(&scene, 0, &sk);
    let n = sk.skeleton.joint_count();
    for a in 0..scene.animations.len() {
        let raw = raw_clip(&scene, a, &sk).unwrap();
        for (j, t) in raw.joints.iter().enumerate() {
            if let Some(r) = &t.rotation { if j < 6 { println!("joint {j} rot {:?} keys {} first times {:?}", r.interpolation, r.times.len(), &r.times[..r.times.len().min(4)]); } }
            if let Some(r) = &t.translation { println!("joint {j} trans {:?} keys {} ", r.interpolation, r.times.len()); }
            if let Some(r) = &t.scale { println!("joint {j} scale {:?} keys {} ", r.interpolation, r.times.len()); }
        }
        println!("{} duration {} min interval {:?}", raw.name, raw.duration, raw.min_key_interval());
        // nlerp of the source keys vs source at midpoints
        let rate = 1.0 / raw.min_key_interval().unwrap();
        let samples = (raw.duration * rate).round() as usize + 1;
        let mut worst = (0.0f32, 0, 0.0);
        let (mut p0, mut p1, mut pm, mut src) = (Pose::identity(n), Pose::identity(n), Pose::identity(n), Pose::identity(n));
        let (mut m1, mut m2) = (vec![Affine3A::IDENTITY; n], vec![Affine3A::IDENTITY; n]);
        for s in 0..samples - 1 {
            let t0 = raw.duration * s as f32 / (samples - 1) as f32;
            let t1 = raw.duration * (s + 1) as f32 / (samples - 1) as f32;
            raw.sample(&sk.skeleton, t0, &mut p0);
            raw.sample(&sk.skeleton, t1, &mut p1);
            raw.sample(&sk.skeleton, 0.5 * (t0 + t1), &mut src);
            for j in 0..n {
                pm.translations[j] = p0.translations[j].lerp(p1.translations[j], 0.5);
                pm.scales[j] = p0.scales[j].lerp(p1.scales[j], 0.5);
                pm.rotations[j] = nlerp(p0.rotations[j], p1.rotations[j], 0.5);
            }
            local_to_model(&sk.skeleton, &pm, &Affine3A::IDENTITY, &mut m1);
            local_to_model(&sk.skeleton, &src, &Affine3A::IDENTITY, &mut m2);
            for j in 0..n {
                let d = shell[j].max(0.03);
                for v in [Vec3A::ZERO, Vec3A::X * d, Vec3A::Y * d, Vec3A::Z * d] {
                    let e = (m1[j].transform_point3a(v) - m2[j].transform_point3a(v)).length();
                    if e > worst.0 { worst = (e, j, t0); }
                }
            }
        }
        println!("  interpolation-only error at midpoints {:?} (rate {rate})", worst);
    }
    println!("shell {:?}", shell);
    for a in 0..scene.animations.len() {
        let raw = raw_clip(&scene, a, &sk).unwrap();
        for rate in [None, Some(24.0)] {
            let settings = CompressSettings { error_threshold: 7.29e-3, joint_shell_distances: shell.clone(), sample_rate: rate, ..CompressSettings::default() };
            let (clip, st) = compress(&raw, &sk.skeleton, &settings);
            println!("{} rate {:?}: {:?}", raw.name, rate, st);
            if raw.name == "Walk" && rate.is_some() {
                let (mut a, mut b) = (Pose::identity(n), Pose::identity(n));
                let (mut ma, mut mb) = (vec![Affine3A::IDENTITY; n], vec![Affine3A::IDENTITY; n]);
                for k in 0..=34 {
                    let t = raw.duration * k as f32 / 34.0;
                    clip.sample(t, sk.skeleton.rest(), &mut a);
                    raw.sample(&sk.skeleton, t, &mut b);
                    local_to_model(&sk.skeleton, &a, &Affine3A::IDENTITY, &mut ma);
                    local_to_model(&sk.skeleton, &b, &Affine3A::IDENTITY, &mut mb);
                    let j = 24;
                    let e = (ma[j].translation - mb[j].translation).length();
                    let mut local = vec![];
                    for i in [3usize, 21, 22, 23, 24] {
                        local.push((i, rotation_angle(a.rotations[i], b.rotations[i]), (a.translations[i]-b.translations[i]).length(), (a.scales[i]-b.scales[i]).length()));
                    }
                    println!("t {t:.3} e {e:.4} {:?}", local);
                }
            }
        }
    }
}

#[allow(dead_code)]
fn unused() {}
