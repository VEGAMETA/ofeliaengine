//! The decal buffer (ADR 0082) over a corner: a floor and a wall, their depth computed on
//! the CPU, two decals projected down onto the floor. The decal pass alone, read back:
//! where the boxes cover the floor, their order and opacity, and none on the wall (its
//! normal is at right angles to the decals'). Under the validation layer with
//! synchronization validation.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, Vec4, look_at, vulkan_perspective};
use engine_render::decals::{Decal, DecalPasses};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const SIZE: u32 = 128;
/// The wall's plane: z = WALL, facing +z, over the floor (y = 0) in front of it.
const WALL: f32 = -0.25;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render decals test"),
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

fn camera() -> Mat4 {
    vulkan_perspective(1.0, 1.0, 0.1, 50.0)
        * look_at(Vec3::new(0.0, 2.0, 2.5), Vec3::new(0.0, 0.0, 0.0), Vec3::Y)
}

/// Clip-space depth of the corner at every pixel (1 where the ray misses).
fn corner_depth(clip_from_world: Mat4) -> Vec<f32> {
    let inverse = clip_from_world.inverse();
    let mut depth = vec![1.0f32; (SIZE * SIZE) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let ndc = [
                (x as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0,
                (y as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0,
            ];
            let unproject = |z: f32| {
                let p = inverse * Vec4::new(ndc[0], ndc[1], z, 1.0);
                p.truncate() / p.w
            };
            let origin = unproject(0.0);
            let direction = (unproject(1.0) - origin).normalize();
            let mut best = f32::INFINITY;
            // The floor in front of the wall.
            if direction.y < 0.0 {
                let t = -origin.y / direction.y;
                let p = origin + direction * t;
                if p.z >= WALL {
                    best = best.min(t);
                }
            }
            // The wall above the floor.
            if direction.z < 0.0 {
                let t = (WALL - origin.z) / direction.z;
                let p = origin + direction * t;
                if p.y >= 0.0 {
                    best = best.min(t);
                }
            }
            if best.is_finite() {
                let clip = clip_from_world * (origin + direction * best).extend(1.0);
                depth[(y * SIZE + x) as usize] = clip.z / clip.w;
            }
        }
    }
    depth
}

/// The pixel a world point lands on.
fn pixel_of(clip_from_world: Mat4, p: Vec3) -> (u32, u32) {
    let clip = clip_from_world * p.extend(1.0);
    let ndc = clip.truncate() / clip.w;
    let x = ((ndc.x * 0.5 + 0.5) * SIZE as f32) as u32;
    let y = ((ndc.y * 0.5 + 0.5) * SIZE as f32) as u32;
    (x.min(SIZE - 1), y.min(SIZE - 1))
}

#[test]
#[allow(clippy::too_many_lines)]
fn decals_cover_the_floor_in_order_and_skip_the_wall() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let passes = DecalPasses::new(&device, &bindless).unwrap();
    let clip_from_world = camera();
    let depth_values = corner_depth(clip_from_world);
    let pixels = u64::from(SIZE * SIZE);
    let (staging, staging_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(pixels * 4)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Upload,
        )
        .unwrap();
    // SAFETY: the mapping covers the buffer; nothing reads it yet.
    unsafe {
        core::ptr::copy_nonoverlapping(
            depth_values.as_ptr().cast::<u8>(),
            staging_memory.mapped_ptr().unwrap().as_ptr(),
            depth_values.len() * 4,
        );
    }
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(pixels * 12)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
    // Projecting down: the decal's +z is the world's +y.
    let down = Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2);
    let decals = [
        // Red over a metre square, rough and metallic.
        Decal {
            world_from_decal: Mat4::from_scale_rotation_translation(Vec3::ONE, down, Vec3::ZERO),
            tint: [1.0, 0.0, 0.0, 1.0],
            roughness: 0.2,
            metalness: 1.0,
            surface: 1.0,
            ..Decal::default()
        },
        // Half-transparent blue over one quarter of it, drawn after it.
        Decal {
            world_from_decal: Mat4::from_scale_rotation_translation(
                Vec3::new(0.5, 0.5, 1.0),
                down,
                Vec3::new(0.25, 0.0, 0.25),
            ),
            tint: [0.0, 0.0, 1.0, 0.5],
            order: 1,
            ..Decal::default()
        },
    ];
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
    ring.begin_slot(0);
    // SAFETY: one-shot recording.
    unsafe {
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .unwrap();
    }
    {
        // SAFETY: nothing is in flight.
        let mut fg = unsafe { graph.begin_slot(0, cmd) };
        let depth = fg.create_texture(
            "depth",
            TextureDesc::new_2d(vk::Format::R32_SFLOAT, SIZE, SIZE),
        );
        let source = fg.import_buffer(
            "depth values",
            ImportedBuffer {
                buffer: staging,
                size: pixels * 4,
                address: 0,
                initial: BufferState::idle(),
                final_state: None,
            },
        );
        let region = |offset: u64| vk::BufferImageCopy {
            buffer_offset: offset,
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
        fg.add_pass("upload depth")
            .read(source, Access::TransferRead)
            .write(depth, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording; layouts arranged by the graph.
                unsafe {
                    ctx.device.cmd_copy_buffer_to_image(
                        ctx.cmd,
                        ctx.buffer(source),
                        ctx.image(depth),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region(0)],
                    );
                }
            });
        let targets = passes
            .add_passes(
                &mut fg,
                &mut ring,
                &bindless,
                depth,
                clip_from_world,
                &decals,
            )
            .expect("the decals reach the view");
        let out = fg.import_buffer(
            "readback",
            ImportedBuffer {
                buffer: readback,
                size: pixels * 12,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        fg.add_pass("read back")
            .read(targets.albedo, Access::TransferRead)
            .read(targets.normal, Access::TransferRead)
            .read(targets.surface, Access::TransferRead)
            .write(out, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording; layouts arranged by the graph; the buffer holds all three.
                unsafe {
                    for (k, t) in [targets.albedo, targets.normal, targets.surface]
                        .into_iter()
                        .enumerate()
                    {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(t),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[region(k as u64 * pixels * 4)],
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
    let ptr = readback_memory.mapped_ptr().unwrap();
    // SAFETY: the GPU is idle; the mapping covers the three targets.
    let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), (pixels * 12) as usize) };
    let texel = |target: u64, p: Vec3| {
        let (x, y) = pixel_of(clip_from_world, p);
        let i = (target * pixels * 4 + u64::from(y * SIZE + x) * 4) as usize;
        [raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]
    };
    let near = |a: u8, b: f32| (f32::from(a) / 255.0 - b).abs() <= 2.0 / 255.0;
    // Red alone; red under half the blue; the floor outside; the wall inside the box.
    let red = texel(0, Vec3::new(-0.25, 0.0, 0.25));
    let mixed = texel(0, Vec3::new(0.25, 0.0, 0.25));
    let outside = texel(0, Vec3::new(0.8, 0.0, 0.1));
    let wall = texel(0, Vec3::new(0.0, 0.3, WALL));
    let by_the_wall = texel(0, Vec3::new(-0.2, 0.0, -0.15));
    println!(
        "albedo texels (rgba8): red {red:?}, red and half blue {mixed:?}, outside {outside:?}, wall {wall:?}, floor by the wall {by_the_wall:?}"
    );
    assert!(
        near(red[0], 1.0) && near(red[2], 0.0) && near(red[3], 1.0),
        "{red:?}"
    );
    assert!(
        near(mixed[0], 0.5) && near(mixed[2], 0.5) && near(mixed[3], 1.0),
        "{mixed:?}"
    );
    assert_eq!(outside[3], 0, "{outside:?}");
    assert_eq!(
        wall[3], 0,
        "a wall at right angles takes no decal: {wall:?}"
    );
    assert!(
        near(by_the_wall[0], 1.0) && near(by_the_wall[3], 1.0),
        "{by_the_wall:?}"
    );
    // No normal maps: no normal coverage. Roughness and metalness of the red decal only.
    assert_eq!(texel(1, Vec3::new(-0.25, 0.0, 0.25))[3], 0);
    let surface = texel(2, Vec3::new(0.25, 0.0, 0.25));
    assert!(
        near(surface[0], 0.2) && near(surface[1], 1.0) && near(surface[2], 1.0),
        "{surface:?}"
    );

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        allocator.destroy_buffer(staging, staging_memory);
        allocator.destroy_buffer(readback, readback_memory);
        d.destroy_command_pool(pool, None);
    }
    drop(passes);
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
