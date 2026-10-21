//! Picking (ADR 0109), under the validation layer with synchronization validation: a pick
//! reads the nearest instance under its pixel from the view's own culled commands.
//!
//! Quads face the camera, three in front and one behind each of them along the same ray
//! (half as large on the screen): an opaque one hides its own; a masked one whose alpha is
//! zero everywhere lets the one behind it be picked; a blended one is picked while
//! transparent surfaces are pickable, and the one behind it otherwise. A pixel of the
//! background picks nothing, and a pick at the view's edge draws without errors.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, ViewFeatures,
};
use engine_render::graph::{RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData};
use engine_render::pick::{PickPass, PickRequest, Picker};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render pick test"),
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

/// The view: SIZE² pixels, 60° vertical field of view.
const SIZE: u32 = 256;
const DISTANCE: f32 = 4.0;
const FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

#[test]
fn a_pick_reads_the_nearest_instance_under_its_pixel() {
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
    let positions = [
        [-0.5, -0.5, 0.0],
        [0.5, -0.5, 0.0],
        [0.5, 0.5, 0.0],
        [-0.5, 0.5, 0.0],
    ];
    let mesh = geometry
        .add_mesh(&MeshData {
            positions: &positions,
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
    // In front: opaque (0), masked and invisible (2), blended (4); each one's twin behind
    // it on the same ray (1, 3, 5), opaque.
    let fronts = [
        (Vec3::new(-0.7, 0.0, -DISTANCE), opaque),
        (Vec3::new(0.7, 0.7, -DISTANCE), masked),
        (Vec3::new(0.7, -0.7, -DISTANCE), blended),
    ];
    let mut draws = Vec::new();
    for &(place, material) in &fronts {
        for (at, material) in [(place, material), (place * 1.5, opaque)] {
            draws.push(DrawItem {
                mesh,
                material,
                world_from_object: Mat4::from_scale_rotation_translation(
                    Vec3::ONE,
                    Quat::IDENTITY,
                    at,
                ),
            });
        }
    }
    let fov = 60f32.to_radians();
    let pixels_per_unit = SIZE as f32 * 0.5 / (fov * 0.5).tan();
    let pixel = |p: Vec3| {
        (
            (p.x / -p.z * pixels_per_unit + SIZE as f32 * 0.5) as u32,
            // Vulkan's projection: y down.
            (-p.y / -p.z * pixels_per_unit + SIZE as f32 * 0.5) as u32,
        )
    };
    let request = |(x, y): (u32, u32), transparent| PickRequest { x, y, transparent };
    // Each frame's pick, and the instance its result must name (polled the next frame).
    let picks = [
        (request(pixel(fronts[0].0), true), Some(0)),
        (request(pixel(fronts[1].0), true), Some(3)),
        (request(pixel(fronts[2].0), true), Some(4)),
        (request(pixel(fronts[2].0), false), Some(5)),
        (request((4, 4), true), None),
        (request((0, SIZE / 2), true), None),
    ];
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let camera = Camera {
        clip_from_world: vulkan_perspective(fov, 1.0, 0.1, 100.0)
            * look_at(Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0), Vec3::Y),
        position: Vec3::ZERO,
    };
    let light = DirectionalLight {
        direction: Vec3::new(0.0, -1.0, 0.0),
        color: Vec3::ONE,
        ambient: Vec3::splat(0.1),
    };
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
    let mut seen = Vec::new();
    for (k, &(pick, _)) in picks.iter().enumerate() {
        let frame_number = k as u64 + 1;
        if let Some(result) = picker.poll(frame_number) {
            seen.push((result.request, result.nearest(0)));
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
            let color = fg.create_texture("color", TextureDesc::new_2d(FORMAT, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(depth_format, SIZE, SIZE));
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
            assert_eq!(
                picker.add_pass(
                    &mut fg,
                    &bindless,
                    &geo,
                    &mats,
                    &passes,
                    (SIZE, SIZE),
                    pick,
                    frame_number,
                ),
                PickPass::Declared
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
    // The last frame's pick.
    let last = picker.poll(picks.len() as u64 + 1).unwrap();
    seen.push((last.request, last.nearest(0)));
    println!("picks: {seen:?}");
    let want: Vec<_> = picks.iter().map(|&(p, id)| (p, id)).collect();
    assert_eq!(seen, want);

    // SAFETY: the device is idle.
    unsafe {
        picker.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(pool, None);
    }
    drop(picker);
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
