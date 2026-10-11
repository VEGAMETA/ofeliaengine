//! Mesh detail levels chosen by the culling pass (ADR 0053), under the validation layer
//! with synchronization validation.
//!
//! One mesh with two coarser levels (errors 0.02 and 0.2 object units) is drawn at
//! distances from 2 to 200 units, some instances scaled up. The culling pass must put each
//! instance in the draw command of the level the CPU's rule picks (the coarsest whose
//! error, seen from the camera, stays within a pixel), and a threshold of 0 must draw only
//! the full mesh.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, LodIndices, cube};
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
            app_name: String::from("engine_render lods test"),
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
const ERRORS: [f32; 3] = [0.0, 0.02, 0.2];

#[test]
fn the_culling_pass_draws_the_level_the_distance_calls_for() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let color_format = vk::Format::R8G8B8A8_UNORM;
    let depth_format = vk::Format::D32_SFLOAT;
    let mut renderer = ForwardRenderer::new(
        &device,
        &bindless,
        color_format,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();

    // The cube with two made-up coarser levels (subsets of its faces): what matters here
    // is which command each instance lands in.
    let cube = cube();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry
        .add_mesh_with_lods(
            &cube.as_data(),
            &[
                LodIndices {
                    indices: &cube.indices[..24],
                    error: ERRORS[1],
                },
                LodIndices {
                    indices: &cube.indices[..12],
                    error: ERRORS[2],
                },
            ],
        )
        .unwrap();
    let info = *geometry.mesh(mesh).unwrap();
    assert_eq!(info.lod_count, 3);
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let material = materials.add(MaterialDesc::solid([0.8, 0.8, 0.8], 0.0, 0.5));
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);

    let eye = Vec3::ZERO;
    let fov = 60f32.to_radians();
    let clip_from_world =
        vulkan_perspective(fov, 1.0, 0.1, 500.0) * look_at(eye, Vec3::new(0.0, 0.0, -1.0), Vec3::Y);
    let camera = Camera {
        clip_from_world,
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.5),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.1),
    };

    // The rule, on the CPU: pixels per unit at distance 1 is the projection's scale times
    // half the height; a level is fine when error × scale × that ≤ the box's nearest
    // distance (its center's minus its radius).
    let pixels_per_unit = 1.0 / (fov * 0.5).tan() * SIZE as f32 * 0.5;
    let half = (info.max - info.min) * 0.5;
    let level = |distance: f32, scale: f32| {
        let nearest = distance - (half * scale).length();
        (0..3)
            .rev()
            .find(|&k| ERRORS[k] * scale * pixels_per_unit <= nearest)
            .unwrap_or(0)
    };
    // (distance, scale), away from the thresholds (about 5.3 and 45 units at scale 1).
    let placed = [
        (2.0, 1.0),
        (3.0, 1.0),
        (4.0, 1.0),
        (8.0, 2.0),
        (8.0, 1.0),
        (12.0, 1.0),
        (20.0, 1.0),
        (20.0, 2.0),
        (30.0, 1.0),
        (60.0, 1.0),
        (100.0, 1.0),
        (200.0, 1.0),
    ];
    let draws: Vec<DrawItem> = placed
        .iter()
        .map(|&(distance, scale)| DrawItem {
            mesh,
            material,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::splat(scale),
                Quat::IDENTITY,
                Vec3::new(0.0, 0.0, -distance),
            ),
        })
        .collect();
    let mut expected = [0u32; 3];
    for &(distance, scale) in &placed {
        expected[level(distance, scale)] += 1;
    }
    println!("CPU rule: {expected:?} instances per level");
    assert!(expected.iter().all(|&n| n > 0), "{expected:?}");

    let command_bytes = 20 * 3u64;
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(command_bytes)
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

    for (frame, threshold) in [1.0f32, 0.0].into_iter().enumerate() {
        renderer.set_lod_threshold(threshold);
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
            let frame_number = frame as u64 + 1;
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                .unwrap();
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(color_format, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(depth_format, SIZE, SIZE));
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: readback,
                    size: command_bytes,
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
                    &draws,
                    &ForwardView {
                        camera,
                        light,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
                        lights: &[],
                        decals: &[],
                        environment: None,
                        reflections: Default::default(),
                    },
                )
                .unwrap();
            assert_eq!(passes.batches, 1);
            assert_eq!(passes.draw_commands, 3);
            let commands = passes.commands.unwrap();
            fg.add_pass("read back")
                .read(commands, Access::TransferRead)
                .write(out, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope.
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(commands),
                            ctx.buffer(out),
                            &[vk::BufferCopy {
                                src_offset: 0,
                                dst_offset: 0,
                                size: command_bytes,
                            }],
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
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), command_bytes as usize) };
        let word = |i: usize| {
            u32::from_le_bytes([
                bytes[4 * i],
                bytes[4 * i + 1],
                bytes[4 * i + 2],
                bytes[4 * i + 3],
            ])
        };
        // index_count, instance_count, first_index, vertex_offset, first_instance.
        let mut counted = [0u32; 3];
        for (k, count) in counted.iter_mut().enumerate() {
            assert_eq!(
                word(k * 5),
                info.lods[k].index_count,
                "level {k} index count"
            );
            assert_eq!(
                word(k * 5 + 2),
                info.lods[k].first_index,
                "level {k} first index"
            );
            *count = word(k * 5 + 1);
        }
        println!("threshold {threshold}: GPU drew {counted:?} instances per level");
        if threshold > 0.0 {
            assert_eq!(counted, expected);
        } else {
            assert_eq!(counted, [placed.len() as u32, 0, 0]);
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
        d.destroy_command_pool(pool, None);
    }
    drop(renderer);
    drop(bindless);
    drop(allocator);
    drop(device);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
