//! Forward renderer benchmark, headless (ADR 0034, ADR 0035): many cube instances with PBR
//! and a shadow map into a 1920×1080 HDR target. For each count, camera and culling mode
//! it reports the CPU time of building the draw list, of `add_passes` (instances, batches
//! and commands into the upload ring) and of recording the graph, the GPU time of the
//! culling, depth pyramid, shadow and forward passes (medians of the measured frames, from
//! the graph's timestamps), and how many instances the GPU drew. Prints Markdown.
//!
//! Culling modes: off, frustum, and frustum + two-phase occlusion (in steady state: the
//! occlusion state carries over the warm-up frames).
//!
//! `cargo run -p engine_render --release --example draw_bench [max-instances]`

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Occlusion, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, cube};
use engine_render::post::HDR_FORMAT;
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};
use std::time::Instant;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WARMUP: usize = 5;
const FRAMES: usize = 30;
/// Cubes are 2 m apart; walls in the city stand every `BLOCK` meters.
const SPACING: f32 = 2.0;
const BLOCK: f32 = 10.0;

#[derive(Clone, Copy, PartialEq)]
enum View {
    /// Above the field, looking at its center: nearly everything is in view.
    Overview,
    /// Inside the field near the ground with an 80 m far plane, as in an open world: a
    /// small part is in view, and the same part at every count.
    Inside,
    /// The same camera in a city: 4 m walls around every 10 m block hide the other
    /// blocks.
    City,
}

#[derive(Clone, Copy, PartialEq)]
enum Culling {
    Off,
    Frustum,
    Occlusion,
}

fn side(count: usize) -> f32 {
    (count as f32).sqrt().ceil() * SPACING
}

/// Instances on a square field (~1 cube per 4 m²), spinning, colored from `palette` (the
/// walls use its last material); in the city, walls follow.
fn fill(
    draws: &mut Vec<DrawItem>,
    mesh: MeshId,
    palette: &[MaterialId],
    count: usize,
    frame: usize,
    city: bool,
) {
    let hues = palette.len() - 1;
    draws.clear();
    let per_row = (count as f32).sqrt().ceil() as usize;
    let half = side(count) * 0.5;
    let t = frame as f32 * 0.02;
    for i in 0..count {
        let (x, z) = ((i % per_row) as f32, (i / per_row) as f32);
        // Cubes sit between the walls, not in them.
        let p = Vec3::new(x * SPACING - half + 1.0, 0.6, z * SPACING - half + 1.0);
        let h = (i as f32 * 0.618_034).fract();
        draws.push(DrawItem {
            mesh,
            material: palette[(h * hues as f32) as usize % hues],
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::splat(0.8),
                Quat::from_rotation_y(t + h * core::f32::consts::TAU),
                p,
            ),
        });
    }
    if city {
        let length = side(count);
        let lines = (length / BLOCK) as usize;
        for k in 0..=lines {
            let at = k as f32 * BLOCK - half;
            for world_from_object in [
                Mat4::from_scale_rotation_translation(
                    Vec3::new(0.5, 4.0, length),
                    Quat::IDENTITY,
                    Vec3::new(at, 2.0, 0.0),
                ),
                Mat4::from_scale_rotation_translation(
                    Vec3::new(length, 4.0, 0.5),
                    Quat::IDENTITY,
                    Vec3::new(0.0, 2.0, at),
                ),
            ] {
                draws.push(DrawItem {
                    mesh,
                    material: palette[hues],
                    world_from_object,
                });
            }
        }
    }
}

fn camera(view: View, count: usize) -> Camera {
    let side = side(count);
    // 1.5 m inside the block a quarter of the way into the field (walls stand at
    // `k × BLOCK - side / 2`), 3 m up, looking along the field.
    let half = side * 0.5;
    let block = ((side * 0.25) / BLOCK).floor() * BLOCK - half + 1.5;
    let (eye, target, far) = match view {
        View::Overview => (
            Vec3::new(0.0, side * 0.9, side * 0.9),
            Vec3::ZERO,
            side * 3.0,
        ),
        View::Inside | View::City => (
            Vec3::new(block, 3.0, block),
            Vec3::new(block + 10.0, 1.0, block + 3.0),
            80.0,
        ),
    };
    Camera {
        clip_from_world: vulkan_perspective(
            60f32.to_radians(),
            WIDTH as f32 / HEIGHT as f32,
            0.1,
            far,
        ) * look_at(eye, target, Vec3::Y),
        position: eye,
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn main() {
    let max_instances: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("draw_bench"),
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
    let mut renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, &mut allocator, 64 << 20, 1).unwrap();
    let mut materials = MaterialTable::new(1);
    // 64 hues from blue to red, then the walls' grey.
    let mut palette: Vec<MaterialId> = (0..64)
        .map(|k| {
            let h = k as f32 / 64.0;
            materials.add(MaterialDesc::solid(
                [0.3 + 0.6 * h, 0.5, 0.9 - 0.6 * h],
                0.0,
                0.5,
            ))
        })
        .collect();
    palette.push(materials.default_material());
    let mut frame_number = 0u64;
    let capacity = max_instances as u64 + 1024;
    let mut ring = UploadRing::new(&device, &mut allocator, 1, capacity * 128).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut occlusion = Occlusion::new(&device, &mut allocator, capacity as u32).unwrap();
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(40)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
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
        direction: Vec3::new(-0.4, -1.0, -0.3),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.08),
    };

    println!("### Forward renderer, GPU-driven ({WIDTH}×{HEIGHT} HDR, shadow map 2048²)\n");
    println!("Device: {}\n", device.adapter().summary());
    println!(
        "| instances | view | culling | drawn | draw list µs | add_passes µs | graph µs | GPU cull µs | GPU pyramid µs | GPU shadow µs | GPU forward µs |"
    );
    println!("|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|");

    let mut draws: Vec<DrawItem> = Vec::new();
    let configurations = [
        (View::Overview, Culling::Off),
        (View::Overview, Culling::Frustum),
        (View::Overview, Culling::Occlusion),
        (View::Inside, Culling::Off),
        (View::Inside, Culling::Frustum),
        (View::Inside, Culling::Occlusion),
        (View::City, Culling::Frustum),
        (View::City, Culling::Occlusion),
    ];
    for count in [1_000usize, 10_000, 100_000]
        .into_iter()
        .filter(|&c| c <= max_instances)
    {
        for (view, culling) in configurations {
            renderer.set_frustum_culling(culling != Culling::Off);
            occlusion.reset();
            let camera = camera(view, count);
            let (mut list, mut prepare, mut record) = (Vec::new(), Vec::new(), Vec::new());
            let (mut cull, mut pyramid, mut shadow, mut forward) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let mut drawn = 0;
            for frame in 0..WARMUP + FRAMES {
                let start = Instant::now();
                fill(&mut draws, mesh, &palette, count, frame, view == View::City);
                let listed = start.elapsed();
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
                let (prepared, executed, phases);
                {
                    // SAFETY: nothing is in flight.
                    let mut fg = unsafe { graph.begin_slot(0, cmd) };
                    frame_number += 1;
                    let geo = geometry
                        .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                        .unwrap();
                    textures
                        .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                        .unwrap();
                    let mats = materials
                        .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                        .unwrap();
                    let hdr =
                        fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                    let depth = fg
                        .create_texture("depth", TextureDesc::new_2d(depth_format, WIDTH, HEIGHT));
                    let out = fg.import_buffer(
                        "readback",
                        ImportedBuffer {
                            buffer: readback,
                            size: 40,
                            address: 0,
                            initial: BufferState::idle(),
                            final_state: Some(BufferState::host_read()),
                        },
                    );
                    let t = Instant::now();
                    let passes = renderer
                        .add_passes(
                            &mut fg,
                            &mut ring,
                            &bindless,
                            &geo,
                            &mats,
                            &draws,
                            &ForwardView {
                                camera,
                                light,
                                color: hdr,
                                depth,
                                clear: Some([0.02, 0.02, 0.05, 1.0]),
                                occlusion: (culling == Culling::Occlusion).then_some(&occlusion),
                            },
                        )
                        .unwrap();
                    prepared = t.elapsed();
                    // One batch (one mesh): a command per phase.
                    phases = if passes.occlusion { 2 } else { 1 };
                    let commands = passes.commands.unwrap();
                    fg.add_pass("consume")
                        .read(hdr, Access::TransferRead)
                        .read(commands, Access::TransferRead)
                        .write(out, Access::TransferWrite)
                        .execute(move |ctx| {
                            let region = vk::BufferCopy {
                                src_offset: 0,
                                dst_offset: 0,
                                // One VkDrawIndexedIndirectCommand (20 bytes) per phase.
                                size: 20 * phases,
                            };
                            // SAFETY: recording outside a rendering scope.
                            unsafe {
                                ctx.device.cmd_copy_buffer(
                                    ctx.cmd,
                                    ctx.buffer(commands),
                                    ctx.buffer(out),
                                    &[region],
                                );
                            }
                        });
                    let t = Instant::now();
                    fg.execute_with_bindless(&mut allocator, Some(&bindless))
                        .unwrap();
                    executed = t.elapsed();
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
                let ptr = readback_mem.mapped_ptr().unwrap();
                // SAFETY: the GPU is idle; the mapping holds the phases' draw commands.
                let commands = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), 40) };
                let count_at = |i: usize| {
                    let at = 20 * i + 4;
                    u32::from_le_bytes([
                        commands[at],
                        commands[at + 1],
                        commands[at + 2],
                        commands[at + 3],
                    ])
                };
                drawn = (0..phases as usize).map(count_at).sum::<u32>();
                if frame >= WARMUP {
                    list.push(listed.as_secs_f64() * 1e6);
                    prepare.push(prepared.as_secs_f64() * 1e6);
                    record.push(executed.as_secs_f64() * 1e6);
                }
                // Times of the frame before this one (read when its slot is reused).
                if frame > WARMUP {
                    let (mut c, mut p, mut s, mut f) = (0.0, 0.0, 0.0, 0.0);
                    for time in graph.gpu_pass_times() {
                        let us = f64::from(time.gpu_us);
                        match time.name {
                            "cull" | "cull early" | "cull late" => c += us,
                            "hiz" => p += us,
                            "shadow" => s += us,
                            "forward" | "forward early" | "forward late" => f += us,
                            _ => {}
                        }
                    }
                    cull.push(c);
                    pyramid.push(p);
                    shadow.push(s);
                    forward.push(f);
                }
            }
            println!(
                "| {} | {} | {} | {drawn} | {:.0} | {:.0} | {:.0} | {:.1} | {:.1} | {:.0} | {:.0} |",
                draws.len(),
                match view {
                    View::Overview => "overview",
                    View::Inside => "inside the field",
                    View::City => "city blocks",
                },
                match culling {
                    Culling::Off => "off",
                    Culling::Frustum => "frustum",
                    Culling::Occlusion => "frustum + occlusion",
                },
                median(&mut list),
                median(&mut prepare),
                median(&mut record),
                median(&mut cull),
                median(&mut pyramid),
                median(&mut shadow),
                median(&mut forward),
            );
        }
    }
    println!();

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        occlusion.destroy(&mut allocator);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
}
