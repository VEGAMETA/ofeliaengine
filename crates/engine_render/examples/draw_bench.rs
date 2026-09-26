//! Forward renderer benchmark, headless (ADR 0034): many cube instances with PBR and a
//! shadow map into a 1920×1080 HDR target. For each count, camera and culling setting it
//! reports the CPU time of building the draw list, of `add_passes` (instances, batches and
//! commands into the upload ring) and of recording the graph, the GPU time of the cull,
//! shadow and forward passes (medians of the measured frames, from the graph's
//! timestamps), and how many instances the GPU found visible. Prints Markdown.
//!
//! `cargo run -p engine_render --release --example draw_bench [max-instances]`

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, Material, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::mesh::{Mesh, cube};
use engine_render::post::HDR_FORMAT;
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

#[derive(Clone, Copy, PartialEq)]
enum View {
    /// Above the field, looking at its center: nearly everything is in view.
    Overview,
    /// Inside the field near the ground with an 80 m far plane, as in an open world: a
    /// small part is in view, and the same part at every count.
    Inside,
}

/// Instances on a square field (side grows with the count, ~1 cube per 4 m²), spinning.
fn fill<'a>(draws: &mut Vec<DrawItem<'a>>, mesh: &'a Mesh, count: usize, frame: usize) {
    draws.clear();
    let side = (count as f32).sqrt().ceil() as usize;
    let spacing = 2.0;
    let half = side as f32 * spacing * 0.5;
    let t = frame as f32 * 0.02;
    for i in 0..count {
        let (x, z) = ((i % side) as f32, (i / side) as f32);
        let p = Vec3::new(x * spacing - half, 0.6, z * spacing - half);
        let h = (i as f32 * 0.618_034).fract();
        draws.push(DrawItem {
            mesh,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::splat(0.8),
                Quat::from_rotation_y(t + h * core::f32::consts::TAU),
                p,
            ),
            material: Material {
                base_color: Vec3::new(0.3 + 0.6 * h, 0.5, 0.9 - 0.6 * h),
                metallic: 0.0,
                roughness: 0.5,
            },
        });
    }
}

fn camera(view: View, count: usize) -> Camera {
    let side = (count as f32).sqrt().ceil() * 2.0;
    let (eye, target) = match view {
        View::Overview => (Vec3::new(0.0, side * 0.9, side * 0.9), Vec3::ZERO),
        View::Inside => (
            Vec3::new(-side * 0.25, 3.0, -side * 0.25),
            Vec3::new(-side * 0.25 + 10.0, 1.0, -side * 0.25 + 3.0),
        ),
    };
    let far = match view {
        View::Overview => side * 3.0,
        View::Inside => 80.0,
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
    let mut mesh = Mesh::new(&device, &mut allocator, &cube()).unwrap();
    let mut ring = UploadRing::new(
        &device,
        &mut allocator,
        1,
        (max_instances as u64 + 1024) * 128,
    )
    .unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(16)
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
        "| cubes | view | culling | visible | draw list µs | add_passes µs | graph µs | GPU cull µs | GPU shadow µs | GPU forward µs |"
    );
    println!("|---:|---|---|---:|---:|---:|---:|---:|---:|---:|");

    let mut draws: Vec<DrawItem<'_>> = Vec::new();
    for count in [1_000usize, 10_000, 100_000]
        .into_iter()
        .filter(|&c| c <= max_instances)
    {
        for view in [View::Overview, View::Inside] {
            for culling in [true, false] {
                renderer.set_frustum_culling(culling);
                let camera = camera(view, count);
                let (mut list, mut prepare, mut record) = (Vec::new(), Vec::new(), Vec::new());
                let (mut cull, mut shadow, mut forward) = (Vec::new(), Vec::new(), Vec::new());
                let mut visible = 0;
                for frame in 0..WARMUP + FRAMES {
                    let start = Instant::now();
                    fill(&mut draws, &mesh, count, frame);
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
                    let (prepared, executed);
                    {
                        // SAFETY: nothing is in flight.
                        let mut fg = unsafe { graph.begin_slot(0, cmd) };
                        let hdr = fg
                            .create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                        let depth = fg.create_texture(
                            "depth",
                            TextureDesc::new_2d(depth_format, WIDTH, HEIGHT),
                        );
                        let out = fg.import_buffer(
                            "readback",
                            ImportedBuffer {
                                buffer: readback,
                                size: 16,
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
                                &draws,
                                &camera,
                                &light,
                                hdr,
                                depth,
                                Some([0.02, 0.02, 0.05, 1.0]),
                            )
                            .unwrap();
                        prepared = t.elapsed();
                        let commands = passes.commands.unwrap();
                        fg.add_pass("consume")
                            .read(hdr, Access::TransferRead)
                            .read(commands, Access::TransferRead)
                            .write(out, Access::TransferWrite)
                            .execute(move |ctx| {
                                let region = vk::BufferCopy {
                                    src_offset: 0,
                                    dst_offset: 0,
                                    size: 16,
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
                    // SAFETY: the GPU is idle; the mapping holds one draw command.
                    let command = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), 16) };
                    visible = u32::from_le_bytes([command[4], command[5], command[6], command[7]]);
                    if frame >= WARMUP {
                        list.push(listed.as_secs_f64() * 1e6);
                        prepare.push(prepared.as_secs_f64() * 1e6);
                        record.push(executed.as_secs_f64() * 1e6);
                    }
                    // Times of the frame before this one (read when its slot is reused).
                    if frame > WARMUP {
                        for time in graph.gpu_pass_times() {
                            let target = match time.name {
                                "cull" => &mut cull,
                                "shadow" => &mut shadow,
                                "forward" => &mut forward,
                                _ => continue,
                            };
                            target.push(f64::from(time.gpu_us));
                        }
                    }
                }
                println!(
                    "| {count} | {} | {} | {visible} | {:.0} | {:.0} | {:.0} | {:.1} | {:.0} | {:.0} |",
                    match view {
                        View::Overview => "overview",
                        View::Inside => "inside the field",
                    },
                    if culling { "on" } else { "off" },
                    median(&mut list),
                    median(&mut prepare),
                    median(&mut record),
                    median(&mut cull),
                    median(&mut shadow),
                    median(&mut forward),
                );
            }
        }
    }
    println!();

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        mesh.destroy(&mut allocator);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
}
