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
//! `cargo run -p engine_render --release --example draw_bench [max-instances] [--prepass]
//! [--cascades <1-4>] [--shadow-res <texels per cascade>] [--sphere <rings>] [--lods]`
//!
//! `--sphere <rings>` draws UV spheres of `rings` × 2·`rings` quads (radius 0.5) instead of
//! cubes; `--lods` gives the mesh detail levels from `engine_assets::generate_lods`
//! (ADR 0053). `--blended` makes every instance half transparent (ADR 0054): the forward
//! column then includes the transparent pass.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Occlusion, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable};
use engine_render::mesh::{
    GeometryCapacity, GeometryPool, LodIndices, MeshId, OwnedMeshData, cube,
};
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

/// A UV sphere of radius 0.5: `rings` × 2·`rings` quads, counter-clockwise from outside.
fn sphere(rings: u32) -> OwnedMeshData {
    let segments = rings * 2;
    let mut mesh = OwnedMeshData::default();
    for r in 0..=rings {
        let theta = core::f32::consts::PI * r as f32 / rings as f32;
        for s in 0..=segments {
            let phi = core::f32::consts::TAU * s as f32 / segments as f32;
            let n = [
                theta.sin() * phi.cos(),
                theta.cos(),
                theta.sin() * phi.sin(),
            ];
            mesh.positions.push(n.map(|c| c * 0.5));
            mesh.normals.push(n);
            mesh.uvs
                .push([s as f32 / segments as f32, r as f32 / rings as f32]);
        }
    }
    let row = segments + 1;
    for r in 0..rings {
        for s in 0..segments {
            let (a, b) = (r * row + s, (r + 1) * row + s);
            mesh.indices
                .extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
        }
    }
    mesh
}

fn main() {
    let mut max_instances: usize = 100_000;
    let mut prepass = false;
    let mut sphere_rings = None;
    let mut lods = false;
    let mut blended = false;
    let mut shadows = ShadowSettings::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--prepass" => prepass = true,
            "--cascades" => shadows.cascades = args.next().unwrap().parse().unwrap(),
            "--shadow-res" => shadows.resolution = args.next().unwrap().parse().unwrap(),
            "--sphere" => sphere_rings = Some(args.next().unwrap().parse().unwrap()),
            "--lods" => lods = true,
            "--blended" => blended = true,
            n => max_instances = n.parse().unwrap(),
        }
    }
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
    let mut renderer =
        ForwardRenderer::new(&device, &bindless, HDR_FORMAT, depth_format, shadows).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh_data = sphere_rings.map_or_else(cube, sphere);
    let levels = if lods {
        engine_assets::generate_lods(
            &mesh_data.positions,
            &mesh_data.indices,
            &engine_assets::LodSettings::default(),
        )
    } else {
        Vec::new()
    };
    print!("Mesh: {} triangles", mesh_data.indices.len() / 3);
    for level in &levels {
        print!(
            ", LOD {} triangles (error {:.4})",
            level.indices.len() / 3,
            level.error
        );
    }
    println!();
    let lod_indices: Vec<LodIndices<'_>> = levels
        .iter()
        .map(|l| LodIndices {
            indices: &l.indices,
            error: l.error,
        })
        .collect();
    let mesh = geometry
        .add_mesh_with_lods(&mesh_data.as_data(), &lod_indices)
        .unwrap();
    let mut textures = TexturePool::new(&device, 64 << 20, 1);
    let mut materials = MaterialTable::new(1);
    // 64 hues from blue to red, then the walls' grey.
    let mut palette: Vec<MaterialId> = (0..64)
        .map(|k| {
            let h = k as f32 / 64.0;
            let solid = MaterialDesc::solid([0.3 + 0.6 * h, 0.5, 0.9 - 0.6 * h], 0.0, 0.5);
            materials.add(if blended {
                MaterialDesc {
                    base_color: [
                        solid.base_color[0],
                        solid.base_color[1],
                        solid.base_color[2],
                        0.5,
                    ],
                    alpha: engine_render::material::AlphaMode::Blend,
                    ..solid
                }
            } else {
                solid
            })
        })
        .collect();
    palette.push(materials.default_material());
    let mut frame_number = 0u64;
    let capacity = max_instances as u64 + 1024;
    // Per instance: its record, and with `--blended` a batch and a command per list.
    let mut ring = UploadRing::new(&device, &mut allocator, 1, capacity * 256).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut occlusion = Occlusion::new(&device, &mut allocator, capacity as u32).unwrap();
    let readback_bytes = 2 * 20 * 4 * (max_instances as u64 + 1024);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                // Up to two phases of commands (20 bytes each): four detail levels of one
                // batch, or one per instance when every instance is blended.
                .size(readback_bytes)
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

    println!(
        "### Forward renderer, GPU-driven ({WIDTH}×{HEIGHT} HDR, {} shadow cascades of {}²)\n",
        shadows.cascade_count(),
        shadows.resolution
    );
    println!("Device: {}\n", device.adapter().summary());
    println!(
        "| instances | view | culling | drawn | draw list µs | add_passes µs | graph µs | GPU cull µs | GPU pyramid µs | GPU shadow µs | GPU prepass µs | GPU forward µs |"
    );
    println!("|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    renderer.set_depth_prepass(prepass);

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
            let (mut cull, mut pyramid, mut shadow, mut depth, mut forward) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
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
                let (prepared, executed, phases, per_list);
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
                                temporal: None,
                                lights: &[],
                                environment: None,
                                reflections: Default::default(),
                            },
                        )
                        .unwrap();
                    prepared = t.elapsed();
                    // One batch (one mesh): a command per detail level and phase.
                    phases = if passes.occlusion { 2 } else { 1 };
                    per_list = u64::from(passes.draw_commands);
                    let commands = passes.commands.unwrap();
                    fg.add_pass("consume")
                        .read(hdr, Access::TransferRead)
                        .read(commands, Access::TransferRead)
                        .write(out, Access::TransferWrite)
                        .execute(move |ctx| {
                            let region = vk::BufferCopy {
                                src_offset: 0,
                                dst_offset: 0,
                                // VkDrawIndexedIndirectCommand: 20 bytes.
                                size: 20 * phases * per_list,
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
                let commands =
                    unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_bytes as usize) };
                let count_at = |i: usize| {
                    let at = 20 * i + 4;
                    u32::from_le_bytes([
                        commands[at],
                        commands[at + 1],
                        commands[at + 2],
                        commands[at + 3],
                    ])
                };
                drawn = (0..(phases * per_list) as usize).map(count_at).sum::<u32>();
                if frame >= WARMUP {
                    list.push(listed.as_secs_f64() * 1e6);
                    prepare.push(prepared.as_secs_f64() * 1e6);
                    record.push(executed.as_secs_f64() * 1e6);
                }
                // Times of the frame before this one (read when its slot is reused).
                if frame > WARMUP {
                    let (mut c, mut p, mut s, mut d, mut f) = (0.0, 0.0, 0.0, 0.0, 0.0);
                    for time in graph.gpu_pass_times() {
                        let us = f64::from(time.gpu_us);
                        match time.name {
                            "cull" | "cull early" | "cull late" => c += us,
                            "hiz" => p += us,
                            "shadow cull" | "shadow" => s += us,
                            "depth prepass" | "depth prepass early" | "depth prepass late" => {
                                d += us
                            }
                            "forward" | "forward early" | "forward late" | "transparent" => f += us,
                            _ => {}
                        }
                    }
                    cull.push(c);
                    pyramid.push(p);
                    shadow.push(s);
                    depth.push(d);
                    forward.push(f);
                }
            }
            println!(
                "| {} | {} | {} | {drawn} | {:.0} | {:.0} | {:.0} | {:.1} | {:.1} | {:.0} | {:.0} | {:.0} |",
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
                median(&mut depth),
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
