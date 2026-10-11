//! Motion vectors and jitter (ADR 0043), under the validation layer with synchronization
//! validation: a moving object moves by exactly its displacement, a moving camera moves
//! everything including the background, jitter never shows in the vectors, a reset stops
//! motion for a frame, objects without a previous transform are still, and an empty frame
//! reports zero motion. With and without the depth prepass and occlusion culling.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::half;
use engine_core::math::{Mat4, Vec3, look_at, vulkan_orthographic};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, MOTION_FORMAT, Occlusion,
    ShadowSettings, Temporal,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::MaterialTable;
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const SIZE: u32 = 64;
/// The orthographic view spans 8 world units: one unit is 8 pixels, 1/8 of the target.
const SPAN: f32 = 8.0;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render motion test"),
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

/// A camera looking down -Z at (x, 0).
fn camera(x: f32) -> Camera {
    let eye = Vec3::new(x, 0.0, 10.0);
    Camera {
        clip_from_world: vulkan_orthographic(
            -SPAN / 2.0,
            SPAN / 2.0,
            -SPAN / 2.0,
            SPAN / 2.0,
            0.1,
            50.0,
        ) * look_at(eye, Vec3::new(x, 0.0, 0.0), Vec3::Y),
        position: eye,
    }
}

#[test]
fn motion_vectors() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut renderer = ForwardRenderer::new(
        &device,
        &bindless,
        vk::Format::R16G16B16A16_SFLOAT,
        vk::Format::D32_SFLOAT,
        ShadowSettings {
            resolution: 512,
            ..ShadowSettings::default()
        },
    )
    .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let temporal = Temporal::new(&device, &mut allocator, 16).unwrap();
    let occlusion = Occlusion::new(&device, &mut allocator, 16).unwrap();
    let bytes = u64::from(SIZE * SIZE * 4);
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes)
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
        direction: Vec3::new(-0.3, -1.0, -0.4),
        color: Vec3::splat(2.0),
        ambient: Vec3::splat(0.1),
    };
    let mut frame_number = 0u64;

    // Renders one frame: a unit cube (scaled 2) at `x` (nothing for `None`), the camera at
    // `camera_x`; returns the motion vectors in pixels and the frame's jitter.
    let mut render = |x: Option<f32>,
                      camera_x: f32,
                      use_occlusion: bool,
                      renderer: &ForwardRenderer|
     -> (Vec<[f32; 2]>, [f32; 2], bool) {
        let cube = x.map(|x| DrawItem {
            mesh,
            material: materials.default_material(),
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::splat(2.0),
                Default::default(),
                Vec3::new(x, 0.0, 0.0),
            ),
        });
        let draws = cube.as_slice();
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
        frame_number += 1;
        let motion;
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
            let color = fg.create_texture(
                "color",
                TextureDesc::new_2d(vk::Format::R16G16B16A16_SFLOAT, SIZE, SIZE),
            );
            let depth = fg.create_texture(
                "depth",
                TextureDesc::new_2d(vk::Format::D32_SFLOAT, SIZE, SIZE),
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
                        camera: camera(camera_x),
                        light,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: use_occlusion.then_some(&occlusion),
                        temporal: Some(&temporal),
                        lights: &[],
                        decals: &[],
                        environment: None,
                        reflections: Default::default(),
                    },
                )
                .unwrap();
            motion = passes.motion.unwrap();
            let velocity = motion.velocity;
            assert_eq!(fg.texture_desc(velocity).format, MOTION_FORMAT);
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: readback,
                    size: bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            fg.add_pass("read back")
                .read(velocity, Access::TransferRead)
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
                            ctx.image(velocity),
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
        let ptr = readback_memory.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), bytes as usize) };
        let pixels = raw
            .chunks_exact(4)
            .map(|p| {
                let h = |a: u8, b: u8| half::to_f32(u16::from_le_bytes([a, b])) * SIZE as f32;
                [h(p[0], p[1]), h(p[2], p[3])]
            })
            .collect();
        (pixels, motion.jitter, motion.reset)
    };
    let at = |v: &[[f32; 2]], x: u32, y: u32| v[(y * SIZE + x) as usize];
    // The cube (2 units wide at x) covers 16 pixels around 32 + 8x; the background is at
    // the left edge.
    let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.02 && (a[1] - b[1]).abs() < 0.02;

    for (prepass, use_occlusion) in [(false, false), (true, false), (false, true), (true, true)] {
        renderer.set_depth_prepass(prepass);
        temporal.reset();
        temporal.set_jitter_phases(8);
        let label = format!("prepass {prepass}, occlusion {use_occlusion}");
        let (v, _, reset) = render(Some(0.0), 0.0, use_occlusion, &renderer);
        assert!(reset, "{label}: the first frame after a reset");
        assert!(
            near(at(&v, 32, 32), [0.0; 2]),
            "{label}: no history, no motion"
        );
        // The cube moves right by 0.5 units: 4 pixels.
        let (v, jitter, reset) = render(Some(0.5), 0.0, use_occlusion, &renderer);
        assert!(!reset);
        assert!(jitter != [0.0; 2], "jittered");
        println!(
            "{label}: cube moved 4 px: {:?}, background {:?}",
            at(&v, 36, 32),
            at(&v, 2, 2)
        );
        assert!(
            near(at(&v, 36, 32), [4.0, 0.0]),
            "{label}: {:?}",
            at(&v, 36, 32)
        );
        assert!(near(at(&v, 2, 2), [0.0, 0.0]), "{label}: still background");
        // The camera moves right by 0.25 units: everything moves 2 pixels left, the
        // background included.
        let (v, _, _) = render(Some(0.5), 0.25, use_occlusion, &renderer);
        println!(
            "{label}: camera moved: {:?}, background {:?}",
            at(&v, 34, 32),
            at(&v, 2, 2)
        );
        assert!(
            near(at(&v, 34, 32), [-2.0, 0.0]),
            "{label}: {:?}",
            at(&v, 34, 32)
        );
        assert!(
            near(at(&v, 2, 2), [-2.0, 0.0]),
            "{label}: {:?}",
            at(&v, 2, 2)
        );
        // Still: no motion, whatever the jitter.
        for _ in 0..3 {
            let (v, jitter, _) = render(Some(0.5), 0.25, use_occlusion, &renderer);
            assert!(
                near(at(&v, 34, 32), [0.0; 2]),
                "{label}: jitter {jitter:?} leaked"
            );
            assert!(near(at(&v, 2, 2), [0.0; 2]));
        }
        // A reset: the next frame has no motion even though the cube jumped.
        temporal.reset();
        let (v, _, reset) = render(Some(-1.0), 0.25, use_occlusion, &renderer);
        assert!(reset && near(at(&v, 26, 32), [0.0; 2]), "{label}: reset");
    }

    // Jitter follows Halton(2, 3), centred.
    temporal.set_jitter_phases(8);
    temporal.reset();
    let jitters: Vec<[f32; 2]> = (0..3)
        .map(|_| render(Some(0.0), 0.0, false, &renderer).1)
        .collect();
    println!("jitter: {jitters:?}");
    assert!(
        jitters
            .iter()
            .all(|j| j[0].abs() <= 0.5 && j[1].abs() <= 0.5)
    );
    assert_ne!(jitters[0], jitters[1]);

    // An empty frame still reports (zero) motion and moves the state on; the cube drawn
    // after it has no previous transform, so it counts as still.
    let (v, _, reset) = render(None, 0.0, false, &renderer);
    assert!(!reset);
    assert!(v.iter().all(|&m| near(m, [0.0; 2])), "empty frame");
    let (v, _, reset) = render(Some(0.5), 0.0, false, &renderer);
    assert!(
        !reset && near(at(&v, 36, 32), [0.0; 2]),
        "{:?}",
        at(&v, 36, 32)
    );

    // SAFETY: the device is idle.
    unsafe {
        let mut temporal = temporal;
        let mut occlusion = occlusion;
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        temporal.destroy(&mut allocator);
        occlusion.destroy(&mut allocator);
        allocator.destroy_buffer(readback, readback_memory);
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
