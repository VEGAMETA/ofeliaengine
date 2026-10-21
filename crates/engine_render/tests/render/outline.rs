//! Outlines (ADR 0109), under the validation layer with synchronization validation: a line
//! of each class's color and width around chosen instances of the GPU scene, dimmed where
//! another surface hides the outlined one.
//!
//! Dark quads face the camera: A (the active instance), a smaller B in front of A's right
//! edge, and C (hovered) to the right. Just outside A's left edge the line is the active
//! color; inside A nothing is drawn; behind B, A's right edge keeps 35 % of its alpha; C's
//! line is the hovered one's, thin and light; the background far away is untouched.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, ForwardRenderer, ForwardView, ShadowSettings, ViewFeatures,
};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData};
use engine_render::outline::{OutlineClass, OutlineItem, OutlineStyle, OutlineView, Outliner};
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
            app_name: String::from("engine_render outline test"),
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
const NEAR: f32 = 0.1;
const FAR: f32 = 100.0;

#[test]
fn outlines_follow_the_chosen_instances_and_dim_where_hidden() {
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
    let mut outliner = Outliner::new(&device, &bindless, FORMAT, 1.0).unwrap();
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
    let dark = materials.add(MaterialDesc::solid([0.0; 3], 0.0, 1.0));
    let mut scene = GpuScene::new(&device, &bindless, 16, 1).unwrap();
    let quad = |at: Vec3, scale: f32| SceneInstance {
        mesh,
        material: dark,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::splat(scale),
            Quat::IDENTITY,
            at,
        ),
    };
    let a = scene.insert(quad(Vec3::new(-0.6, 0.0, -4.0), 1.0));
    let _b = scene.insert(quad(Vec3::new(-0.05, 0.0, -3.0), 0.5));
    let c = scene.insert(quad(Vec3::new(1.2, 0.0, -4.0), 1.0));
    let items = [
        OutlineItem {
            slot: a.slot(),
            class: OutlineClass::Active,
        },
        OutlineItem {
            slot: c.slot(),
            class: OutlineClass::Hovered,
        },
    ];
    let fov = 60f32.to_radians();
    let clip_from_world = vulkan_perspective(fov, 1.0, NEAR, FAR)
        * look_at(Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0), Vec3::Y);
    let camera = Camera {
        clip_from_world,
        position: Vec3::ZERO,
    };
    let light = DirectionalLight {
        direction: Vec3::new(0.0, -1.0, 0.0),
        color: Vec3::ZERO,
        ambient: Vec3::ZERO,
    };
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let image_bytes = u64::from(SIZE * SIZE) * 8;
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(image_bytes)
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
    // Two frames: the scene's instances arrive in the first.
    for frame_number in 1..=2u64 {
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
            assert!(outliner.add_passes(
                &mut fg,
                &mut ring,
                &bindless,
                &geo,
                &mats,
                &frame,
                &items,
                &OutlineView {
                    target: color,
                    rect: vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent: vk::Extent2D {
                            width: SIZE,
                            height: SIZE,
                        },
                    },
                    clip_from_world,
                    near: NEAR,
                    far: FAR,
                    scene_depth: depth,
                    render: (SIZE, SIZE),
                },
                &OutlineStyle::default(),
            ));
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: readback,
                    size: image_bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            fg.add_pass("read back")
                .read(color, Access::TransferRead)
                .write(out, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; the image is in
                    // TRANSFER_SRC layout for this pass.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(color),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[vk::BufferImageCopy {
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
    }
    let ptr = readback_mem.mapped_ptr().unwrap();
    // SAFETY: the GPU is idle; the mapping covers the image.
    let pixels = unsafe {
        core::slice::from_raw_parts(ptr.as_ptr().cast::<u16>(), (SIZE * SIZE * 4) as usize)
    };
    let at = |x: u32, y: u32| {
        let i = ((y * SIZE + x) * 4) as usize;
        [0, 1, 2].map(|k| half_to_f32(pixels[i + k]))
    };
    // A point's pixel column (y = 0: the view's middle row).
    let pixels_per_unit = SIZE as f32 * 0.5 / (fov * 0.5).tan();
    let column = |x: f32, z: f32| (x / -z * pixels_per_unit + SIZE as f32 * 0.5) as u32;
    let middle = SIZE / 2;
    // The brightest of the pixels from `from` to `to` on the middle row: the line's pixel
    // a pixel away from the edge, wherever the edge falls between pixels.
    let brightest = |from: u32, to: u32| {
        (from..=to)
            .map(|x| at(x, middle))
            .max_by(|p, q| p[0].total_cmp(&q[0]))
            .unwrap()
    };
    let style = OutlineStyle::default();
    // Just outside A's left edge: the active color, whole.
    let edge = column(-1.1, -4.0);
    let left = brightest(edge - 3, edge);
    let active = style.colors[1];
    println!("left of A {left:?}");
    assert!(
        (left[0] - active[0]).abs() < 0.08 && (left[1] - active[1]).abs() < 0.08,
        "{left:?}"
    );
    // Inside A: nothing (the quads are black).
    let inside = at(column(-0.85, -4.0), middle);
    assert!(inside.iter().all(|&v| v < 0.02), "{inside:?}");
    // Right of A's right edge, behind B: 35 % of the color, over B's black.
    let edge = column(-0.1, -4.0);
    let hidden = brightest(edge, edge + 3);
    println!("hidden right of A {hidden:?}");
    assert!(
        (hidden[0] - active[0] * style.hidden).abs() < 0.08,
        "{hidden:?}"
    );
    // Left of C's left edge: the hovered one's light line (1 pixel, translucent).
    let edge = column(0.7, -4.0);
    let hovered = brightest(edge - 3, edge);
    println!("left of C {hovered:?}");
    assert!(
        hovered[0] > 0.1 && hovered[0] < 0.6 && (hovered[0] - hovered[1]).abs() < 0.02,
        "{hovered:?}"
    );
    // The background far from all of them.
    assert!(at(4, 4).iter().all(|&v| v < 0.02));

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        scene.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
    drop(outliner);
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

/// An IEEE half's value.
fn half_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((bits >> 10) & 0x1F);
    let mantissa = f32::from(bits & 0x3FF);
    sign * match exponent {
        0 => mantissa / 1024.0 * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + mantissa / 1024.0) * 2f32.powi(e - 15),
    }
}
