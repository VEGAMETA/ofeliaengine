//! Skinning benchmark (ADR 0068), headless: tubes of 4096 vertices bent by a chain of 32
//! joints (4 or 8 influences per vertex), skinned by the compute pass and drawn through
//! the forward pass with motion vectors at 1920 × 1080, then the skinned motion pass.
//!
//! Per configuration (medians over 60 frames): GPU µs of the skinning pass (and per 100k
//! vertices), of the skinned motion pass, of the shadow and forward passes; CPU µs to pose
//! every instance (`set_pose` + `update_bounds`) and to declare the skinning pass. Prints
//! Markdown.
//!
//! `cargo run -p engine_render --release --example skin_bench`

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::too_many_lines
)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
    ViewFeatures,
};
use engine_render::graph::{Access, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, OwnedMeshData};
use engine_render::post::HDR_FORMAT;
use engine_render::skinning::{SkinnedMeshData, Skinning};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance as VkInstance, InstanceDesc,
};
use std::time::Instant;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WARMUP: usize = 10;
const FRAMES: usize = 60;
const JOINTS: usize = 32;
const SEGMENTS: usize = 64;
const RINGS: usize = 64;

/// A tube along +Y, 2 long, 0.1 thick: `RINGS × SEGMENTS` vertices, each ring weighted
/// to the joints around its height (spread over `sets × 4` joints).
fn tube(sets: usize) -> (OwnedMeshData, Vec<[u16; 4]>, Vec<[f32; 4]>) {
    let mut mesh = OwnedMeshData::default();
    let (mut joints, mut weights) = (Vec::new(), Vec::new());
    for r in 0..RINGS {
        let v = r as f32 / (RINGS - 1) as f32;
        let at = v * (JOINTS - 1) as f32;
        for s in 0..SEGMENTS {
            let a = s as f32 / SEGMENTS as f32 * core::f32::consts::TAU;
            let n = Vec3::new(a.cos(), 0.0, a.sin());
            mesh.positions
                .push((n * 0.1 + Vec3::Y * 2.0 * v).to_array());
            mesh.normals.push(n.to_array());
            mesh.tangents.push([0.0, 1.0, 0.0, 1.0]);
            mesh.uvs.push([s as f32 / SEGMENTS as f32, v]);
            for k in 0..sets {
                let mut j = [0u16; 4];
                let mut w = [0.0f32; 4];
                for i in 0..4 {
                    let offset = (k * 4 + i) as f32 - (sets * 4) as f32 / 2.0 + 0.5;
                    let joint = (at + offset).round().clamp(0.0, (JOINTS - 1) as f32);
                    j[i] = joint as u16;
                    w[i] = (1.0 - ((at + offset) - at).abs() / (sets * 4) as f32).max(0.05);
                }
                joints.push(j);
                weights.push(w);
            }
        }
    }
    for r in 0..RINGS - 1 {
        for s in 0..SEGMENTS {
            let (a, b) = (
                (r * SEGMENTS + s) as u32,
                (r * SEGMENTS + (s + 1) % SEGMENTS) as u32,
            );
            let (c, d) = (a + SEGMENTS as u32, b + SEGMENTS as u32);
            mesh.indices.extend([a, c, b, b, c, d]);
        }
    }
    (mesh, joints, weights)
}

/// Skinning matrices of the chain bent by `t`: joint k turned about Z by a wave.
fn palette(t: f32, phase: f32) -> Vec<[f32; 12]> {
    let mut out = Vec::with_capacity(JOINTS);
    let mut model = Mat4::IDENTITY;
    let step = 2.0 / (JOINTS - 1) as f32;
    for k in 0..JOINTS {
        let local = Mat4::from_rotation_translation(
            Quat::from_rotation_z(0.08 * (t * 3.0 + phase + k as f32 * 0.3).sin()),
            Vec3::new(0.0, if k == 0 { 0.0 } else { step }, 0.0),
        );
        model *= local;
        let bind = Mat4::from_translation(Vec3::new(0.0, step * k as f32, 0.0)).inverse();
        let m = model * bind;
        let row = |i: usize| m.row(i).to_array();
        let (a, b, c) = (row(0), row(1), row(2));
        out.push([
            a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], c[0], c[1], c[2], c[3],
        ]);
    }
    out
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn main() {
    let instance = VkInstance::new(&InstanceDesc {
        app_name: String::from("skin_bench"),
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
    println!("{}\n", device.adapter().summary());
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
    // SAFETY: plain object creation.
    let (pool, cmd) = unsafe {
        let pool = d
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
                    .command_pool(pool)
                    .command_buffer_count(1),
            )
            .unwrap()[0];
        (pool, cmd)
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.4),
        color: Vec3::splat(2.0),
        ambient: Vec3::splat(0.1),
    };
    println!(
        "| vertices | influences | instances | skinning µs | µs per 100k vertices | skinned motion µs | shadow µs | forward µs | CPU pose µs | CPU declare µs |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (vertices, sets) in [
        (100_000usize, 1usize),
        (100_000, 2),
        (1_000_000, 1),
        (1_000_000, 2),
    ] {
        let per = RINGS * SEGMENTS;
        let count = vertices.div_ceil(per);
        let mut geometry = GeometryPool::new(
            &device,
            &mut allocator,
            GeometryCapacity {
                vertices: (count * per + 1024) as u32,
                indices: (count * (RINGS - 1) * SEGMENTS * 6 + 1024) as u32,
            },
            1,
        )
        .unwrap();
        let mut skinning = Skinning::new(
            &device,
            &mut allocator,
            &bindless,
            depth_format,
            (count * per + per) as u32,
            1,
        )
        .unwrap();
        let mut textures = TexturePool::new(&device, 1 << 20, 1);
        let mut materials = MaterialTable::new(1);
        let material = materials.add(MaterialDesc::solid([0.8, 0.5, 0.3], 0.0, 0.5));
        let mut ring = UploadRing::new(&device, &mut allocator, 1, 64 << 20).unwrap();
        let mut graph = RenderGraph::new(&device, 1);
        let temporal = Temporal::new(&device, &mut allocator, count as u32 + 16).unwrap();
        let (mesh, joints, weights) = tube(sets);
        let skin = skinning
            .add_skin(&SkinnedMeshData {
                mesh: mesh.as_data(),
                joints: &joints,
                weights: &weights,
                joint_count: JOINTS as u16,
            })
            .unwrap();
        let per_row = (count as f32).sqrt().ceil() as usize;
        let mut items = Vec::new();
        let mut ids = Vec::new();
        for i in 0..count {
            let (id, out) = skinning.add_instance(skin, &mut geometry).unwrap();
            let (x, z) = ((i % per_row) as f32, (i / per_row) as f32);
            let world = Mat4::from_translation(Vec3::new(
                (x - per_row as f32 / 2.0) * 0.4,
                0.0,
                (z - per_row as f32 / 2.0) * 0.4,
            ));
            items.push(DrawItem {
                mesh: out,
                material,
                world_from_object: world,
            });
            ids.push(id);
        }
        let extent = per_row as f32 * 0.4;
        let eye = Vec3::new(0.0, extent * 0.6 + 2.0, extent * 0.8 + 2.0);
        let camera = Camera {
            clip_from_world: vulkan_perspective(1.0, WIDTH as f32 / HEIGHT as f32, 0.1, 200.0)
                * look_at(eye, Vec3::new(0.0, 1.0, 0.0), Vec3::Y),
            position: eye,
        };
        let palettes: Vec<Vec<Vec<[f32; 12]>>> = (0..4)
            .map(|f| (0..8).map(|k| palette(f as f32 * 0.1, k as f32)).collect())
            .collect();
        let (
            mut skin_us,
            mut motion_us,
            mut shadow_us,
            mut forward_us,
            mut pose_us,
            mut declare_us,
        ) = (vec![], vec![], vec![], vec![], vec![], vec![]);
        let mut frame_number = 0u64;
        for frame in 0..WARMUP + FRAMES {
            let t0 = Instant::now();
            for (k, (&id, item)) in ids.iter().zip(&items).enumerate() {
                skinning.set_pose(id, &palettes[frame % 4][k % 8], &item.world_from_object);
            }
            skinning.update_bounds(&mut geometry, None);
            let posed = t0.elapsed();
            frame_number += 1;
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
            let declared;
            {
                // SAFETY: nothing is in flight.
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                textures
                    .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                    .unwrap();
                let mats = materials
                    .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                    .unwrap();
                let geo = geometry
                    .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                    .unwrap();
                let t1 = Instant::now();
                let skins = skinning
                    .begin_frame(
                        &mut fg,
                        &mut ring,
                        &device,
                        &mut allocator,
                        &bindless,
                        &geo,
                        frame_number,
                    )
                    .unwrap();
                declared = t1.elapsed();
                let hdr = fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                let depth =
                    fg.create_texture("depth", TextureDesc::new_2d(depth_format, WIDTH, HEIGHT));
                let passes = renderer
                    .add_passes(
                        &mut fg,
                        &mut ring,
                        &bindless,
                        &geo,
                        &mats,
                        &items,
                        &ForwardView {
                            camera,
                            light,
                            color: hdr,
                            depth,
                            clear: Some([0.0; 4]),
                            occlusion: None,
                            temporal: Some(&temporal),
                            shadow_cache: None,
                            virtual_shadows: None,
                            lights: &[],
                            decals: &[],
                            environment: None,
                            reflections: Default::default(),
                            gi: None,
                            features: ViewFeatures::ALL,
                        },
                    )
                    .unwrap();
                let motion = passes.motion.unwrap();
                skins
                    .add_motion_pass(&mut fg, &mut ring, &bindless, &geo, depth, &motion)
                    .unwrap();
                fg.add_pass("consume")
                    .read(hdr, Access::TransferRead)
                    .read(motion.velocity, Access::TransferRead)
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
                let (mut s, mut m, mut sh, mut f) = (0.0, 0.0, 0.0, 0.0);
                for time in graph.gpu_pass_times() {
                    let us = f64::from(time.gpu_us);
                    match time.name {
                        "skinning" => s += us,
                        "skinned motion" => m += us,
                        "shadow" => sh += us,
                        "forward" => f += us,
                        _ => {}
                    }
                }
                skin_us.push(s);
                motion_us.push(m);
                shadow_us.push(sh);
                forward_us.push(f);
                pose_us.push(posed.as_secs_f64() * 1e6);
                declare_us.push(declared.as_secs_f64() * 1e6);
            }
        }
        let skin = median(&mut skin_us);
        println!(
            "| {} | {} | {count} | {skin:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} | {:.0} |",
            count * per,
            sets * 4,
            skin / (count * per) as f64 * 1e5,
            median(&mut motion_us),
            median(&mut shadow_us),
            median(&mut forward_us),
            median(&mut pose_us),
            median(&mut declare_us),
        );
        device.wait_idle().unwrap();
        let mut temporal = temporal;
        // SAFETY: the device is idle.
        unsafe {
            skinning.destroy(&mut allocator);
            geometry.destroy(&mut allocator);
            textures.destroy(&mut allocator, &bindless);
            materials.destroy(&mut allocator);
            ring.destroy(&mut allocator);
            graph.destroy(&mut allocator, Some(&bindless));
            temporal.destroy(&mut allocator);
        }
    }
    // SAFETY: the device is idle.
    unsafe { d.destroy_command_pool(pool, None) };
}
