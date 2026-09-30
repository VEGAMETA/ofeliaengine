//! Transparent surfaces (ADR 0054), under the validation layer with synchronization
//! validation.
//!
//! Emissive-only materials (no light, no ambient) make every pixel's color exactly the
//! blend of what covers it. Two blended layers (alpha 0.5 and 0.3) in front of an opaque
//! background must composite back to front — whatever the draw list's order, with and
//! without two-phase occlusion culling — and the reactive mask must hold their combined
//! coverage 1 − (1 − 0.5)(1 − 0.3).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Occlusion, REACTIVE_FORMAT,
    ShadowSettings, Temporal,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::post::HDR_FORMAT;
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render transparency test"),
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

const SIZE: u32 = 64;
const BACKGROUND: [f32; 3] = [0.2, 0.4, 0.8];
const FAR: ([f32; 3], f32) = ([1.0, 0.0, 0.0], 0.5);
const NEAR: ([f32; 3], f32) = ([0.0, 1.0, 0.0], 0.3);

fn half(bits: u16) -> f32 {
    engine_core::half::to_f32(bits)
}

#[test]
fn blended_layers_composite_back_to_front() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
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
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let emissive = |color: [f32; 3], alpha: Option<f32>| MaterialDesc {
        base_color: [0.0, 0.0, 0.0, alpha.unwrap_or(1.0)],
        emissive: color,
        alpha: if alpha.is_some() {
            AlphaMode::Blend
        } else {
            AlphaMode::Opaque
        },
        ..MaterialDesc::solid([0.0; 3], 0.0, 1.0)
    };
    let background = materials.add(emissive(BACKGROUND, None));
    let far = materials.add(emissive(FAR.0, Some(FAR.1)));
    let near = materials.add(emissive(NEAR.0, Some(NEAR.1)));
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut occlusion = Occlusion::new(&device, &mut allocator, 64).unwrap();
    let mut temporal = Temporal::new(&device, &mut allocator, 64).unwrap();

    let eye = Vec3::ZERO;
    let camera = Camera {
        clip_from_world: vulkan_perspective(60f32.to_radians(), 1.0, 0.1, 100.0)
            * look_at(eye, Vec3::new(0.0, 0.0, -1.0), Vec3::Y),
        position: eye,
    };
    // No light at all: every surface shows its emission only.
    let light = DirectionalLight {
        direction: Vec3::new(0.0, -1.0, 0.0),
        color: Vec3::ZERO,
        ambient: Vec3::ZERO,
    };
    // Thin slabs across the middle of the view; the background fills it.
    let slab = |z: f32, size: f32, material| DrawItem {
        mesh,
        material,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(size, size, 0.1),
            Quat::IDENTITY,
            Vec3::new(0.0, 0.0, z),
        ),
    };
    let wall = slab(-20.0, 100.0, background);
    let far_layer = slab(-10.0, 4.0, far);
    let near_layer = slab(-5.0, 1.0, near);

    // Premultiplied "over", back to front.
    let expected: [f32; 3] = core::array::from_fn(|c| {
        let behind = FAR.0[c] * FAR.1 + (1.0 - FAR.1) * BACKGROUND[c];
        NEAR.0[c] * NEAR.1 + (1.0 - NEAR.1) * behind
    });
    let coverage = 1.0 - (1.0 - FAR.1) * (1.0 - NEAR.1);
    println!("expected center {expected:?}, coverage {coverage}");

    let image_bytes = u64::from(SIZE * SIZE * 8);
    let mask_bytes = u64::from(SIZE * SIZE);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(image_bytes + mask_bytes)
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

    // (draw list, occlusion culling, temporal state)
    let cases = [
        (vec![near_layer, far_layer, wall], false, false),
        (vec![wall, far_layer, near_layer], false, false),
        (vec![far_layer, wall, near_layer], true, false),
        (vec![near_layer, wall, far_layer], false, true),
    ];
    let mut frame_number = 0;
    for (draws, occluded, temporal_state) in &cases {
        // Two frames: the second has last frame's visibility for occlusion culling.
        for _ in 0..2 {
            frame_number += 1;
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
            {
                // SAFETY: nothing is in flight.
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                let geo = geometry
                    .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                    .unwrap();
                textures
                    .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                    .unwrap();
                let mats = materials
                    .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                    .unwrap();
                let color = fg.create_texture("color", TextureDesc::new_2d(HDR_FORMAT, SIZE, SIZE));
                let depth =
                    fg.create_texture("depth", TextureDesc::new_2d(depth_format, SIZE, SIZE));
                let out = fg.import_buffer(
                    "readback",
                    ImportedBuffer {
                        buffer: readback,
                        size: image_bytes + mask_bytes,
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
                        &mats,
                        draws,
                        &ForwardView {
                            camera,
                            light,
                            color,
                            depth,
                            clear: Some([0.0, 0.0, 0.0, 1.0]),
                            occlusion: occluded.then_some(&occlusion),
                            temporal: temporal_state.then_some(&temporal),
                            lights: &[],
                            environment: None,
                        },
                    )
                    .unwrap();
                let reactive = passes.motion.and_then(|m| m.reactive);
                assert_eq!(reactive.is_some(), *temporal_state);
                if let Some(mask) = reactive {
                    assert_eq!(fg.texture_desc(mask).format, REACTIVE_FORMAT);
                }
                let region = |extent: u32, offset: u64| vk::BufferImageCopy {
                    buffer_offset: offset,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width: extent,
                        height: extent,
                        depth: 1,
                    },
                    ..Default::default()
                };
                let mut pass = fg.add_pass("read back").read(color, Access::TransferRead);
                if let Some(mask) = reactive {
                    pass = pass.read(mask, Access::TransferRead);
                }
                pass.write(out, Access::TransferWrite).execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; layouts arranged by the
                    // graph.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(color),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[region(SIZE, 0)],
                        );
                        if let Some(mask) = reactive {
                            ctx.device.cmd_copy_image_to_buffer(
                                ctx.cmd,
                                ctx.image(mask),
                                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                ctx.buffer(out),
                                &[region(SIZE, image_bytes)],
                            );
                        }
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
        }
        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers both images.
        let bytes = unsafe {
            core::slice::from_raw_parts(ptr.as_ptr(), (image_bytes + mask_bytes) as usize)
        };
        let pixel = |x: u32, y: u32| -> [f32; 3] {
            let at = ((y * SIZE + x) * 8) as usize;
            core::array::from_fn(|c| {
                half(u16::from_le_bytes([
                    bytes[at + 2 * c],
                    bytes[at + 2 * c + 1],
                ]))
            })
        };
        let center = pixel(SIZE / 2, SIZE / 2);
        let corner = pixel(2, 2);
        println!(
            "occlusion {occluded}, temporal {temporal_state}: center {center:?}, corner {corner:?}"
        );
        for c in 0..3 {
            assert!(
                (center[c] - expected[c]).abs() <= 2e-3,
                "center {center:?}, expected {expected:?}"
            );
            // Outside both layers: the background, untouched.
            assert!(
                (corner[c] - BACKGROUND[c]).abs() <= 2e-3,
                "corner {corner:?}"
            );
        }
        if *temporal_state {
            let mask = |x: u32, y: u32| {
                f32::from(bytes[(image_bytes + u64::from(y * SIZE + x)) as usize]) / 255.0
            };
            let (center, corner) = (mask(SIZE / 2, SIZE / 2), mask(2, 2));
            println!("reactive mask: center {center}, corner {corner}");
            assert!(
                (center - coverage).abs() <= 1.5 / 255.0,
                "{center} vs {coverage}"
            );
            assert_eq!(corner, 0.0);
        }
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(readback, readback_mem);
        temporal.destroy(&mut allocator);
        occlusion.destroy(&mut allocator);
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
