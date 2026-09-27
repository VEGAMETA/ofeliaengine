//! Two-phase occlusion culling (ADR 0035), under the validation layer with synchronization
//! validation.
//!
//! A wall fills the view; 200 cubes stand behind it and 20 in front of it. Three frames
//! are rendered with one [`Occlusion`] state:
//!
//! 1. nothing was visible before: the early phase draws nothing, the pyramid is empty (far),
//!    and the late phase draws everything in the frustum;
//! 2. the early phase draws everything that was visible; the late phase finds the cubes
//!    behind the wall hidden and draws nothing new;
//! 3. the early phase draws only the wall and the cubes in front of it; the hidden cubes
//!    stay hidden.
//!
//! Every frame must look exactly like frustum culling alone: occlusion culling may only
//! skip what cannot be seen. A fourth frame after `reset` repeats the first.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Material, Occlusion,
    ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render occlusion test"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: true,
        })?;
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    match attempt() {
        Ok(pair) => Some(pair),
        Err(e) if !required => {
            println!("skipping GPU test (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            None
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

const SIZE: u32 = 256;
const BEHIND: usize = 200;
const IN_FRONT: usize = 20;
/// Also in the list: cubes behind the camera, culled by the frustum in both phases.
const OUTSIDE: usize = 10;

#[test]
fn hidden_instances_are_skipped_and_the_image_is_unchanged() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let color_format = vk::Format::R8G8B8A8_UNORM;
    let depth_format = vk::Format::D32_SFLOAT;
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        color_format,
        depth_format,
        ShadowSettings {
            resolution: 1024,
            ..ShadowSettings::default()
        },
    )
    .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut frame_number = 0u64;
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut occlusion = Occlusion::new(&device, &mut allocator, 1024).unwrap();

    // The camera looks down -Z from z = 12. The wall at z = 5 is 12 × 12: at 7 m the view
    // is 8.1 m tall, so it covers the whole image.
    let eye = Vec3::new(0.0, 0.0, 12.0);
    let clip_from_world =
        vulkan_perspective(60f32.to_radians(), 1.0, 0.5, 100.0) * look_at(eye, Vec3::ZERO, Vec3::Y);
    let camera = Camera {
        clip_from_world,
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.5),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.1),
    };
    let material = |i: usize| Material {
        base_color: Vec3::new(
            (i % 7) as f32 / 7.0,
            (i % 5) as f32 / 5.0,
            (i % 3) as f32 / 3.0,
        ),
        metallic: 0.0,
        roughness: 0.6,
    };
    let mut draws = vec![DrawItem {
        mesh,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(12.0, 12.0, 0.5),
            Default::default(),
            Vec3::new(0.0, 0.0, 5.0),
        ),
        material: Material::default(),
    }];
    for i in 0..BEHIND {
        let (x, y, z) = ((i % 10) as f32, ((i / 10) % 5) as f32, (i / 50) as f32);
        draws.push(DrawItem {
            mesh,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::splat(0.5),
                Default::default(),
                Vec3::new(x * 0.6 - 2.7, y * 0.6 - 1.2, -z * 5.0),
            ),
            material: material(i),
        });
    }
    for i in 0..IN_FRONT {
        let (x, y) = ((i % 5) as f32, (i / 5) as f32);
        draws.push(DrawItem {
            mesh,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::splat(0.3),
                Default::default(),
                Vec3::new(x * 0.8 - 1.6, y * 0.8 - 1.2, 8.0),
            ),
            material: material(i + 3),
        });
    }
    for i in 0..OUTSIDE {
        draws.push(DrawItem {
            mesh,
            world_from_object: Mat4::from_translation(Vec3::new(i as f32, 0.0, 20.0)),
            material: material(i),
        });
    }
    let in_frustum = (1 + BEHIND + IN_FRONT) as u32;

    let image_bytes = u64::from(SIZE * SIZE * 4);
    let readback_size = image_bytes + 40;
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(readback_size)
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

    // Renders one frame; returns the image and the (early, late) instance counts.
    let mut render = |occlusion: Option<&Occlusion>| -> (Vec<u8>, Option<(u32, u32)>) {
        // SAFETY: one-shot recording and submission, then wait.
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
        let two_phase;
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, {
                    frame_number += 1;
                    frame_number
                })
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(color_format, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(depth_format, SIZE, SIZE));
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: readback,
                    size: readback_size,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            let passes = renderer
                .add_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    &draws,
                    &ForwardView {
                        camera,
                        light,
                        color,
                        depth,
                        clear: Some([0.0, 0.0, 0.25, 1.0]),
                        occlusion,
                    },
                )
                .unwrap();
            assert_eq!(passes.batches, 1);
            two_phase = passes.occlusion;
            let commands = passes.commands.unwrap();
            // One VkDrawIndexedIndirectCommand (5 words) per phase.
            let command_bytes = if two_phase { 40 } else { 20 };
            fg.add_pass("read back")
                .read(color, Access::TransferRead)
                .read(commands, Access::TransferRead)
                .write(out, Access::TransferWrite)
                .execute(move |ctx| {
                    let region = vk::BufferImageCopy {
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_extent: vk::Extent3D {
                            width: SIZE,
                            height: SIZE,
                            depth: 1,
                        },
                        ..Default::default()
                    };
                    let commands_region = vk::BufferCopy {
                        src_offset: 0,
                        dst_offset: image_bytes,
                        size: command_bytes,
                    };
                    // SAFETY: recording; layouts arranged by the graph.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(color),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[region],
                        );
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(commands),
                            ctx.buffer(out),
                            &[commands_region],
                        );
                    }
                });
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
        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
        let word = |i: usize| {
            let at = image_bytes as usize + 4 * i;
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        // (vertex_count, instance_count, first_vertex, first_instance) per phase.
        let counts = two_phase.then(|| (word(1), word(6)));
        if !two_phase {
            assert_eq!(word(1), in_frustum, "frustum culling alone");
        }
        (bytes[..image_bytes as usize].to_vec(), counts)
    };

    let (reference, _) = render(None);
    let background = reference
        .chunks_exact(4)
        .filter(|p| *p == [0, 0, 64, 255])
        .count();
    assert_eq!(background, 0, "the wall fills the image");

    let mut frames = Vec::new();
    for frame in 1..=3 {
        let (image, counts) = render(Some(&occlusion));
        let (early, late) = counts.unwrap();
        println!("frame {frame}: early {early}, late {late}");
        let differing = image
            .chunks_exact(4)
            .zip(reference.chunks_exact(4))
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(
            differing, 0,
            "frame {frame}: occlusion culling changed the image"
        );
        frames.push((early, late));
    }
    let visible = (1 + IN_FRONT) as u32;
    assert_eq!(frames[0], (0, in_frustum), "first frame: all late");
    assert_eq!(frames[1], (in_frustum, 0), "second frame: all early");
    assert_eq!(
        frames[2],
        (visible, 0),
        "third frame: only the wall and what is in front of it"
    );

    occlusion.reset();
    let (image, counts) = render(Some(&occlusion));
    assert_eq!(counts, Some((0, in_frustum)), "after reset: all late again");
    assert!(image == reference);

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        occlusion.destroy(&mut allocator);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
    drop(renderer);
    drop(bindless);
    drop(allocator);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
