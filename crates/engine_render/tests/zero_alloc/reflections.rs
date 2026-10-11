//! CLAUDE.md §5: zero heap allocations per frame in steady state, for local reflections
//! (ADR 0073): reflection probes (one captured again every few frames, its faces over
//! several frames: the capture views, the face copies, mips and prefilter), screen-space
//! reflections at half resolution with their filters and history, the composite, under
//! TAA's jitter, declared and recorded.
//!
//! A counting global allocator observes the process while frames are declared and
//! recorded after a warm-up (driver allocations happen in C and are not counted). One
//! test per file, so nothing else allocates concurrently. Every frame is submitted and
//! waited for: with async compute (ADR 0065) the graph submits its earlier segments itself
//! during `execute`.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
};
use engine_render::graph::{RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::reflections::{
    ProbeSettings, ReflectionProbe, ReflectionProbes, Reflections, ScreenSpaceReflections,
    SsrSettings,
};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance as VkInstance, InstanceDesc,
};
use std::sync::atomic::Ordering;

use crate::ALLOCATIONS;

#[test]
fn steady_state_reflection_frames_do_not_allocate() {
    let _alone = crate::alone();
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let setup = || -> Result<(VkInstance, Device), engine_rhi::RhiError> {
        let instance = VkInstance::new(&InstanceDesc {
            app_name: String::from("engine_render reflections zero-alloc test"),
            validation: false,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: false,
        })?;
        let adapter = instance.select_adapter(&AdapterPreference::HighPerformance)?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    let (_instance, device) = match setup() {
        Ok(pair) => pair,
        Err(e) if !required => {
            println!("skipping: {e}");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let (color_format, depth_format) = (vk::Format::R16G16B16A16_SFLOAT, vk::Format::D32_SFLOAT);
    let mut renderer = ForwardRenderer::new(
        &device,
        &bindless,
        color_format,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    renderer.set_reflections(&device, &bindless, true).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 2).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 2);
    let mut materials = MaterialTable::new(2);
    let opaque = materials.add(MaterialDesc::solid([0.6, 0.5, 0.4], 0.0, 0.5));
    let glass = materials.add(MaterialDesc {
        base_color: [0.4, 0.8, 1.0, 0.4],
        alpha: AlphaMode::Blend,
        ..MaterialDesc::default()
    });
    let draws: Vec<DrawItem> = (0..64)
        .map(|i| DrawItem {
            mesh,
            material: if i % 8 == 0 { glass } else { opaque },
            world_from_object: Mat4::from_translation(Vec3::new(
                (i % 8) as f32 * 2.0 - 8.0,
                0.0,
                -((i / 8) as f32) * 2.0,
            )),
        })
        .collect();
    let mut ring = UploadRing::new(&device, &mut allocator, 2, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 2);
    let temporal = Temporal::new(&device, &mut allocator, 64).unwrap();
    let mut environment =
        Environment::new(&device, &mut allocator, &bindless, Sky::default()).unwrap();
    // Two probes, two faces a frame.
    let mut probes = ReflectionProbes::new(
        &device,
        &mut allocator,
        &bindless,
        ProbeSettings {
            capacity: 2,
            faces_per_frame: 2,
            ..ProbeSettings::default()
        },
    )
    .unwrap();
    let probe = probes
        .add(ReflectionProbe::room(
            Vec3::new(-4.0, 2.0, -6.0),
            Vec3::new(4.0, 2.0, 8.0),
        ))
        .unwrap();
    probes
        .add(ReflectionProbe::room(
            Vec3::new(4.0, 2.0, -6.0),
            Vec3::new(4.0, 2.0, 8.0),
        ))
        .unwrap();
    let mut ssr = ScreenSpaceReflections::new(
        &device,
        &mut allocator,
        &bindless,
        640,
        360,
        SsrSettings::default(),
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
    let eye = Vec3::new(0.0, 4.0, 12.0);
    let camera = Camera {
        clip_from_world: vulkan_perspective(1.0, 16.0 / 9.0, 0.1, 500.0)
            * look_at(eye, Vec3::new(0.0, 0.0, -4.0), Vec3::Y),
        position: eye,
    };
    let mut frame_number = 0u64;
    // `refresh`: the probe is captured again continuously (every frame draws faces).
    let mut frame = |i: usize, refresh: bool| {
        let light = DirectionalLight {
            direction: Vec3::new(-0.3, -1.0, -0.2),
            color: Vec3::splat(3.0),
            ambient: Vec3::ZERO,
        };
        if refresh {
            probes.refresh(probe);
        }
        frame_number += 1;
        let slot = (frame_number % 2) as usize;
        // SAFETY: the previous frame finished (waited below).
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
        ring.begin_slot(slot);
        {
            // SAFETY: every earlier frame was waited for.
            let mut fg = unsafe { graph.begin_slot(slot, cmd) };
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(color_format, 640, 360));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(depth_format, 640, 360));
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
                        temporal: Some(&temporal),
                        lights: &[],
                        decals: &[],
                        environment: Some(&environment),
                        reflections: Reflections {
                            probes: Some(&probes),
                            ssr: Some(&ssr),
                        },
                    },
                )
                .unwrap();
            std::hint::black_box(passes);
            fg.add_pass("present stand-in")
                .read(color, engine_render::graph::Access::TransferRead)
                .side_effects()
                .execute(|_| {});
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        // SAFETY: recorded above; submitted after the graph's own segments, then waited
        // for.
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
        std::hint::black_box(i);
        probes.stats().faces
    };

    // Captures every frame (the probe always waits for a new one), then a static scene
    // (cached probes). A capture now and then also allocates: the render graph frees the
    // transient images a frame did not use, and the next capture makes them again (see
    // ADR 0073).
    for (name, refresh) in [("capturing every frame", true), ("cached probes", false)] {
        for i in 0..30 {
            frame(i, refresh);
        }
        let mut faces = 0;
        // Shading variants the frames have not drawn with are made on the device's
        // warm-up thread (ADR 0079), which allocates: done before counting.
        device.wait_for_pipeline_warm_up();
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        for i in 30..130 {
            faces += frame(i, refresh);
        }
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        println!("{name}, 100 frames: {allocations} heap allocations ({faces} probe faces drawn)");
        assert_eq!(allocations, 0, "{name}");
        assert_eq!(faces > 0, refresh, "{name}: {faces} faces drawn");
    }

    device.wait_idle().unwrap();
    let mut temporal = temporal;
    // SAFETY: nothing is in use.
    unsafe {
        environment.destroy(&mut allocator, &bindless);
        probes.destroy(&mut allocator, &bindless);
        ssr.destroy(&mut allocator, &bindless);
        temporal.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(pool, None);
    }
}
