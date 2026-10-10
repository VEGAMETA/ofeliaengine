//! Skins and animations of the Khronos sample models (`tools/fetch-test-assets.ps1`;
//! skipped without them, unless `ENGINE_REQUIRE_GOLDEN=1`): the importer against the
//! accessors read directly with the `gltf` crate, compressed clips against the source
//! within their error bound in object space, cooking and packs.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::float_cmp)]

use engine_anim::{CompressSettings, Pose, compress, local_to_model, rotation_angle};
use engine_assets::animation::{raw_clip, shell_distances, skin_skeleton};
use engine_assets::{CookSettings, cook_scene, import_gltf, load_pack, write_pack};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Vec3A};
use std::path::{Path, PathBuf};

fn assets() -> PathBuf {
    std::env::var_os("ENGINE_TEST_ASSETS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets"),
        PathBuf::from,
    )
}

fn read(name: &str) -> Option<(Vec<u8>, PathBuf)> {
    let path = assets().join(name);
    match std::fs::read(&path) {
        Ok(bytes) => Some((bytes, path)),
        Err(_) => {
            assert!(
                !std::env::var("ENGINE_REQUIRE_GOLDEN").is_ok_and(|v| v == "1"),
                "{} missing (tools/fetch-test-assets.ps1)",
                path.display()
            );
            println!("skipped: {} missing", path.display());
            None
        }
    }
}

const MODELS: [&str; 5] = [
    "Fox/Fox.gltf",
    "CesiumMan/CesiumMan.gltf",
    "RiggedFigure/RiggedFigure.gltf",
    "RiggedSimple/RiggedSimple.gltf",
    "SimpleSkin/SimpleSkin.gltf",
];

#[test]
fn the_importer_reads_what_the_accessors_hold() {
    let Some((bytes, path)) = read("Fox/Fox.gltf") else {
        return;
    };
    let dir = path.parent();
    let scene = import_gltf(&bytes, dir).unwrap();
    assert_eq!(scene.skins.len(), 1);
    let names: Vec<&str> = scene.animations.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["Survey", "Walk", "Run"]);
    // Independently, with the gltf crate: every rotation key of every channel.
    let document = gltf::Gltf::from_slice(&bytes).unwrap().document;
    let buffers: Vec<Vec<u8>> = document
        .buffers()
        .map(|b| match b.source() {
            gltf::buffer::Source::Uri(uri) => std::fs::read(path.with_file_name(uri)).unwrap(),
            gltf::buffer::Source::Bin => unreachable!("Fox.gltf has no GLB chunk"),
        })
        .collect();
    for (a, animation) in document.animations().enumerate() {
        for (c, channel) in animation.channels().enumerate() {
            let reader = channel.reader(|b| Some(buffers[b.index()].as_slice()));
            let times: Vec<f32> = reader.read_inputs().unwrap().collect();
            let ours = &scene.animations[a].channels[c];
            assert_eq!(ours.node, channel.target().node().index());
            if let gltf::animation::util::ReadOutputs::Rotations(r) = reader.read_outputs().unwrap()
            {
                let values: Vec<[f32; 4]> = r.into_f32().collect();
                let engine_assets::ChannelKeys::Rotation(track) = &ours.keys else {
                    panic!("channel {c} of animation {a} is a rotation");
                };
                assert_eq!(track.times, times);
                for (q, v) in track.values.iter().zip(&values) {
                    assert_eq!(q.to_array(), *v);
                }
            }
        }
    }
    // Every skinned vertex: joints and weights as stored.
    let mesh = document.meshes().next().unwrap();
    let primitive = mesh.primitives().next().unwrap();
    let reader = primitive.reader(|b| Some(buffers[b.index()].as_slice()));
    let joints: Vec<[u16; 4]> = reader.read_joints(0).unwrap().into_u16().collect();
    let ours = &scene.meshes[0].primitives[0];
    assert_eq!(ours.joints, joints);
    assert_eq!(ours.influence_sets(), 1);
}

#[test]
fn clips_sample_the_source_within_their_bound() {
    for model in MODELS {
        let Some((bytes, path)) = read(model) else {
            continue;
        };
        let scene = import_gltf(&bytes, path.parent()).unwrap();
        for skin in 0..scene.skins.len() {
            let skeleton = skin_skeleton(&scene, skin).unwrap();
            let shell = shell_distances(&scene, skin, &skeleton);
            let joints = skeleton.skeleton.joint_count();
            // The skin's joints map to joints whose bind pose matches the inverse bind
            // matrices: model(rest) · inverse bind ≈ identity.
            let mut bind = vec![Affine3A::IDENTITY; joints];
            local_to_model(
                &skeleton.skeleton,
                skeleton.skeleton.rest(),
                &Affine3A::IDENTITY,
                &mut bind,
            );
            let size = bind
                .iter()
                .map(|m| m.translation.length())
                .fold(0.0f32, f32::max)
                .max(1e-3);
            for (k, &j) in skeleton.skin_joints.iter().enumerate() {
                let m = bind[j as usize] * skeleton.inverse_bind[k];
                let off = m.transform_point3a(Vec3A::ZERO).length() / size;
                assert!(
                    off < 1e-2,
                    "{model}: skin joint {k} is not in its bind pose ({off})"
                );
            }
            for animation in 0..scene.animations.len() {
                let Some(raw) = raw_clip(&scene, animation, &skeleton) else {
                    continue;
                };
                // Threshold relative to the model's size: 0.01 % of it.
                let threshold = size * 1e-4;
                let settings = CompressSettings {
                    error_threshold: threshold,
                    joint_shell_distances: shell.clone(),
                    ..CompressSettings::default()
                };
                let (clip, stats) = compress(&raw, &skeleton.skeleton, &settings);
                println!(
                    "{model} `{}`: {} joints, {:.2} s at {:.0} Hz; source {} B, uniform f32 {} B, compressed {} B ({:.1}x source, {:.1}x uniform), {:.1} bits, error {:.3e} (bound {:.3e})",
                    raw.name,
                    joints,
                    raw.duration,
                    stats.sample_rate,
                    stats.source_bytes,
                    stats.uniform_bytes,
                    stats.compressed_bytes,
                    stats.source_bytes as f32 / stats.compressed_bytes as f32,
                    stats.uniform_bytes as f32 / stats.compressed_bytes as f32,
                    stats.mean_bits,
                    stats.max_error,
                    threshold
                );
                assert!(
                    stats.max_error <= threshold,
                    "{model}: {} > {threshold}",
                    stats.max_error
                );
                // At every key of the source: the compressed pose against the source's,
                // through the hierarchy, on each joint's farthest skinned vertex.
                let mut times: Vec<f32> = raw
                    .joints
                    .iter()
                    .flat_map(|j| {
                        j.rotation
                            .iter()
                            .flat_map(|t| t.times.iter().copied())
                            .chain(
                                j.translation
                                    .iter()
                                    .chain(j.scale.iter())
                                    .flat_map(|t| t.times.iter().copied()),
                            )
                    })
                    .collect();
                times.sort_by(f32::total_cmp);
                times.dedup();
                let (mut a, mut b) = (Pose::identity(joints), Pose::identity(joints));
                let (mut ma, mut mb) = (bind.clone(), bind.clone());
                let mut worst = 0.0f32;
                for &t in &times {
                    clip.sample(t, skeleton.skeleton.rest(), &mut a);
                    raw.sample(&skeleton.skeleton, t, &mut b);
                    local_to_model(&skeleton.skeleton, &a, &Affine3A::IDENTITY, &mut ma);
                    local_to_model(&skeleton.skeleton, &b, &Affine3A::IDENTITY, &mut mb);
                    for j in 0..joints {
                        let d = shell[j].max(settings.shell_distance);
                        for v in [Vec3A::ZERO, Vec3A::X * d, Vec3A::Y * d, Vec3A::Z * d] {
                            let e =
                                (ma[j].transform_point3a(v) - mb[j].transform_point3a(v)).length();
                            worst = worst.max(e);
                        }
                    }
                    // Joints the clip does not move keep their rest pose exactly.
                    for j in 0..joints {
                        let tracks = &raw.joints[j];
                        if tracks.rotation.is_none() {
                            assert!(
                                rotation_angle(
                                    a.rotations[j],
                                    skeleton.skeleton.rest().rotations[j]
                                ) < 1e-6
                            );
                        }
                    }
                }
                // Keys fall between samples when the rate is not the source's: within
                // the bound up to the interpolation between them.
                assert!(
                    worst <= threshold * 2.0,
                    "{model}: error {worst} at the keys"
                );
            }
        }
    }
}

#[test]
fn skinned_scenes_cook_into_packs() {
    let jobs = ThreadPool::builder().workers(3).build().unwrap();
    for model in ["Fox/Fox.gltf", "CesiumMan/CesiumMan.gltf"] {
        let Some((bytes, path)) = read(model) else {
            continue;
        };
        let scene = import_gltf(&bytes, path.parent()).unwrap();
        let settings = CookSettings {
            lods: false,
            compression_level: 3,
            block_compression: false,
            ..CookSettings::default()
        };
        let (cooked, stats) = cook_scene(&scene, path.parent(), None, &jobs, &settings).unwrap();
        println!(
            "{model}: {} clips, {} B of keys -> {} B ({:.1}x), largest error {:.2e}, {:.1} ms",
            stats.clips,
            stats.clip_source_bytes,
            stats.clip_bytes,
            stats.clip_source_bytes as f32 / stats.clip_bytes.max(1) as f32,
            stats.clip_error,
            stats.animation_seconds * 1e3
        );
        assert_eq!(cooked.skins.len(), 1);
        assert_eq!(cooked.skins[0].clips.len(), scene.animations.len());
        assert!(cooked.instances.iter().any(|i| i.skin == Some(0)));
        for i in cooked.instances.iter().filter(|i| i.skin.is_some()) {
            let mesh = &cooked.meshes[i.mesh as usize];
            assert_eq!(mesh.joints.len(), mesh.positions.len());
            for w in &mesh.weights {
                assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-5);
            }
        }
        let pack = std::env::temp_dir().join(format!(
            "engine_assets_anim_{}_{}.pack",
            std::process::id(),
            model.replace('/', "_")
        ));
        write_pack(&pack, &cooked, &jobs, 3).unwrap();
        let (loaded, _) = load_pack(&pack, &jobs, 4).unwrap();
        assert_eq!(loaded.skins, cooked.skins);
        assert_eq!(loaded.meshes, cooked.meshes);
        assert_eq!(loaded.instances, cooked.instances);
        let _ = std::fs::remove_file(pack);
    }
}
