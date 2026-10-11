//! The scene for ray queries (ADR 0087): cubes in a GPU scene (one scaled, one mirrored,
//! one removed, one transparent), its acceleration structures built, rays cast against
//! them on the GPU and their hits compared with the cubes' boxes: distances, slots, faces,
//! instance masks, the removed cube gone; over four frames, the bottom level compacted, a
//! cube moved, the top level kept once the scene is still. Under the validation layer with
//! synchronization validation.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{BufferState, ImportedBuffer, RenderGraph};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::rays::{RAY_MASK_ALL, RAY_MASK_OPAQUE, RayCast, RayHit, RayScene, RaySettings};
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
            app_name: String::from("engine_render rays test"),
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

/// A ray's expected nearest hit: distance, slot, front face.
type Expected = (f32, u32, u32);

fn ray(origin: Vec3, direction: Vec3) -> RayCast {
    RayCast {
        origin: origin.extend(0.0).to_array(),
        direction: direction.extend(100.0).to_array(),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn rays_hit_the_scenes_cubes() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    if device.ray_query().is_none() {
        assert!(
            !std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1"),
            "both reference GPUs have ray queries"
        );
        println!("skipping: no ray queries on this GPU");
        return;
    }
    println!("{:?}", device.ray_query().unwrap());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut scene = GpuScene::new(&device, &bindless, 64, 1).unwrap();
    let mut rays = RayScene::new(&device, &bindless, RaySettings::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let opaque = materials.add(MaterialDesc::solid([0.8, 0.8, 0.8], 0.0, 0.5));
    let glass = materials.add(MaterialDesc {
        base_color: [0.4, 0.8, 1.0, 0.3],
        alpha: AlphaMode::Blend,
        ..MaterialDesc::default()
    });
    let place =
        |scale: Vec3, at: Vec3| Mat4::from_scale_rotation_translation(scale, Quat::IDENTITY, at);
    let a = scene.insert(SceneInstance {
        mesh,
        material: opaque,
        world_from_object: place(Vec3::ONE, Vec3::ZERO),
    });
    let b = scene.insert(SceneInstance {
        mesh,
        material: opaque,
        world_from_object: place(Vec3::splat(2.0), Vec3::new(3.0, 0.0, 0.0)),
    });
    let mirrored = scene.insert(SceneInstance {
        mesh,
        material: opaque,
        world_from_object: place(Vec3::new(-1.0, 1.0, 1.0), Vec3::new(-3.0, 0.0, 0.0)),
    });
    let removed = scene.insert(SceneInstance {
        mesh,
        material: opaque,
        world_from_object: place(Vec3::ONE, Vec3::new(0.0, 0.0, 20.0)),
    });
    let transparent = scene.insert(SceneInstance {
        mesh,
        material: glass,
        world_from_object: place(Vec3::ONE, Vec3::new(0.0, 5.0, 0.0)),
    });
    assert!(scene.remove(removed));

    let z = Vec3::Z;
    // Each ray, its mask, and the expected hit: distance, instance, front face.
    let cases: [(RayCast, u32, Option<Expected>); 8] = [
        (
            ray(Vec3::new(0.0, 0.0, -10.0), z),
            RAY_MASK_ALL,
            Some((9.5, a.slot(), 1)),
        ),
        (
            ray(Vec3::new(3.0, 0.0, -10.0), z),
            RAY_MASK_ALL,
            Some((9.0, b.slot(), 1)),
        ),
        (
            ray(Vec3::new(-3.0, 0.0, -10.0), z),
            RAY_MASK_ALL,
            Some((9.5, mirrored.slot(), 1)),
        ),
        (ray(Vec3::new(6.0, 0.0, -10.0), z), RAY_MASK_ALL, None),
        // From inside the first cube: its far face's back.
        (ray(Vec3::ZERO, z), RAY_MASK_ALL, Some((0.5, a.slot(), 0))),
        // The transparent cube: only with its mask.
        (ray(Vec3::new(0.0, 5.0, -10.0), z), RAY_MASK_OPAQUE, None),
        (
            ray(Vec3::new(0.0, 5.0, -10.0), z),
            RAY_MASK_ALL,
            Some((9.5, transparent.slot(), 1)),
        ),
        // Where the removed cube was.
        (ray(Vec3::new(0.0, 0.0, 15.0), z), RAY_MASK_ALL, None),
    ];

    let hit_bytes = (cases.len() * size_of::<RayHit>()) as u64;
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default().size(hit_bytes).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                    | vk::BufferUsageFlags::TRANSFER_DST,
            ),
            MemoryLocation::Readback,
        )
        .unwrap();
    // SAFETY: created with the device address usage.
    let readback_address = unsafe {
        d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(readback))
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

    // Four frames: the first builds; the second compacts the bottom level and moves the
    // second cube (the top level follows the GPU scene); the third settles the move; the
    // fourth changes nothing (the top level is kept).
    let mut built_bytes = 0;
    for frame_number in 1..=4u64 {
        if frame_number == 2 {
            scene.set_transform(b, &place(Vec3::splat(2.0), Vec3::new(3.0, 0.0, 1.0)));
        }
        ring.begin_slot(0);
        // SAFETY: one-shot recording.
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
            let ray_frame = rays
                .add_passes(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &mut ring,
                    &bindless,
                    &geo,
                    &scene_frame,
                    frame_number,
                )
                .unwrap()
                .unwrap();
            let hits = fg.import_buffer(
                "hits",
                ImportedBuffer {
                    buffer: readback,
                    size: hit_bytes,
                    address: readback_address,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            for (k, (cast, mask, _)) in cases.iter().enumerate() {
                let address = ring.push(core::slice::from_ref(cast)).unwrap();
                let one = fg.create_buffer(
                    "hit",
                    engine_render::graph::BufferDesc {
                        size: size_of::<RayHit>() as u64,
                    },
                );
                ray_frame.add_cast_pass(&mut fg, &bindless, address, one, 1, *mask);
                let offset = (k * size_of::<RayHit>()) as u64;
                fg.add_pass("copy hit")
                    .read(one, engine_render::graph::Access::TransferRead)
                    .write(hits, engine_render::graph::Access::TransferWrite)
                    .execute(move |ctx| {
                        // SAFETY: recording; the graph made both buffers ready.
                        unsafe {
                            ctx.device.cmd_copy_buffer(
                                ctx.cmd,
                                ctx.buffer(one),
                                ctx.buffer(hits),
                                &[vk::BufferCopy {
                                    src_offset: 0,
                                    dst_offset: offset,
                                    size: size_of::<RayHit>() as u64,
                                }],
                            );
                        }
                    });
            }
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
        // SAFETY: the GPU is idle; the mapping holds the hits.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), hit_bytes as usize) };
        let results = bytemuck::cast_slice::<u8, RayHit>(bytes).to_vec();
        let stats = rays.stats();
        println!("frame {frame_number}: {stats:?}");
        for (k, hit) in results.iter().enumerate() {
            println!(
                "  ray {k}: t {:.4}, slot {}, front {}",
                hit.t, hit.slot, hit.front
            );
        }
        // From the second frame the moved cube is hit 1 unit later.
        for (k, ((_, _, expected), hit)) in cases.iter().zip(&results).enumerate() {
            match expected {
                None => assert!(
                    hit.t < 0.0,
                    "frame {frame_number}, ray {k} should miss: {hit:?}"
                ),
                Some((t, slot, front)) => {
                    let t = if *slot == b.slot() && frame_number > 1 {
                        t + 1.0
                    } else {
                        *t
                    };
                    assert!(
                        (hit.t - t).abs() < 1e-3,
                        "frame {frame_number}, ray {k}: {hit:?}, expected t {t}"
                    );
                    assert_eq!(hit.slot, *slot, "frame {frame_number}, ray {k}");
                    assert_eq!(
                        hit.front, *front,
                        "frame {frame_number}, ray {k}: front face"
                    );
                }
            }
        }
        assert_eq!(stats.blas_count, 1, "one mesh, one structure");
        assert_eq!(stats.triangles, 12);
        match frame_number {
            1 => {
                assert_eq!(stats.built, 1);
                built_bytes = stats.blas_bytes;
            }
            2 => {
                assert_eq!(stats.compacted, 1, "compacted once its frame finished");
                assert!(
                    stats.blas_bytes < built_bytes,
                    "{} -> {}",
                    built_bytes,
                    stats.blas_bytes
                );
                assert!(stats.rebuilt);
            }
            3 => assert!(stats.rebuilt, "the move settles"),
            _ => assert!(!stats.rebuilt, "a still scene keeps its top level"),
        }
    }

    // SAFETY: the device is idle.
    unsafe {
        rays.destroy(&device, &mut allocator);
        scene.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(readback, readback_memory);
        d.destroy_command_pool(pool, None);
    }
    drop(rays);
    drop(scene);
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
