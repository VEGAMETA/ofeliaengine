//! CLAUDE.md §5: zero heap allocations per frame in steady state, for local lights'
//! shadows (ADR 0071): casters moving near shadowed lights every frame (shadows redrawn
//! within the budget), a moving light, lights entering and leaving the view, the sun's
//! contact shadows and moving decals (ADR 0082), through the persistent GPU scene and a
//! draw list, uploaded and declared, culled and drawn, recorded.
//!
//! A counting global allocator observes the process while frames are declared and
//! recorded after a warm-up (driver allocations happen in C and are not counted). One
//! test per file, so nothing else allocates concurrently. The frames are recorded, not
//! submitted.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, Decal, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, LocalLight,
    LocalShadowSettings, Occlusion, ShadowSettings, Spot, Temporal,
};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance as VkInstance, InstanceDesc,
};
use std::sync::atomic::Ordering;

use crate::ALLOCATIONS;

#[test]
fn steady_state_local_shadow_frames_do_not_allocate() {
    let _alone = crate::alone();
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let setup = || -> Result<(VkInstance, Device), engine_rhi::RhiError> {
        let instance = VkInstance::new(&InstanceDesc {
            app_name: String::from("engine_render local shadows zero-alloc test"),
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
    renderer
        .set_local_shadows(
            &device,
            &mut allocator,
            &bindless,
            Some(LocalShadowSettings {
                updates_per_frame: 8,
                ..LocalShadowSettings::default()
            }),
        )
        .unwrap();
    renderer
        .set_contact_shadows(&device, &bindless, Some(Default::default()))
        .unwrap();
    renderer.set_decals(&device, &bindless, true).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 2).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 2);
    let mut materials = MaterialTable::new(2);
    let palette: Vec<_> = (0..8)
        .map(|i| materials.add(MaterialDesc::solid([i as f32 / 8.0, 0.5, 0.5], 0.0, 0.5)))
        .collect();
    let mut ring = UploadRing::new(&device, &mut allocator, 2, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 2);
    // Nothing here is submitted: with async compute (the contact shadows' pass) the graph
    // would submit the earlier segments itself, waiting on work that never runs.
    graph.set_async_compute(false);
    let occlusion = Occlusion::new(&device, &mut allocator, 4096).unwrap();
    let temporal = Temporal::new(&device, &mut allocator, 16).unwrap();
    let mut scene = GpuScene::new(&device, &bindless, 4096, 2).unwrap();
    let place = |i: usize, t: f32| {
        Mat4::from_translation(Vec3::new(
            (i % 50) as f32 - 25.0,
            (t * 0.1 + i as f32).sin(),
            (i / 50) as f32 - 20.0,
        ))
    };
    let mut ids: Vec<_> = (0..2000)
        .map(|i| {
            scene.insert(SceneInstance {
                mesh,
                material: palette[i % palette.len()],
                world_from_object: place(i, 0.0),
            })
        })
        .collect();
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
    let eye = Vec3::new(0.0, 20.0, 30.0);
    let camera = Camera {
        clip_from_world: vulkan_perspective(1.0, 16.0 / 9.0, 0.5, 200.0)
            * look_at(eye, Vec3::ZERO, Vec3::Y),
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.4),
        color: Vec3::splat(2.0),
        ambient: Vec3::splat(0.1),
    };
    // Shadowed point and spot lights over the field; one of them moves.
    let mut lights: Vec<LocalLight> = (0..12)
        .map(|k| LocalLight {
            position: Vec3::new(k as f32 * 4.0 - 22.0, 3.0, (k % 3) as f32 * 8.0 - 12.0),
            range: 6.0,
            color: Vec3::splat(4.0),
            spot: (k % 2 == 0).then_some(Spot {
                direction: Vec3::NEG_Y,
                inner: 0.4,
                outer: 0.8,
            }),
            shadow: true,
        })
        .collect();
    let mut draws: Vec<DrawItem> = (0..500)
        .map(|i| DrawItem {
            mesh,
            material: palette[i % palette.len()],
            world_from_object: place(i, 0.0),
        })
        .collect();
    let mut frame_number = 0u64;
    let mut frame = |i: usize, use_scene: bool| {
        lights[3].position.x = (i as f32 * 0.1).sin() * 3.0 - 10.0;
        // Decals over the field, drifting (in and out of tiles and z-bins).
        let decals: [Decal; 24] = core::array::from_fn(|k| Decal {
            world_from_decal: Mat4::from_scale_rotation_translation(
                Vec3::new(3.0, 3.0, 4.0),
                Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2),
                Vec3::new(
                    (k % 6) as f32 * 8.0 - 22.0 + (i as f32 * 0.07 + k as f32).sin() * 2.0,
                    0.0,
                    (k / 6) as f32 * 6.0 - 18.0,
                ),
            ),
            tint: [0.8, 0.3, 0.2, 0.9],
            surface: 0.5,
            order: (k % 3) as i32,
            ..Decal::default()
        });
        for k in 0..5 {
            let index = (i * 31 + k * 97) % draws.len();
            draws[index].world_from_object = place(index, i as f32);
        }
        // 1 % moves, 10 instances removed and inserted again (churn).
        for k in 0..20 {
            let index = (i * 37 + k * 101) % ids.len();
            scene.set_transform(ids[index], &place(index, i as f32));
        }
        for k in 0..10 {
            let index = (i * 13 + k * 7) % ids.len();
            scene.remove(ids[index]);
            ids[index] = scene.insert(SceneInstance {
                mesh,
                material: palette[(index + i) % palette.len()],
                world_from_object: place(index, i as f32),
            });
        }
        frame_number += 1;
        let slot = (frame_number % 2) as usize;
        // SAFETY: nothing recorded by this test is ever submitted.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
        ring.begin_slot(slot);
        {
            // SAFETY: nothing submitted, so no GPU work uses any slot.
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
            let scene_frame = scene
                .add_passes(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    frame_number,
                )
                .unwrap()
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(color_format, 640, 360));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(depth_format, 640, 360));
            let view = ForwardView {
                camera,
                light,
                color,
                depth,
                clear: Some([0.0; 4]),
                occlusion: Some(&occlusion),
                temporal: Some(&temporal),
                lights: &lights,
                decals: &decals,
                environment: None,
                reflections: Default::default(),
            };
            let passes = if use_scene {
                renderer.add_scene_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    &scene_frame,
                    &view,
                )
            } else {
                renderer.add_passes(&mut fg, &mut ring, &bindless, &geo, &mats, &draws, &view)
            };
            let passes = passes.unwrap();
            assert!(passes.decals.is_some(), "the decals draw");
            std::hint::black_box(passes);
            fg.add_pass("present stand-in")
                .read(color, engine_render::graph::Access::TransferRead)
                .side_effects()
                .execute(|_| {});
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        // SAFETY: recording command buffer.
        unsafe { d.end_command_buffer(cmd).unwrap() };
    };

    for use_scene in [true, false] {
        for i in 0..40 {
            frame(i, use_scene);
        }
        // Shading variants the frames have not drawn with are made on the device's
        // warm-up thread (ADR 0079), which allocates: done before counting.
        device.wait_for_pipeline_warm_up();
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        for i in 40..140 {
            frame(i, use_scene);
        }
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        let stats = renderer.local_shadow_stats();
        println!(
            "100 frames ({}): {allocations} heap allocations; {stats:?}",
            if use_scene { "GPU scene" } else { "draw list" }
        );
        assert!(stats.views_drawn > 0, "shadows are redrawn: {stats:?}");
        assert_eq!(allocations, 0);
    }

    device.wait_idle().unwrap();
    let (mut occlusion, mut temporal) = (occlusion, temporal);
    // SAFETY: nothing is in use.
    unsafe {
        renderer.destroy(&device, &mut allocator, &bindless);
        scene.destroy(&mut allocator);
        occlusion.destroy(&mut allocator);
        temporal.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(pool, None);
    }
}
