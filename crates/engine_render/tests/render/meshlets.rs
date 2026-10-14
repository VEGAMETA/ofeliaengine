//! Drawing by meshlet (ADR 0085) under the validation layer with synchronization
//! validation.
//!
//! Spheres with meshlets (`engine_assets::meshlet`) in and around the view, rotated and
//! uniformly scaled, one stretched (no cone test), one mirrored and one in centimetres at
//! 0.008, and cubes (one meshlet whose normals spread too far for a cone), are drawn with
//! and without meshlet culling. The images must be the same (culling removes only
//! meshlets that cannot be seen), and the meshlet lists must have drawn fewer meshlets
//! than the meshes have (frustum and normal cones cull some).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_assets::meshlet::build_meshlets;
use engine_core::math::{EulerRot, Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Occlusion, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const SIZE: u32 = 256;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render meshlet test"),
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

/// A unit sphere of `rings` × `segments` quads, counter-clockwise from outside.
pub(crate) fn sphere(rings: u32, segments: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
    let mut positions = Vec::new();
    for r in 0..=rings {
        let theta = core::f32::consts::PI * r as f32 / rings as f32;
        for s in 0..=segments {
            let phi = core::f32::consts::TAU * s as f32 / segments as f32;
            positions.push([
                theta.sin() * phi.cos(),
                theta.cos(),
                theta.sin() * phi.sin(),
            ]);
        }
    }
    let row = segments + 1;
    let mut indices = Vec::new();
    for r in 0..rings {
        for s in 0..segments {
            let a = r * row + s;
            for t in [[a, a + 1, a + row], [a + 1, a + row + 1, a + row]] {
                // Outward: the face normal points away from the center.
                let [p, q, w] = t.map(|i| Vec3::from(positions[i as usize]));
                let n = (q - p).cross(w - p);
                if n.length_squared() < 1e-12 {
                    continue;
                }
                if n.dot(p + q + w) >= 0.0 {
                    indices.extend(t);
                } else {
                    indices.extend([t[0], t[2], t[1]]);
                }
            }
        }
    }
    (positions, indices)
}

#[test]
fn meshlet_culling_keeps_the_image_and_draws_less() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let color_format = vk::Format::R8G8B8A8_UNORM;
    let depth_format = vk::Format::D32_SFLOAT;
    let mut renderer = ForwardRenderer::new(
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
    if !renderer
        .set_meshlet_culling(&device, &bindless, true)
        .unwrap()
    {
        println!("skipping: no drawIndirectCount or shaderInt64");
        return;
    }

    let (positions, mut indices) = sphere(48, 96);
    let meshlets = build_meshlets(&positions, &mut indices);
    assert!(meshlets.len() > 40, "{} meshlets", meshlets.len());
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry
        .add_mesh(&MeshData {
            positions: &positions,
            normals: &positions,
            tangents: &[],
            uvs: &[],
            indices: &indices,
            meshlets: bytemuck::cast_slice(&meshlets),
        })
        .unwrap();
    // The sphere in centimetres, drawn at 0.008 (Sponza's units and scale): large
    // object-space bounds under a small instance scale.
    let big: Vec<[f32; 3]> = positions.iter().map(|p| p.map(|c| c * 125.0)).collect();
    let mut big_indices = indices.clone();
    let big_meshlets = build_meshlets(&big, &mut big_indices);
    let big_mesh = geometry
        .add_mesh(&MeshData {
            positions: &big,
            normals: &positions,
            tangents: &[],
            uvs: &[],
            indices: &big_indices,
            meshlets: bytemuck::cast_slice(&big_meshlets),
        })
        .unwrap();
    // A cube: one meshlet, its normals over six directions, so no cone (meshoptimizer's
    // axis 0 and cutoff 127).
    let cube_data = cube();
    let mut cube_indices = cube_data.indices.clone();
    let cube_meshlets = build_meshlets(&cube_data.positions, &mut cube_indices);
    assert_eq!(cube_meshlets.len(), 1);
    assert_eq!(
        cube_meshlets[0].cone,
        [0, 0, 0, 127],
        "the cube has no cone"
    );
    let cube_mesh = geometry
        .add_mesh(&MeshData {
            indices: &cube_indices,
            meshlets: bytemuck::cast_slice(&cube_meshlets),
            ..cube_data.as_data()
        })
        .unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);

    let eye = Vec3::new(0.0, 0.0, 10.0);
    let clip_from_world =
        vulkan_perspective(60f32.to_radians(), 1.0, 0.5, 60.0) * look_at(eye, Vec3::ZERO, Vec3::Y);
    let camera = Camera {
        clip_from_world,
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.5),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.1),
    };
    // Spheres: near the center, at the edges of the view, rotated; one stretched (its
    // cones are not tested) and one mirrored (its cone axes flip with it).
    let placements = [
        Mat4::from_translation(Vec3::new(0.0, 0.0, 0.0)),
        Mat4::from_scale_rotation_translation(
            Vec3::splat(1.5),
            Quat::from_euler(EulerRot::XYZ, 0.7, 1.9, 0.3),
            Vec3::new(-4.5, 2.0, -3.0),
        ),
        Mat4::from_scale_rotation_translation(
            Vec3::splat(2.0),
            Quat::from_euler(EulerRot::XYZ, 2.1, 0.4, 1.2),
            Vec3::new(6.0, -1.0, -2.0),
        ),
        Mat4::from_scale_rotation_translation(
            Vec3::new(0.6, 2.2, 0.8),
            Quat::from_euler(EulerRot::XYZ, 0.2, 0.0, 0.9),
            Vec3::new(2.5, 3.5, 1.0),
        ),
        Mat4::from_scale_rotation_translation(
            Vec3::new(-1.0, 1.0, 1.0),
            Quat::from_euler(EulerRot::XYZ, 1.0, 2.0, 0.5),
            Vec3::new(-2.0, -3.0, 2.0),
        ),
    ];
    let mut draws: Vec<DrawItem> = placements
        .iter()
        .enumerate()
        .map(|(k, &world_from_object)| DrawItem {
            mesh,
            material: materials.add(MaterialDesc::solid(
                [0.2 + 0.15 * k as f32, 0.8 - 0.1 * k as f32, 0.5],
                0.0,
                0.6,
            )),
            world_from_object,
        })
        .collect();
    for (k, at) in [Vec3::new(-1.5, 1.5, 5.0), Vec3::new(3.0, -3.5, 3.0)]
        .into_iter()
        .enumerate()
    {
        draws.push(DrawItem {
            mesh: cube_mesh,
            material: materials.add(MaterialDesc::solid([0.9, 0.3, 0.2 * k as f32], 0.0, 0.5)),
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::splat(1.2),
                Quat::from_euler(EulerRot::XYZ, 0.5, 0.6 * k as f32, 0.2),
                at,
            ),
        });
    }
    draws.push(DrawItem {
        mesh: big_mesh,
        material: materials.add(MaterialDesc::solid([0.9, 0.9, 0.2], 0.0, 0.5)),
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::splat(0.008),
            Quat::from_euler(EulerRot::XYZ, 0.3, 0.8, 0.0),
            Vec3::new(1.5, -2.0, 4.0),
        ),
    });

    let image_bytes = u64::from(SIZE * SIZE * 4);
    // The image, then each phase's meshlet draw counts (32 words each).
    let readback_size = image_bytes + 256;
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

    let mut images = Vec::new();
    let mut drawn = Vec::new();
    // Occlusion culling's two phases, with and without meshlets: each its own state.
    let occlusions = [
        Occlusion::new(&device, &mut allocator, 64).unwrap(),
        Occlusion::new(&device, &mut allocator, 64).unwrap(),
    ];
    // Meshlets on and off with the depth prepass, then on without it: not drawn by meshlet
    // then (their order would undo the front-to-back order). Then three frames each with
    // occlusion culling (the first all in the late phase, the others in both), with and
    // without meshlets.
    let mut configs = vec![(true, true, None), (false, true, None), (true, false, None)];
    configs.extend([(true, true, Some(0)); 3]);
    configs.extend([(false, true, Some(1)); 3]);
    for (frame, (meshlets_on, prepass, occluded)) in configs.into_iter().enumerate() {
        let occlusion: Option<&Occlusion> = occluded.map(|k: usize| &occlusions[k]);
        let phases = 1 + u64::from(occlusion.is_some());
        renderer
            .set_meshlet_culling(&device, &bindless, meshlets_on)
            .unwrap();
        renderer.set_depth_prepass(prepass);
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
                        clear: Some([0.0, 0.0, 0.25, 1.0]),
                        occlusion,
                        temporal: None,
                        shadow_cache: None,
                        lights: &[],
                        decals: &[],
                        environment: None,
                        reflections: Default::default(),
                        gi: None,
                    },
                )
                .unwrap();
            assert_eq!(passes.meshlets.is_some(), meshlets_on && prepass);
            let commands = passes.commands.unwrap();
            let lists = passes.meshlets;
            assert_eq!(1 + u64::from(passes.occlusion), phases);
            fg.add_pass("read back")
                .read(color, Access::TransferRead)
                .read(commands, Access::TransferRead)
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
                        if let Some(lists) = lists {
                            // Each phase's draw counts, per state.
                            let counts = |phase: u64| vk::BufferCopy {
                                src_offset: lists.offset + phase * lists.bytes,
                                dst_offset: image_bytes + phase * 128,
                                size: 128,
                            };
                            let regions = [counts(0), counts(1)];
                            ctx.device.cmd_copy_buffer(
                                ctx.cmd,
                                ctx.buffer(commands),
                                ctx.buffer(out),
                                &regions[..phases as usize],
                            );
                        }
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
        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
        if meshlets_on && prepass {
            let counts: Vec<u32> = bytes
                [image_bytes as usize..(image_bytes + 128 * phases) as usize]
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            drawn.push(counts.iter().sum::<u32>());
        }
        images.push(bytes[..image_bytes as usize].to_vec());
    }
    let total = meshlets.len() as u32 * placements.len() as u32
        + big_meshlets.len() as u32
        + 2 * cube_meshlets.len() as u32;
    println!("meshlets drawn {drawn:?} of {total}");
    for &n in &drawn {
        assert!(n > 0 && n < total * 3 / 4, "{n} of {total} meshlets drawn");
    }
    let background = images[1]
        .chunks_exact(4)
        .filter(|p| *p == [0, 0, 64, 255])
        .count();
    let covered = (SIZE * SIZE) as usize - background;
    println!("{covered} of {} pixels covered", SIZE * SIZE);
    assert!(covered > 4000, "the spheres should cover the image");
    // The frames without occlusion culling, and the last of each with it.
    for (k, image) in [
        (0, &images[0]),
        (2, &images[2]),
        (5, &images[5]),
        (8, &images[8]),
    ] {
        let differing = image
            .chunks_exact(4)
            .zip(images[1].chunks_exact(4))
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(
            differing, 0,
            "frame {k}: meshlets changed {differing} pixels"
        );
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        for mut occlusion in occlusions {
            occlusion.destroy(&mut allocator);
        }
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(readback, readback_mem);
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
