//! Ambient occlusion (GTAO, ADR 0058), under the validation layer with synchronization
//! validation, against a geometry with a known answer: a floor meeting a wall much larger
//! than the effect radius. With a hard radius R (no falloff), a floor point at distance d
//! from the wall sees the fraction of its cosine-weighted hemisphere whose rays do not hit
//! the wall within R: 1/2 at the wall, 1 beyond R, integrated numerically below.
//!
//! - With many samples per direction GTAO finds the horizons and matches the reference
//!   at the effect radius (the requested radius × 1.457).
//! - The presets (3 samples per side) find lower horizons. XeGTAO tuned the multiplier
//!   and the falloff so that they match a ray-traced reference at the requested radius:
//!   they are checked against that one.
//!
//! Checked with and without two-phase occlusion culling (another order of passes) and
//! with a temporal state (noise rotating per frame).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::ao::{AO_FORMAT, AoQuality, AoSettings};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Occlusion, ShadowSettings,
    Temporal,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::post::HDR_FORMAT;
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
            app_name: String::from("engine_render ao test"),
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
/// The effect radius: `AoSettings::radius` (1) × the radius multiplier.
const EFFECT_RADIUS: f32 = 1.457;

/// The cosine-weighted visibility of a floor point at distance `d` from the wall, with
/// occluders counted within `radius`: a ray leaving towards the wall (`x < 0`) hits it at
/// distance `d / −x`.
fn hard_radius_visibility(d: f32, radius: f32) -> f32 {
    const N: usize = 400;
    let (mut hidden, mut total) = (0.0f64, 0.0f64);
    for i in 0..N {
        let theta = (i as f64 + 0.5) / N as f64 * core::f64::consts::FRAC_PI_2;
        for j in 0..N {
            let phi = (j as f64 + 0.5) / N as f64 * core::f64::consts::TAU;
            let weight = theta.cos() * theta.sin();
            let x = theta.sin() * phi.cos();
            total += weight;
            if x < 0.0 && f64::from(d) <= f64::from(radius) * -x {
                hidden += weight;
            }
        }
    }
    (1.0 - hidden / total) as f32
}
/// Floor points at these distances from the wall (world units; the effect radius is 1).
const DISTANCES: [f32; 7] = [0.1, 0.2, 0.4, 0.8, 1.2, 2.5, 4.0];

#[test]
fn a_wall_hides_half_of_the_floor_next_to_it() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let depth_format = vk::Format::D32_SFLOAT;
    let mut renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let settings = |quality| AoSettings {
        quality,
        radius: 1.0,
        falloff: 0.01,
        power: 1.0,
        ..AoSettings::default()
    };
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let white = materials.add(MaterialDesc::solid([0.8; 3], 0.0, 0.8));
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut occlusion = Occlusion::new(&device, &mut allocator, 64).unwrap();
    let mut temporal = Temporal::new(&device, &mut allocator, 64).unwrap();

    // The floor's top at y = 0, the wall's face at x = 0 (both 40 units across).
    let slab = |size: Vec3, center: Vec3| DrawItem {
        mesh,
        material: white,
        world_from_object: Mat4::from_scale_rotation_translation(size, Quat::IDENTITY, center),
    };
    let draws = [
        slab(Vec3::new(40.0, 0.2, 40.0), Vec3::new(0.0, -0.1, 0.0)),
        slab(Vec3::new(0.2, 40.0, 40.0), Vec3::new(-0.1, 0.0, 0.0)),
    ];
    let eye = Vec3::new(6.0, 5.0, 0.5);
    let camera = Camera {
        clip_from_world: vulkan_perspective(60f32.to_radians(), 1.0, 0.1, 100.0)
            * look_at(eye, Vec3::new(1.2, 0.0, 0.0), Vec3::Y),
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(0.3, -1.0, 0.2),
        color: Vec3::ZERO,
        ambient: Vec3::ONE,
    };
    // Where the floor points land on the screen.
    let pixel_of = |p: Vec3| {
        let c = camera.clip_from_world * p.extend(1.0);
        let ndc = c.truncate() / c.w;
        (
            ((ndc.x * 0.5 + 0.5) * SIZE as f32) as u32,
            ((ndc.y * 0.5 + 0.5) * SIZE as f32) as u32,
        )
    };

    let bytes = u64::from(SIZE * SIZE);
    let (readback, readback_mem) = allocator
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

    let reference: Vec<f32> = DISTANCES
        .iter()
        .map(|&d| hard_radius_visibility(d, EFFECT_RADIUS))
        .collect();
    let requested: Vec<f32> = DISTANCES
        .iter()
        .map(|&d| hard_radius_visibility(d, 1.0))
        .collect();
    println!("reference {DISTANCES:?} -> {reference:?}, at the requested radius {requested:?}");
    let many = AoQuality::Custom {
        slices: 9,
        steps: 32,
    };
    let mut frame_number = 0;
    // (quality, occlusion culling, temporal state)
    for (quality, occluded, temporal_state) in [
        (many, false, false),
        (many, true, false),
        (many, false, true),
        (AoQuality::Low, false, false),
        (AoQuality::Medium, false, true),
        (AoQuality::High, false, false),
        (AoQuality::Ultra, false, true),
    ] {
        renderer.set_ambient_occlusion(Some(settings(quality)));
        // Two frames: occlusion culling's second frame has the first's visibility.
        for _ in 0..2 {
            frame_number += 1;
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
                let color = fg.create_texture("color", TextureDesc::new_2d(HDR_FORMAT, SIZE, SIZE));
                let depth =
                    fg.create_texture("depth", TextureDesc::new_2d(depth_format, SIZE, SIZE));
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
                            clear: Some([0.0, 0.0, 0.0, 1.0]),
                            occlusion: occluded.then_some(&occlusion),
                            temporal: temporal_state.then_some(&temporal),
                            lights: &[],
                            environment: None,
                        },
                    )
                    .unwrap();
                let ao = passes.ao.unwrap();
                assert_eq!(fg.texture_desc(ao).format, AO_FORMAT);
                fg.add_pass("read back")
                    .read(ao, Access::TransferRead)
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
                        // SAFETY: recording outside a rendering scope; layouts arranged by
                        // the graph.
                        unsafe {
                            ctx.device.cmd_copy_image_to_buffer(
                                ctx.cmd,
                                ctx.image(ao),
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
        }
        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let image = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), bytes as usize) };
        let at = |p: Vec3| {
            let (x, y) = pixel_of(p);
            f32::from(image[(y * SIZE + x) as usize]) / 255.0
        };
        // The mean of a row of pixels at the same distance from the wall: the noise of
        // the few-slice presets averages out, as TAA averages it over frames.
        let values: Vec<f32> = DISTANCES
            .iter()
            .map(|&d| {
                let row = [-0.6, -0.4, -0.2, 0.0, 0.2, 0.4, 0.6];
                row.iter().map(|&z| at(Vec3::new(d, 0.0, z))).sum::<f32>() / row.len() as f32
            })
            .collect();
        println!(
            "{quality:?}, occlusion {occluded}, temporal {temporal_state}: {DISTANCES:?} -> {values:?}"
        );
        for (k, &v) in values.iter().enumerate() {
            if quality == many {
                let r = reference[k];
                // The exact horizons; next to the wall the normal comes partly from the
                // wall's pixels (normals from depth).
                let tolerance = if k == 0 { 0.08 } else { 0.04 };
                assert!(
                    (v - r).abs() <= tolerance,
                    "d = {}: {v} vs {r}",
                    DISTANCES[k]
                );
            } else {
                // Presets: the reference at the requested radius; next to the wall the
                // blur also takes in the corner's pixels, whose normal is half the wall's.
                // Two samples per side (Low, Medium) are coarser than three.
                let r = requested[k];
                let coarse = matches!(quality, AoQuality::Low | AoQuality::Medium);
                let tolerance = if coarse { 0.08 } else { 0.05 };
                let (below, above) = if k == 0 {
                    (0.03, 0.12)
                } else {
                    (tolerance, tolerance)
                };
                assert!(
                    v >= r - below && v <= r + above,
                    "d = {}: {v} vs {r}",
                    DISTANCES[k]
                );
            }
        }
        // Rising away from the wall (within noise).
        assert!(values.windows(2).all(|w| w[1] >= w[0] - 0.03), "{values:?}");
        // Open floor across the view, both sides of the camera's axis.
        for z in [-2.0, 2.0] {
            let open = at(Vec3::new(3.5, 0.0, z));
            assert!(open > 0.97, "open floor at z = {z}: {open}");
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
        temporal.destroy(&mut allocator);
        occlusion.destroy(&mut allocator);
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
