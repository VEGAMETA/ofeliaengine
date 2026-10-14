//! Forward PBR with a shadow map, through the render graph, under the validation layer with
//! synchronization validation. Checks lighting and shadowing at projected points.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Vec3, Vec4, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::post::{HDR_FORMAT, Tonemap};
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
            app_name: String::from("engine_render forward test"),
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

const SIZE: u32 = 128;

#[test]
fn pbr_lighting_and_shadows() {
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
    let hdr_renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings {
            resolution: 1024,
            ..ShadowSettings::default()
        },
    )
    .unwrap();
    let tonemap = Tonemap::new(&device, &bindless, color_format).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let red = materials.add(MaterialDesc::solid([0.9, 0.2, 0.2], 0.0, 0.6));
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
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

    // A ground slab with its top at y = 0 and a cube floating above the origin.
    let draws = [
        DrawItem {
            mesh,
            material: materials.default_material(),
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::new(8.0, 0.2, 8.0),
                Default::default(),
                Vec3::new(0.0, -0.1, 0.0),
            ),
        },
        DrawItem {
            mesh,
            material: red,
            world_from_object: Mat4::from_translation(Vec3::new(0.0, 1.5, 0.0)),
        },
    ];
    let eye = Vec3::new(0.0, 5.0, 6.0);
    let clip_from_world = vulkan_perspective(45f32.to_radians(), 1.0, 0.1, 100.0)
        * look_at(eye, Vec3::new(0.0, 0.5, 0.0), Vec3::Y);
    let camera = Camera {
        clip_from_world,
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::NEG_Y,
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.05),
    };

    for (frame, hdr) in [false, true].into_iter().enumerate() {
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
                    size: readback_size,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            if hdr {
                let scene = fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, SIZE, SIZE));
                hdr_renderer.add_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    &draws,
                    &ForwardView {
                        camera,
                        light,
                        color: scene,
                        depth,
                        clear: Some([0.0, 0.0, 0.25, 1.0]),
                        occlusion: None,
                        temporal: None,
                        shadow_cache: None,
                        lights: &[],
                        decals: &[],
                        environment: None,
                        reflections: Default::default(),
                        gi: None,
                    },
                );
                tonemap.add_pass(&mut fg, &bindless, scene, color, 1.0);
            } else {
                renderer.add_passes(
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
                        clear: Some([0.0, 0.0, 0.25, 1.0]),
                        occlusion: None,
                        temporal: None,
                        shadow_cache: None,
                        lights: &[],
                        decals: &[],
                        environment: None,
                        reflections: Default::default(),
                        gi: None,
                    },
                );
            }
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
        println!(
            "graph: {:?}; ring used {} bytes",
            graph.stats(),
            ring.used()
        );

        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
        let at = |world: Vec3| {
            let clip = clip_from_world * Vec4::new(world.x, world.y, world.z, 1.0);
            let ndc = clip.truncate() / clip.w;
            let x = ((ndc.x * 0.5 + 0.5) * SIZE as f32) as u32;
            let y = ((ndc.y * 0.5 + 0.5) * SIZE as f32) as u32;
            let i = ((y.min(SIZE - 1) * SIZE + x.min(SIZE - 1)) * 4) as usize;
            [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
        };
        let luma = |p: [u8; 4]| u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]);
        let shadowed = at(Vec3::new(0.0, 0.0, 0.0));
        let lit = at(Vec3::new(2.5, 0.0, 0.0));
        let top = at(Vec3::new(0.0, 2.0, 0.0));
        let corner = [bytes[4], bytes[5], bytes[6], bytes[7]];
        println!(
            "hdr+tonemap={hdr}: shadowed ground {shadowed:?}, lit ground {lit:?}, cube top {top:?}, corner {corner:?}"
        );
        assert_eq!(corner, [0, 0, 64, 255], "background");
        assert!(
            luma(lit) > 3 * luma(shadowed),
            "lit ground {lit:?} should be much brighter than the shadow {shadowed:?}"
        );
        // In shadow only the ambient term remains: 0.05 × 0.8 ≈ 10/255 per channel.
        assert!(shadowed[0] <= 20, "{shadowed:?}");
        // The red cube's lit top face is red.
        assert!(top[0] > 2 * u8::max(top[1], 1) && top[0] > 100, "{top:?}");
        if hdr {
            // Tonemapping compresses highlights: nothing reaches full white.
            assert!(lit[0] < 250 && top[0] < 250, "{lit:?} {top:?}");
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
    drop(hdr_renderer);
    drop(tonemap);
    drop(bindless);
    drop(allocator);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
