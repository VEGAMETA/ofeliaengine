//! Real content through the forward renderer, headless (ADR 0039): a glTF scene imported
//! into the geometry pool, drawn into a 1920×1080 HDR target with a shadow map from a fixed
//! camera. Reports the scene's size, the pool's memory, and the median GPU time of the
//! shadow and forward passes, for the meshes as imported (indexed, vertices shared between
//! triangles) and as a triangle soup (three vertices per triangle, what the renderer drew
//! before the pool). Materials are the glTF factors only; textures come with the texture
//! pipeline.
//!
//! `cargo run -p engine_render --release --example scene_bench -- test-assets/Sponza/Sponza.gltf`
//! (scenes: `tools/fetch-test-assets.ps1`)

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_assets::{ImportedScene, import_gltf};
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Material, ShadowSettings,
};
use engine_render::graph::{Access, RenderGraph, TextureDesc};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData, MeshId};
use engine_render::post::HDR_FORMAT;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};
use std::time::Instant;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WARMUP: usize = 30;
const FRAMES: usize = 200;

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// World transforms of every node that has a mesh: `(mesh index, world from object)`.
fn instances(scene: &ImportedScene) -> Vec<(usize, Mat4)> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, Mat4)> = scene.roots.iter().map(|&r| (r, Mat4::IDENTITY)).collect();
    while let Some((index, parent)) = stack.pop() {
        let node = &scene.nodes[index];
        let local = Mat4::from_scale_rotation_translation(
            Vec3::from(node.scale),
            Quat::from_array(node.rotation),
            Vec3::from(node.translation),
        );
        let world = parent * local;
        if let Some(mesh) = node.mesh {
            out.push((mesh, world));
        }
        stack.extend(node.children.iter().map(|&c| (c, world)));
    }
    out
}

/// `values` in the order of `indices` (empty stays empty).
fn gather<T: Copy>(values: &[T], indices: &[u32]) -> Vec<T> {
    if values.is_empty() {
        return Vec::new();
    }
    indices.iter().map(|&i| values[i as usize]).collect()
}

/// Adds every primitive to the pool, indexed or as a triangle soup; ids per mesh, per
/// primitive.
fn add_meshes(scene: &ImportedScene, pool: &mut GeometryPool, soup: bool) -> Vec<Vec<MeshId>> {
    scene
        .meshes
        .iter()
        .map(|mesh| {
            mesh.primitives
                .iter()
                .map(|p| {
                    if !soup {
                        return pool
                            .add_mesh(&MeshData {
                                positions: &p.positions,
                                normals: &p.normals,
                                tangents: &p.tangents,
                                uvs: &p.uvs,
                                indices: &p.indices,
                            })
                            .unwrap();
                    }
                    let (positions, normals) = (
                        gather(&p.positions, &p.indices),
                        gather(&p.normals, &p.indices),
                    );
                    let (tangents, uvs) =
                        (gather(&p.tangents, &p.indices), gather(&p.uvs, &p.indices));
                    let indices: Vec<u32> = (0..p.indices.len() as u32).collect();
                    pool.add_mesh(&MeshData {
                        positions: &positions,
                        normals: &normals,
                        tangents: &tangents,
                        uvs: &uvs,
                        indices: &indices,
                    })
                    .unwrap()
                })
                .collect()
        })
        .collect()
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| String::from("test-assets/Sponza/Sponza.gltf"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!("{path}: {e} (download test scenes with tools/fetch-test-assets.ps1)")
    });
    let start = Instant::now();
    let scene = import_gltf(&bytes, std::path::Path::new(&path).parent()).unwrap();
    let imported = start.elapsed();
    let placed = instances(&scene);

    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("scene_bench"),
        validation: false,
        debug_utils: true,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .unwrap();
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    let device = Device::new(&instance, &adapter).unwrap();
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let depth_format = vk::Format::D32_SFLOAT;
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 64 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    // SAFETY: plain object creation.
    let (command_pool, cmd) = unsafe {
        let command_pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
            .unwrap();
        let cmd = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .command_buffer_count(1),
            )
            .unwrap()[0];
        (command_pool, cmd)
    };

    println!("### Scene through the forward renderer ({WIDTH}×{HEIGHT} HDR, shadow map 2048²)");
    println!();
    println!("Device: {}", device.adapter().summary());
    println!(
        "Scene: {path}: {} triangles in {} primitives, {} draws; imported in {:.0} ms",
        scene.triangle_count(),
        scene
            .meshes
            .iter()
            .map(|m| m.primitives.len())
            .sum::<usize>(),
        placed
            .iter()
            .map(|&(m, _)| scene.meshes[m].primitives.len())
            .sum::<usize>(),
        imported.as_secs_f64() * 1e3
    );
    println!();
    println!("| meshes | vertices | pool MB | added ms | GPU shadow µs | GPU forward µs |");
    println!("|---|---:|---:|---:|---:|---:|");

    let mut frame_number = 0u64;
    for soup in [false, true] {
        let mut pool =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let start = Instant::now();
        let ids = add_meshes(&scene, &mut pool, soup);
        let added = start.elapsed();
        let (vertices, _, indices, _) = pool.usage();

        // Draws sorted by mesh, so instances of a primitive share a batch.
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        let mut draws: Vec<DrawItem> = Vec::new();
        for &(mesh, world) in &placed {
            for (p, primitive) in scene.meshes[mesh].primitives.iter().enumerate() {
                let m = primitive.material.map(|i| &scene.materials[i]);
                let base = m.map_or([0.8, 0.8, 0.8, 1.0], |m| m.base_color);
                draws.push(DrawItem {
                    mesh: ids[mesh][p],
                    world_from_object: world,
                    material: Material {
                        base_color: Vec3::new(base[0], base[1], base[2]),
                        metallic: m.map_or(0.0, |m| m.metallic),
                        roughness: m.map_or(0.5, |m| m.roughness),
                    },
                });
                for corner in 0..8 {
                    let pick = |bit: usize, axis: usize| {
                        if corner & bit != 0 {
                            primitive.max[axis]
                        } else {
                            primitive.min[axis]
                        }
                    };
                    let c = world.transform_point3(Vec3::new(pick(1, 0), pick(2, 1), pick(4, 2)));
                    min = min.min(c);
                    max = max.max(c);
                }
            }
        }
        draws.sort_by_key(|d| format!("{:?}", d.mesh));
        // An interior view down the longest horizontal axis at a fifth of the height, or
        // an object view from the front.
        let size = max - min;
        let center = (min + max) * 0.5;
        let (eye, target) = if size.x.max(size.z) > 2.5 * size.y {
            let y = min.y + size.y * 0.2;
            if size.x >= size.z {
                (
                    Vec3::new(min.x + size.x * 0.08, y, center.z),
                    Vec3::new(max.x, y, center.z),
                )
            } else {
                (
                    Vec3::new(center.x, y, min.z + size.z * 0.08),
                    Vec3::new(center.x, y, max.z),
                )
            }
        } else {
            (center + Vec3::new(0.0, 0.0, size.length() * 0.9), center)
        };
        let camera = Camera {
            clip_from_world: vulkan_perspective(
                60f32.to_radians(),
                WIDTH as f32 / HEIGHT as f32,
                size.length() * 0.001,
                size.length() * 2.0,
            ) * look_at(eye, target, Vec3::Y),
            position: eye,
        };
        let light = DirectionalLight {
            direction: Vec3::new(-0.3, -1.0, -0.2),
            color: Vec3::splat(3.0),
            ambient: Vec3::splat(0.08),
        };

        let (mut shadow, mut forward) = (Vec::new(), Vec::new());
        for frame in 0..WARMUP + FRAMES {
            // SAFETY: the previous frame finished (waited below).
            unsafe {
                d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                    .unwrap();
                d.begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .unwrap();
            }
            ring.begin_slot(0);
            {
                // SAFETY: nothing is in flight.
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                frame_number += 1;
                let geo = pool
                    .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                    .unwrap();
                let hdr = fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                let depth =
                    fg.create_texture("depth", TextureDesc::new_2d(depth_format, WIDTH, HEIGHT));
                renderer
                    .add_passes(
                        &mut fg,
                        &mut ring,
                        &bindless,
                        &geo,
                        &draws,
                        &ForwardView {
                            camera,
                            light,
                            color: hdr,
                            depth,
                            clear: Some([0.02, 0.02, 0.05, 1.0]),
                            occlusion: None,
                        },
                    )
                    .unwrap();
                fg.add_pass("consume")
                    .read(hdr, Access::TransferRead)
                    .side_effects()
                    .execute(|_| {});
                fg.execute_with_bindless(&mut allocator, Some(&bindless))
                    .unwrap();
            }
            // SAFETY: as above.
            unsafe {
                d.end_command_buffer(cmd).unwrap();
                let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
                d.queue_submit2(
                    device.graphics.raw,
                    &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                    vk::Fence::null(),
                )
                .unwrap();
                device.wait_idle().unwrap();
            }
            if frame > WARMUP {
                for time in graph.gpu_pass_times() {
                    match time.name {
                        "shadow" => shadow.push(f64::from(time.gpu_us)),
                        "forward" => forward.push(f64::from(time.gpu_us)),
                        _ => {}
                    }
                }
            }
        }
        println!(
            "| {} | {vertices} | {:.1} | {:.0} | {:.0} | {:.0} |",
            if soup {
                "triangle soup"
            } else {
                "indexed (pool)"
            },
            (vertices * 28 + indices * 4) as f64 / (1 << 20) as f64,
            added.as_secs_f64() * 1e3,
            median(&mut shadow),
            median(&mut forward),
        );
        // SAFETY: the device is idle.
        unsafe { pool.destroy(&mut allocator) };
    }
    println!();

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        d.destroy_command_pool(command_pool, None);
    }
}
