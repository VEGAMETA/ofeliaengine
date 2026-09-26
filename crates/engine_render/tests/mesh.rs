//! A lit cube rendered through the render graph, checked pixel by pixel, under the
//! validation layer with synchronization validation.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3};
use engine_render::graph::{
    Access, BufferState, ImportedBuffer, LoadOp, RenderGraph, StoreOp, TextureDesc,
};
use engine_render::mesh::{Mesh, MeshRenderer, cube, perspective};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render mesh test"),
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

#[test]
fn lit_cube_through_the_graph() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    println!("bindless capacities: {:?}", bindless.capacities());
    let color_format = vk::Format::R8G8B8A8_UNORM;
    let depth_format = vk::Format::D32_SFLOAT;
    let renderer = MeshRenderer::new(&device, &bindless, color_format, depth_format).unwrap();
    let mut mesh = Mesh::new(&device, &mut allocator, &cube()).unwrap();
    let mut graph = RenderGraph::new(&device, 1);

    let readback_size = u64::from(SIZE * SIZE * 4);
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

    let camera = engine_core::math::look_at(Vec3::new(0.0, 0.0, 3.0), Vec3::ZERO, Vec3::Y);
    let clip_from_world = perspective(45f32.to_radians(), 1.0, 0.1, 100.0) * camera;
    // Straight on: the front face (normal +Z, towards the light) fills the centre.
    // Turned 45° about Y: two faces at 45° to the light meet in the centre.
    // Lifted: the cube's top face (normal +Y) is seen from above at the top of the image.
    let cases = [
        ("front", Mat4::IDENTITY, 204u8),
        (
            "turned",
            Mat4::from_quat(Quat::from_rotation_y(45f32.to_radians())),
            150u8,
        ),
    ];
    for (name, world_from_object, expected_centre) in cases {
        // SAFETY: the device is idle between iterations.
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
        {
            // SAFETY: nothing from slot 0 is in flight (device idle).
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
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
            let (renderer, bindless, mesh) = (&renderer, &bindless, &mesh);
            fg.add_pass("cube")
                .color(color, LoadOp::Clear([0.0, 0.0, 0.25, 1.0]), StoreOp::Store)
                .depth(
                    depth,
                    LoadOp::Clear([1.0, 0.0, 0.0, 0.0]),
                    StoreOp::DontCare,
                )
                .execute(move |ctx| {
                    renderer.draw(ctx, bindless, mesh, clip_from_world, world_from_object);
                });
            fg.add_pass("read back")
                .read(color, Access::TransferRead)
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
                    // SAFETY: recording; layouts arranged by the graph.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(color),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[region],
                        );
                    }
                });
            fg.execute(&mut allocator).unwrap();
        }
        // SAFETY: submit and wait.
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
        // SAFETY: the GPU is idle; the mapping covers the image.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
        let pixel = |x: u32, y: u32| {
            let i = ((y * SIZE + x) * 4) as usize;
            [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
        };
        let centre = pixel(SIZE / 2, SIZE / 2);
        let corner = pixel(1, 1);
        println!("{name}: centre {centre:?}, corner {corner:?}");
        assert_eq!(corner, [0, 0, 64, 255], "{name}: background");
        for channel in &centre[..3] {
            assert!(
                channel.abs_diff(expected_centre) <= 3,
                "{name}: centre {centre:?}, expected {expected_centre}"
            );
        }
        // The cube is about half the image wide: a pixel a quarter in from the left edge
        // is inside it when straight on.
        if name == "front" {
            assert_ne!(pixel(SIZE / 2 - SIZE / 8, SIZE / 2), corner);
        }
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator);
        mesh.destroy(&mut allocator);
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
