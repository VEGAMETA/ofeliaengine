//! CLAUDE.md §5: zero heap allocations per frame in steady state, for skinned meshes
//! (ADR 0068): every instance posed every frame, its bounds given to the pool and the
//! GPU scene, the skinning pass, the forward passes and the skinned motion pass declared
//! and recorded.
//!
//! A counting global allocator observes the process while frames are declared and
//! recorded after a warm-up (driver allocations happen in C and are not counted). One
//! test per file, so nothing else allocates concurrently. The frames are recorded, not
//! submitted.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, ForwardRenderer, ForwardView, Occlusion, ShadowSettings, Temporal,
    ViewFeatures,
};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::skinning::{SkinnedMeshData, Skinning};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance as VkInstance, InstanceDesc,
};
use std::sync::atomic::Ordering;

use crate::ALLOCATIONS;

#[test]
fn steady_state_skinning_frames_do_not_allocate() {
    let _alone = crate::alone();
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let setup = || -> Result<(VkInstance, Device), engine_rhi::RhiError> {
        let instance = VkInstance::new(&InstanceDesc {
            app_name: String::from("engine_render skinning zero-alloc test"),
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
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        color_format,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 2).unwrap();
    let mut skinning =
        Skinning::new(&device, &mut allocator, &bindless, depth_format, 1 << 12, 2).unwrap();
    // A cube whose top half follows joint 1, with one and with two sets of influences.
    let block = cube();
    let skins: Vec<_> = [1usize, 2]
        .into_iter()
        .map(|sets| {
            let mut joints = Vec::new();
            let mut weights = Vec::new();
            for p in &block.positions {
                let top = p[1] > 0.0;
                for _ in 0..sets {
                    joints.push([0, 1, 0, 0]);
                    weights.push(if top {
                        [0.2, 0.8, 0.0, 0.0]
                    } else {
                        [1.0, 0.0, 0.0, 0.0]
                    });
                }
            }
            skinning
                .add_skin(&SkinnedMeshData {
                    mesh: block.as_data(),
                    joints: &joints,
                    weights: &weights,
                    joint_count: 2,
                })
                .unwrap()
        })
        .collect();
    let mut textures = TexturePool::new(&device, 1 << 20, 2);
    let mut materials = MaterialTable::new(2);
    let palette: Vec<_> = (0..8)
        .map(|i| materials.add(MaterialDesc::solid([i as f32 / 8.0, 0.5, 0.5], 0.0, 0.5)))
        .collect();
    let mut ring = UploadRing::new(&device, &mut allocator, 2, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 2);
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
    let skinned: Vec<_> = (0..40)
        .map(|i| {
            let (id, mesh) = skinning.add_instance(skins[i % 2], &mut geometry).unwrap();
            let scene_id = scene.insert(SceneInstance {
                mesh,
                material: palette[i % palette.len()],
                world_from_object: place(i, 0.0),
            });
            skinning.set_scene_instance(id, Some(scene_id));
            (id, scene_id)
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
    let mut frame_number = 0u64;
    let mut frame = |i: usize| {
        // Every instance posed and moved every frame.
        let t = i as f32 * 0.05;
        let bend = Mat4::from_rotation_z(t.sin());
        let r = |m: Mat4| {
            let row = |k: usize| m.row(k).to_array();
            let (a, b, c) = (row(0), row(1), row(2));
            [
                a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], c[0], c[1], c[2], c[3],
            ]
        };
        let pose = [r(Mat4::IDENTITY), r(bend)];
        for (k, &(id, scene_id)) in skinned.iter().enumerate() {
            let world = place(k, i as f32);
            skinning.set_pose(id, &pose, &world);
            scene.set_transform(scene_id, &world);
        }
        skinning.update_bounds(&mut geometry, Some(&mut scene));
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
            let skins_frame = skinning
                .begin_frame(
                    &mut fg,
                    &mut ring,
                    &device,
                    &mut allocator,
                    &bindless,
                    &geo,
                    frame_number,
                )
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
            let passes = renderer
                .add_scene_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    &scene_frame,
                    &ForwardView {
                        camera,
                        light,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: Some(&occlusion),
                        temporal: Some(&temporal),
                        shadow_cache: None,
                        virtual_shadows: None,
                        lights: &[],
                        decals: &[],
                        environment: None,
                        reflections: Default::default(),
                        gi: None,
                        features: ViewFeatures::ALL,
                    },
                )
                .unwrap();
            skins_frame
                .add_motion_pass(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    depth,
                    &passes.motion.unwrap(),
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
        // SAFETY: recording command buffer.
        unsafe { d.end_command_buffer(cmd).unwrap() };
    };

    for i in 0..30 {
        frame(i);
    }
    // Shading variants the frames have not drawn with are made on the device's
    // warm-up thread (ADR 0079), which allocates: done before counting.
    device.wait_for_pipeline_warm_up();
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 30..130 {
        frame(i);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!(
        "100 frames of {} skinned instances: {allocations} heap allocations",
        skinned.len()
    );
    assert_eq!(allocations, 0);

    device.wait_idle().unwrap();
    let (mut occlusion, mut temporal) = (occlusion, temporal);
    // SAFETY: nothing is in use.
    unsafe {
        skinning.destroy(&mut allocator);
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
