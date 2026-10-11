//! The scene for ray queries (ADR 0087): cubes in a GPU scene (one scaled, one mirrored,
//! one removed, one transparent), its acceleration structures built, rays cast against
//! them on the GPU and their hits compared with the cubes' boxes: distances, slots, faces,
//! instance masks, the removed cube gone; over six frames, the bottom level compacted, a
//! cube moved (the top level rebuilt, then kept once the move settles), moved again (the
//! top level refitted), a cube added (rebuilt). Bottom-level builds held to a budget wait
//! for later frames. Under the validation layer with synchronization validation.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{BufferState, ImportedBuffer, RenderGraph};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::rays::{
    RAY_MASK_ALL, RAY_MASK_OPAQUE, RayCast, RayHit, RayScene, RaySettings, RayStats,
};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation,
};

/// A ray's expected nearest hit: distance, slot, front face.
type Expected = (f32, u32, u32);

/// Rays a frame casts at most.
const MAX_RAYS: usize = 16;

fn ray(origin: Vec3, direction: Vec3) -> RayCast {
    RayCast {
        origin: origin.extend(0.0).to_array(),
        direction: direction.extend(100.0).to_array(),
    }
}

fn place(scale: Vec3, at: Vec3) -> Mat4 {
    Mat4::from_scale_rotation_translation(scale, Quat::IDENTITY, at)
}

/// A device with a geometry pool, materials, a GPU scene and a readback buffer for hits.
struct Bed {
    instance: Instance,
    device: Device,
    allocator: Allocator,
    bindless: Bindless,
    geometry: GeometryPool,
    textures: TexturePool,
    materials: MaterialTable,
    ring: UploadRing,
    graph: RenderGraph,
    scene: GpuScene,
    readback: vk::Buffer,
    readback_memory: Allocation,
    readback_address: vk::DeviceAddress,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
}

impl Bed {
    /// `None` without a GPU or without ray queries (an error with `ENGINE_REQUIRE_GPU=1`).
    fn new() -> Option<Self> {
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
        let (instance, device) = match attempt() {
            Ok(pair) => pair,
            Err(e) if !required => {
                println!("skipping GPU test (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
                return None;
            }
            Err(e) => panic!("GPU required but unavailable: {e}"),
        };
        println!("adapter: {}", device.adapter().summary());
        if device.ray_query().is_none() {
            assert!(!required, "both reference GPUs have ray queries");
            println!("skipping: no ray queries on this GPU");
            return None;
        }
        println!("{:?}", device.ray_query().unwrap());
        let d = device.raw();
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let textures = TexturePool::new(&device, 1 << 20, 1);
        let materials = MaterialTable::new(1);
        let ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let scene = GpuScene::new(&device, &bindless, 64, 1).unwrap();
        let (readback, readback_memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size((MAX_RAYS * size_of::<RayHit>()) as u64)
                    .usage(
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
        Some(Self {
            instance,
            device,
            allocator,
            bindless,
            geometry,
            textures,
            materials,
            ring,
            graph,
            scene,
            readback,
            readback_memory,
            readback_address,
            pool,
            cmd,
        })
    }

    fn rays(&self, settings: RaySettings) -> RayScene {
        RayScene::new(&self.device, &self.bindless, settings, 1).unwrap()
    }

    /// Records and runs a frame: the scene's passes, `rays`' structures, then each ray of
    /// `casts` (with its mask) in a pass of its own; returns their hits and `rays`' stats.
    fn frame(
        &mut self,
        rays: &mut RayScene,
        frame_number: u64,
        casts: &[(RayCast, u32)],
    ) -> (Vec<RayHit>, RayStats) {
        assert!(casts.len() <= MAX_RAYS);
        let d = self.device.raw();
        let hit_bytes = (casts.len() * size_of::<RayHit>()) as u64;
        self.ring.begin_slot(0);
        // SAFETY: one-shot recording.
        unsafe {
            d.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(
                self.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
        }
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { self.graph.begin_slot(0, self.cmd) };
            self.textures
                .begin_frame(&mut fg, &mut self.allocator, &self.bindless, frame_number)
                .unwrap();
            let mats = self
                .materials
                .begin_frame(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &self.textures,
                    frame_number,
                )
                .unwrap();
            let geo = self
                .geometry
                .begin_frame(&mut fg, &self.device, &mut self.allocator, frame_number)
                .unwrap();
            let scene_frame = self
                .scene
                .add_passes(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &mut self.ring,
                    &self.bindless,
                    &geo,
                    &mats,
                    frame_number,
                )
                .unwrap()
                .unwrap();
            let ray_frame = rays
                .add_passes(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &mut self.ring,
                    &self.bindless,
                    &geo,
                    &scene_frame,
                    frame_number,
                )
                .unwrap()
                .unwrap();
            let hits = fg.import_buffer(
                "hits",
                ImportedBuffer {
                    buffer: self.readback,
                    size: hit_bytes,
                    address: self.readback_address,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            for (k, (cast, mask)) in casts.iter().enumerate() {
                let address = self.ring.push(core::slice::from_ref(cast)).unwrap();
                let one = fg.create_buffer(
                    "hit",
                    engine_render::graph::BufferDesc {
                        size: size_of::<RayHit>() as u64,
                    },
                );
                ray_frame.add_cast_pass(&mut fg, &self.bindless, address, one, 1, *mask);
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
            fg.execute_with_bindless(&mut self.allocator, Some(&self.bindless))
                .unwrap();
        }
        // SAFETY: as above.
        unsafe {
            d.end_command_buffer(self.cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(self.cmd)];
            d.queue_submit2(
                self.device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                vk::Fence::null(),
            )
            .unwrap();
            self.device.wait_idle().unwrap();
        }
        let ptr = self.readback_memory.mapped_ptr().unwrap();
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
        (results, stats)
    }

    /// Frees everything, then checks the validation layer's report.
    fn finish(mut self, rays: Vec<RayScene>) {
        let d = self.device.raw().clone();
        // SAFETY: the device is idle.
        unsafe {
            for mut rays in rays {
                rays.destroy(&self.device, &mut self.allocator);
            }
            self.scene.destroy(&mut self.allocator);
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.ring.destroy(&mut self.allocator);
            self.geometry.destroy(&mut self.allocator);
            self.materials.destroy(&mut self.allocator);
            self.textures.destroy(&mut self.allocator, &self.bindless);
            self.allocator
                .destroy_buffer(self.readback, self.readback_memory);
            d.destroy_command_pool(self.pool, None);
        }
        let Self {
            instance,
            device,
            allocator,
            bindless,
            scene,
            ..
        } = self;
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
}

/// Checks each hit against its expectation, `moved` added to the distance of the hits on
/// `moved_slot`.
fn check(
    frame_number: u64,
    hits: &[RayHit],
    expected: &[Option<Expected>],
    moved_slot: u32,
    moved: f32,
) {
    for (k, (expected, hit)) in expected.iter().zip(hits).enumerate() {
        match expected {
            None => assert!(
                hit.t < 0.0,
                "frame {frame_number}, ray {k} should miss: {hit:?}"
            ),
            Some((t, slot, front)) => {
                let t = if *slot == moved_slot { t + moved } else { *t };
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
}

#[test]
#[allow(clippy::too_many_lines)]
fn rays_hit_the_scenes_cubes() {
    let Some(mut bed) = Bed::new() else {
        return;
    };
    let mut rays = bed.rays(RaySettings::default());
    let mesh = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let opaque = bed
        .materials
        .add(MaterialDesc::solid([0.8, 0.8, 0.8], 0.0, 0.5));
    let glass = bed.materials.add(MaterialDesc {
        base_color: [0.4, 0.8, 1.0, 0.3],
        alpha: AlphaMode::Blend,
        ..MaterialDesc::default()
    });
    let scene = &mut bed.scene;
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
    let mut cases: Vec<(RayCast, u32, Option<Expected>)> = vec![
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

    // Six frames: the first builds; the second compacts the bottom level and moves the
    // second cube (rebuilt: a structure changed); the third settles the move and the
    // fourth changes nothing (the top level kept); the fifth moves it again (refitted);
    // the sixth adds a cube where the fourth ray passes (rebuilt).
    let mut built_bytes = 0;
    for frame_number in 1..=6u64 {
        let moved = match frame_number {
            1 => 0.0,
            2..=4 => 1.0,
            _ => 2.5,
        };
        if matches!(frame_number, 2 | 5) {
            let to = place(Vec3::splat(2.0), Vec3::new(3.0, 0.0, moved));
            assert!(bed.scene.set_transform(b, &to));
        }
        if frame_number == 6 {
            let c = bed.scene.insert(SceneInstance {
                mesh,
                material: opaque,
                world_from_object: place(Vec3::ONE, Vec3::new(6.0, 0.0, 0.0)),
            });
            cases[3].2 = Some((9.5, c.slot(), 1));
        }
        let casts: Vec<_> = cases.iter().map(|(cast, mask, _)| (*cast, *mask)).collect();
        let (hits, stats) = bed.frame(&mut rays, frame_number, &casts);
        let expected: Vec<_> = cases.iter().map(|(.., e)| *e).collect();
        check(frame_number, &hits, &expected, b.slot(), moved);
        assert_eq!(stats.blas_count, 1, "one mesh, one structure");
        assert_eq!(stats.triangles, 12);
        assert_eq!(stats.pending, 0);
        let kept = !stats.rebuilt && !stats.refitted;
        match frame_number {
            1 => {
                assert_eq!(stats.built, 1);
                assert!(stats.rebuilt);
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
                assert!(stats.rebuilt, "a bottom level changed");
            }
            3 => assert!(kept, "a settled move changes nothing"),
            4 => assert!(kept, "a still scene keeps its top level"),
            5 => assert!(stats.refitted && !stats.rebuilt, "only a move: refitted"),
            _ => assert!(stats.rebuilt, "an instance added: rebuilt"),
        }
    }
    bed.finish(vec![rays]);
}

#[test]
fn bottom_levels_wait_for_their_budget() {
    let Some(mut bed) = Bed::new() else {
        return;
    };
    // A budget smaller than either mesh: one structure per frame.
    let mut rays = bed.rays(RaySettings {
        build_budget: 1,
        ..RaySettings::default()
    });
    let first = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let second = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let opaque = bed
        .materials
        .add(MaterialDesc::solid([0.8, 0.8, 0.8], 0.0, 0.5));
    for (mesh, x) in [(first, 0.0), (second, 3.0)] {
        bed.scene.insert(SceneInstance {
            mesh,
            material: opaque,
            world_from_object: place(Vec3::ONE, Vec3::new(x, 0.0, 0.0)),
        });
    }
    let casts = [
        (ray(Vec3::new(0.0, 0.0, -10.0), Vec3::Z), RAY_MASK_ALL),
        (ray(Vec3::new(3.0, 0.0, -10.0), Vec3::Z), RAY_MASK_ALL),
    ];
    // The first frame builds one mesh's structure: one cube is hit, the other waits.
    let (hits, stats) = bed.frame(&mut rays, 1, &casts);
    assert_eq!((stats.built, stats.blas_count, stats.pending), (1, 1, 1));
    assert_eq!(
        hits.iter().filter(|h| h.t >= 0.0).count(),
        1,
        "only the built mesh's cube is in the top level: {hits:?}"
    );
    // The second builds the other (and compacts the first): both are hit.
    let (hits, stats) = bed.frame(&mut rays, 2, &casts);
    assert_eq!((stats.built, stats.blas_count, stats.pending), (1, 2, 0));
    assert!(stats.rebuilt, "a structure was added");
    for hit in &hits {
        assert!((hit.t - 9.5).abs() < 1e-3, "{hits:?}");
    }
    bed.finish(vec![rays]);
}
