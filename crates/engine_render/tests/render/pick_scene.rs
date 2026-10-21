//! Picks among the persistent scene's candidates (ADR 0109), under the validation layer
//! with synchronization validation: the instances whose world box reaches the pixels,
//! drawn alone.
//!
//! Quads face the camera on its axis, nearest first: D blended, A opaque, B masked and
//! invisible (its alpha is zero), C opaque and largest; E opaque to the side. At the view's
//! middle the square picks D while transparent surfaces count and A otherwise; the list
//! holds D, A, C front to back (A and C without D when transparent surfaces do not count),
//! never B or E; a pixel outside the view picks nothing.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, ForwardRenderer, ForwardView, ShadowSettings, ViewFeatures,
};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData};
use engine_render::pick::{ListRequest, PickPass, PickRequest, PickView, Picker};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render scene pick test"),
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
const FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

#[test]
fn scene_picks_draw_the_candidates_under_the_pixel() {
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
        FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut picker = Picker::new(&device, &mut allocator, &bindless, 1).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mut textures = TexturePool::new(&device, 16 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let mesh = geometry
        .add_mesh(&MeshData {
            positions: &[
                [-0.5, -0.5, 0.0],
                [0.5, -0.5, 0.0],
                [0.5, 0.5, 0.0],
                [-0.5, 0.5, 0.0],
            ],
            normals: &[[0.0, 0.0, 1.0]; 4],
            tangents: &[],
            uvs: &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
            indices: &[0, 1, 2, 0, 2, 3],
            meshlets: &[],
        })
        .unwrap();
    let opaque = materials.add(MaterialDesc::solid([0.5; 3], 0.0, 0.5));
    let masked = materials.add(MaterialDesc {
        base_color: [1.0, 1.0, 1.0, 0.0],
        alpha: AlphaMode::Mask { cutoff: 0.5 },
        ..MaterialDesc::default()
    });
    let blended = materials.add(MaterialDesc {
        base_color: [1.0, 1.0, 1.0, 0.5],
        alpha: AlphaMode::Blend,
        ..MaterialDesc::default()
    });
    let mut scene = GpuScene::new(&device, &bindless, 16, 1).unwrap();
    let mut quad = |x: f32, z: f32, scale: f32, material| {
        scene
            .insert(SceneInstance {
                mesh,
                material,
                world_from_object: Mat4::from_scale_rotation_translation(
                    Vec3::splat(scale),
                    Quat::IDENTITY,
                    Vec3::new(x, 0.0, z),
                ),
            })
            .slot()
    };
    let e = quad(1.5, -3.0, 0.5, opaque);
    let c = quad(0.0, -5.0, 2.0, opaque);
    let b = quad(0.0, -3.5, 1.0, masked);
    let a = quad(0.0, -3.0, 0.5, opaque);
    let dd = quad(0.0, -2.0, 0.4, blended);
    let clip_from_world = vulkan_perspective(60f32.to_radians(), 1.0, 0.1, 100.0)
        * look_at(Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0), Vec3::Y);
    let camera = Camera {
        clip_from_world,
        position: Vec3::ZERO,
    };
    let light = DirectionalLight {
        direction: Vec3::new(0.0, -1.0, 0.0),
        color: Vec3::ONE,
        ambient: Vec3::splat(0.1),
    };
    let view = PickView {
        clip_from_world,
        size: (SIZE, SIZE),
    };
    let middle = SIZE / 2;
    let square = |transparent| PickRequest {
        x: middle,
        y: middle,
        transparent,
    };
    let list = |transparent| ListRequest {
        x: middle,
        y: middle,
        radius: 3,
        transparent,
    };
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
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
    // Each frame: the results of the frame before, then its own picks (with transparent
    // surfaces, without them, then outside the view).
    let mut squares = Vec::new();
    let mut lists = Vec::new();
    for frame_number in 1..=4u64 {
        if let Some(r) = picker.poll(frame_number) {
            squares.push((r.request, r.nearest(0)));
        }
        if let Some(r) = picker.poll_list(frame_number) {
            lists.push((r.request, r.hits.iter().map(|h| h.0).collect::<Vec<_>>()));
        }
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
            let frame = scene
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
            let color = fg.create_texture("color", TextureDesc::new_2d(FORMAT, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(depth_format, SIZE, SIZE));
            renderer
                .add_scene_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    &frame,
                    &ForwardView {
                        camera,
                        light,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
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
            let (pick, listed) = match frame_number {
                1 => (square(true), list(true)),
                2 => (square(false), list(false)),
                _ => (
                    PickRequest {
                        x: SIZE + 10,
                        ..square(true)
                    },
                    ListRequest {
                        x: SIZE + 10,
                        ..list(true)
                    },
                ),
            };
            let picked = picker.add_scene_pass(
                &mut fg,
                &mut ring,
                &bindless,
                &geo,
                &mats,
                &frame,
                &view,
                pick,
                frame_number,
            );
            let listed = picker.add_list_pass(
                &mut fg,
                &mut ring,
                &bindless,
                &geo,
                &mats,
                &frame,
                &view,
                listed,
                frame_number,
            );
            let expected = if frame_number <= 2 {
                PickPass::Declared
            } else {
                PickPass::Empty
            };
            assert_eq!(
                (picked, listed),
                (expected, expected),
                "frame {frame_number}"
            );
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
    println!("squares {squares:?}");
    println!("lists {lists:?}");
    assert_eq!(
        squares,
        [(square(true), Some(dd)), (square(false), Some(a))],
        "the nearest surface, the blended one only while it counts"
    );
    assert_eq!(
        lists,
        [(list(true), vec![dd, a, c]), (list(false), vec![a, c])],
        "front to back, the hidden ones too; never the invisible one or the one aside"
    );
    assert!(
        ![b, e]
            .iter()
            .any(|s| lists.iter().any(|(_, l)| l.contains(s)))
    );

    // SAFETY: the device is idle.
    unsafe {
        picker.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        scene.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(pool, None);
    }
    drop(picker);
    drop(scene);
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
