//! Casters drawn into the sun's virtual shadow pages (ADR 0093 step 3b) by the forward
//! renderer, against the depths a ray along the light finds.
//!
//! A ground slab and two boxes under a directional light, seen from a camera that stands,
//! stands still and steps aside; then the plate in the air moves. After each frame, every
//! texel of every backed page must
//! hold the depth (along the light, across the pages' range) where the light's ray
//! through the texel's center leaves the first box it meets: the shadow pass draws the
//! faces turned away from the light. And the sun's visibility the view's shading reads
//! (its mask) must be what a ray to the sun finds from each pixel's surface, but in the
//! filter's penumbra. Once with the boxes drawn whole, once by meshlet, under
//! synchronization validation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use engine_assets::meshlet::build_meshlets;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, ViewFeatures,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_render::vsm::{
    ALLOCATED, PAGE_TEXELS, PHYSICAL, TABLE_PAGES, VirtualShadows, VsmSettings, depth_range,
};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const LIGHT: Vec3 = Vec3::new(-0.4, -1.0, -0.3);
/// Physical pages: room for the view's.
const POOL_PAGES: u32 = 1024;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render vsm draw test"),
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

/// An axis-aligned box: center and size.
#[derive(Clone, Copy)]
struct Box3 {
    center: Vec3,
    size: Vec3,
}

const BOXES: [Box3; 3] = [
    // The ground, its top at y = 0.
    Box3 {
        center: Vec3::new(0.0, -0.1, -10.0),
        size: Vec3::new(40.0, 0.2, 40.0),
    },
    // A plate in the air, and a box on the ground.
    Box3 {
        center: Vec3::new(0.0, 3.0, -5.0),
        size: Vec3::new(3.0, 0.5, 3.0),
    },
    Box3 {
        center: Vec3::new(3.0, 1.0, -3.0),
        size: Vec3::new(1.0, 2.0, 1.0),
    },
];

/// [`BOXES`] with the plate in the air moved by `plate`.
fn boxes_with(plate: Vec3) -> [Box3; 3] {
    let mut boxes = BOXES;
    boxes[1].center += plate;
    boxes
}

/// The pages' depth range over `boxes`: their extent along the light, as the renderer
/// finds it.
fn depth_range_of(boxes: &[Box3]) -> [f64; 2] {
    let direction = LIGHT.normalize();
    let along = boxes.iter().flat_map(|b| {
        (0..8).map(move |i: u32| {
            let corner = b.center
                + b.size
                    * 0.5
                    * Vec3::new(
                        if i & 1 == 0 { -1.0 } else { 1.0 },
                        if i & 2 == 0 { -1.0 } else { 1.0 },
                        if i & 4 == 0 { -1.0 } else { 1.0 },
                    );
            corner.dot(direction)
        })
    });
    let extent = along.fold([f32::MAX, f32::MIN], |[lo, hi], d| [lo.min(d), hi.max(d)]);
    depth_range(extent).map(f64::from)
}

/// Where the ray `origin + t · direction` enters `b` and the normal of the face it enters
/// by, if it meets it in front of the origin.
fn enter(b: &Box3, origin: [f64; 3], direction: [f64; 3]) -> Option<(f64, [f64; 3])> {
    let (mut near, mut far, mut axis) = (f64::MIN, f64::MAX, 0);
    for i in 0..3 {
        let c = f64::from(b.center.to_array()[i]);
        let h = f64::from(b.size.to_array()[i]) * 0.5;
        let t1 = (c - h - origin[i]) / direction[i];
        let t2 = (c + h - origin[i]) / direction[i];
        if t1.min(t2) > near {
            near = t1.min(t2);
            axis = i;
        }
        far = far.min(t1.max(t2));
    }
    (near <= far && near > 0.0).then(|| {
        let mut normal = [0.0; 3];
        normal[axis] = -direction[axis].signum();
        (near, normal)
    })
}

/// Where the ray `origin + t · direction` leaves `b`, if it meets it.
fn exit(b: &Box3, origin: [f64; 3], direction: [f64; 3]) -> Option<f64> {
    let (mut near, mut far) = (f64::MIN, f64::MAX);
    for i in 0..3 {
        let c = f64::from(b.center.to_array()[i]);
        let h = f64::from(b.size.to_array()[i]) * 0.5;
        let t1 = (c - h - origin[i]) / direction[i];
        let t2 = (c + h - origin[i]) / direction[i];
        near = near.max(t1.min(t2));
        far = far.min(t1.max(t2));
    }
    (near <= far).then_some(far)
}

#[test]
fn casters_drawn_whole_fill_the_pages_with_their_depths() {
    run(false);
}

#[test]
fn casters_drawn_by_meshlet_fill_the_pages_with_their_depths() {
    run(true);
}

#[allow(clippy::too_many_lines)]
fn run(meshlets: bool) {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = if meshlets {
        let data = cube();
        let mut indices = data.indices.clone();
        let cube_meshlets = build_meshlets(&data.positions, &mut indices);
        geometry
            .add_mesh(&MeshData {
                indices: &indices,
                meshlets: bytemuck::cast_slice(&cube_meshlets),
                ..data.as_data()
            })
            .unwrap()
    } else {
        geometry.add_mesh(&cube().as_data()).unwrap()
    };
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let gray = materials.add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 1.0));
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 8 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut renderer =
        ForwardRenderer::new(&device, &bindless, HDR, DEPTH, ShadowSettings::default()).unwrap();
    if !renderer
        .set_virtual_shadows(&device, &bindless, true)
        .unwrap()
    {
        println!(
            "skipping: no fragmentStoresAndAtomics, drawIndirectCount, shaderInt64 or \
             shaderClipDistance"
        );
        return;
    }
    let settings = VsmSettings {
        pool_pages: POOL_PAGES,
        ..VsmSettings::default()
    };
    let vsm = VirtualShadows::new(&device, &mut allocator, &bindless, settings).unwrap();
    let light = DirectionalLight {
        direction: LIGHT,
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.1),
    };
    let direction = LIGHT.normalize();
    let world_from_light = VirtualShadows::light_from_world(LIGHT).inverse();

    let rows = POOL_PAGES.div_ceil(TABLE_PAGES);
    let pool_extent = [TABLE_PAGES * PAGE_TEXELS, rows * PAGE_TEXELS];
    let pool_bytes = u64::from(pool_extent[0]) * u64::from(pool_extent[1]) * 4;
    let table_bytes = u64::from(settings.levels * TABLE_PAGES * TABLE_PAGES) * 4;
    let mut readback = |bytes: u64| {
        allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap()
    };
    let (pool_out, pool_memory) = readback(pool_bytes);
    let (table_out, table_memory) = readback(table_bytes);
    let mask_bytes = u64::from(WIDTH * HEIGHT) * 4;
    let (mask_out, mask_memory) = readback(mask_bytes);
    let d = device.raw();
    // SAFETY: plain object creation.
    let (command_pool, cmd) = unsafe {
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

    // Standing, standing still, a step aside; then the plate moves under a still camera
    // (the pages it draws were all drawn before: only the scene's change redraws them),
    // within the scene's depth range.
    let frames = [
        (Vec3::new(0.0, 1.7, 4.0), Vec3::ZERO),
        (Vec3::new(0.0, 1.7, 4.0), Vec3::ZERO),
        (Vec3::new(1.0, 1.7, 3.5), Vec3::ZERO),
        (Vec3::new(1.0, 1.7, 3.5), Vec3::new(-1.5, 0.0, 0.5)),
    ];
    for (frame, &(eye, plate)) in frames.iter().enumerate() {
        let boxes = boxes_with(plate);
        let draws: Vec<DrawItem> = boxes
            .iter()
            .map(|b| DrawItem {
                mesh,
                material: gray,
                world_from_object: Mat4::from_scale_rotation_translation(
                    b.size,
                    Quat::IDENTITY,
                    b.center,
                ),
            })
            .collect();
        let [lo, hi] = depth_range_of(&boxes);
        let clip_from_world = vulkan_perspective(1.0, WIDTH as f32 / HEIGHT as f32, 0.1, 100.0)
            * look_at(eye, eye + Vec3::new(0.0, -0.3, -1.0), Vec3::Y);
        let camera = Camera {
            clip_from_world,
            position: eye,
        };
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
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, 1)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, 1)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, 1)
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(HDR, WIDTH, HEIGHT));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, WIDTH, HEIGHT));
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
                        virtual_shadows: Some(&vsm),
                        lights: &[],
                        decals: &[],
                        environment: None,
                        reflections: Default::default(),
                        gi: None,
                        features: ViewFeatures::NONE,
                    },
                )
                .unwrap();
            let pages = passes.virtual_shadows.expect("the view drew its pages");
            let mask = passes
                .contact_shadows
                .expect("the view's shading read the sun's mask");
            let mask_target = fg.import_buffer(
                "mask read back",
                ImportedBuffer {
                    buffer: mask_out,
                    size: mask_bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            let pool_target = fg.import_buffer(
                "pool read back",
                ImportedBuffer {
                    buffer: pool_out,
                    size: pool_bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            let table_target = fg.import_buffer(
                "table read back",
                ImportedBuffer {
                    buffer: table_out,
                    size: table_bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            fg.add_pass("vsm read back")
                .read(pages.pool, Access::TransferRead)
                .read(pages.table, Access::TransferRead)
                .read(mask, Access::TransferRead)
                .write(pool_target, Access::TransferWrite)
                .write(table_target, Access::TransferWrite)
                .write(mask_target, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording; the graph orders the copies after the pages' passes.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(mask),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(mask_target),
                            &[vk::BufferImageCopy {
                                image_subresource: vk::ImageSubresourceLayers {
                                    aspect_mask: vk::ImageAspectFlags::COLOR,
                                    mip_level: 0,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                },
                                image_extent: vk::Extent3D {
                                    width: WIDTH,
                                    height: HEIGHT,
                                    depth: 1,
                                },
                                ..Default::default()
                            }],
                        );
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(pages.pool),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(pool_target),
                            &[vk::BufferImageCopy {
                                image_subresource: vk::ImageSubresourceLayers {
                                    aspect_mask: vk::ImageAspectFlags::COLOR,
                                    mip_level: 0,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                },
                                image_extent: vk::Extent3D {
                                    width: pool_extent[0],
                                    height: pool_extent[1],
                                    depth: 1,
                                },
                                ..Default::default()
                            }],
                        );
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(pages.table),
                            ctx.buffer(table_target),
                            &[vk::BufferCopy {
                                src_offset: 0,
                                dst_offset: 0,
                                size: table_bytes,
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
        let words = |memory: &engine_rhi::Allocation, bytes: u64| -> Vec<u32> {
            let ptr = memory.mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping covers the buffer.
            unsafe { core::slice::from_raw_parts(ptr.as_ptr().cast::<u32>(), (bytes / 4) as usize) }
                .to_vec()
        };
        let pool = words(&pool_memory, pool_bytes);
        let table = words(&table_memory, table_bytes);
        let mask = words(&mask_memory, mask_bytes);

        // Every texel of every backed page against the ray through its center.
        let (mut texels, mut covered, mut wrong, mut worst) = (0u64, 0u64, 0u64, 0.0f64);
        let page = PAGE_TEXELS as usize;
        let side = TABLE_PAGES as i64;
        let width = pool_extent[0] as usize;
        let mut backed = 0;
        for (e, &entry) in table.iter().enumerate() {
            if entry & ALLOCATED == 0 {
                continue;
            }
            backed += 1;
            let k = e / (TABLE_PAGES * TABLE_PAGES) as usize;
            let level = vsm.level(k as u32).unwrap();
            let local = (e % (TABLE_PAGES * TABLE_PAGES) as usize) as i64;
            let at = [local % side, local / side];
            let first = [i64::from(level.first_page.x), i64::from(level.first_page.y)];
            // The window's page at this place of the table.
            let absolute = [0, 1].map(|i| first[i] + (at[i] - first[i]).rem_euclid(side));
            let physical = (entry & PHYSICAL) as usize;
            let origin = (physical / 64) * page * width + (physical % 64) * page;
            let texel = f64::from(level.texel);
            for ty in 0..page {
                for tx in 0..page {
                    let virtual_texel = [
                        absolute[0] as f64 * page as f64 + tx as f64 + 0.5,
                        absolute[1] as f64 * page as f64 + ty as f64 + 0.5,
                    ];
                    let across = world_from_light.transform_point3(Vec3::new(
                        (virtual_texel[0] * texel) as f32,
                        (virtual_texel[1] * texel) as f32,
                        0.0,
                    ));
                    let origin_world = across.to_array().map(f64::from);
                    let ray = direction.to_array().map(f64::from);
                    let nearest = boxes
                        .iter()
                        .filter_map(|b| exit(b, origin_world, ray))
                        .fold(f64::MAX, f64::min);
                    let expected = if nearest == f64::MAX {
                        1.0
                    } else {
                        ((nearest - lo) / (hi - lo)).clamp(0.0, 1.0)
                    };
                    let stored = f64::from(f32::from_bits(pool[origin + ty * width + tx]));
                    texels += 1;
                    if expected < 1.0 {
                        covered += 1;
                    }
                    let error = (stored - expected).abs();
                    if error > 1e-4 {
                        wrong += 1;
                        worst = worst.max(error);
                    }
                }
            }
        }
        println!(
            "frame {frame}: {backed} pages backed, {texels} texels, {covered} under a caster, \
             {wrong} wrong (worst by {worst:.4})"
        );
        assert!(backed > 0, "frame {frame}: no page backed");
        assert!(
            covered > texels / 10,
            "frame {frame}: the casters cover the pages"
        );
        // The sun's visibility per pixel: the surface a ray from the camera through the
        // pixel's center meets, then a ray from it to the sun.
        let world_from_clip = clip_from_world.inverse();
        let to_sun = direction.to_array().map(|c| -f64::from(c));
        let eye_at = eye.to_array().map(f64::from);
        let (mut compared, mut penumbra, mut differ, mut in_shadow) = (0u32, 0u32, 0u32, 0u32);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let ndc = [
                    (x as f32 + 0.5) / WIDTH as f32 * 2.0 - 1.0,
                    (y as f32 + 0.5) / HEIGHT as f32 * 2.0 - 1.0,
                ];
                let far = world_from_clip * engine_core::math::Vec4::new(ndc[0], ndc[1], 1.0, 1.0);
                let ray = (far.truncate() / far.w - eye)
                    .normalize()
                    .to_array()
                    .map(f64::from);
                let hit = boxes
                    .iter()
                    .filter_map(|b| enter(b, eye_at, ray))
                    .min_by(|a, b| a.0.total_cmp(&b.0));
                let Some((t, normal)) = hit else {
                    continue;
                };
                // Faces turned from the sun are dark whatever their shadow says.
                let facing = (0..3).map(|i| normal[i] * to_sun[i]).sum::<f64>();
                if facing < 0.1 {
                    continue;
                }
                let point = [0, 1, 2].map(|i| eye_at[i] + t * ray[i] + normal[i] * 1e-3);
                let shadowed = boxes.iter().any(|b| enter(b, point, to_sun).is_some());
                let visible = f64::from(f32::from_bits(mask[(y * WIDTH + x) as usize]));
                if visible > 0.05 && visible < 0.95 {
                    penumbra += 1;
                    continue;
                }
                compared += 1;
                in_shadow += u32::from(shadowed);
                if (visible < 0.5) != shadowed {
                    differ += 1;
                }
            }
        }
        println!(
            "         {compared} pixels compared ({in_shadow} in shadow), {differ} differ, \
             {penumbra} in the filter's penumbra"
        );
        assert!(in_shadow > 500, "frame {frame}: the view shows shadows");
        assert!(
            f64::from(differ) < f64::from(compared) * 0.01,
            "frame {frame}: {differ} of {compared} pixels see the sun otherwise than a ray"
        );
        // Rasterization and the ray part ways only on the casters' silhouettes.
        assert!(
            (wrong as f64) < texels as f64 * 0.002,
            "frame {frame}: {wrong} of {texels} texels hold another depth (worst by {worst})"
        );
    }

    // SAFETY: the device is idle.
    unsafe {
        let mut vsm = vsm;
        vsm.destroy(&mut allocator, &bindless);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(pool_out, pool_memory);
        allocator.destroy_buffer(table_out, table_memory);
        allocator.destroy_buffer(mask_out, mask_memory);
        d.destroy_command_pool(command_pool, None);
    }
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
