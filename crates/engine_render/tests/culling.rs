//! GPU frustum culling and indirect instanced drawing (ADR 0034), under the validation
//! layer with synchronization validation.
//!
//! Hundreds of instances of three meshes in six batches (sizes chosen so batch boundaries
//! fall inside waves) are scattered in and around the view frustum. The test checks that
//! the culling pass counts exactly the instances the CPU reference (`engine_render::cull`)
//! finds visible, that turning culling off counts every instance, and that the rendered
//! image is the same either way (culling removes only what is off screen).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{EulerRot, Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::cull::{Frustum, transform_box};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, OwnedMeshData, cube};
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
            app_name: String::from("engine_render culling test"),
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

/// `(mesh, instances)` runs; neighbours differ, so each run is one batch.
const RUNS: [(usize, usize); 6] = [(0, 37), (1, 1), (0, 150), (2, 64), (1, 5), (2, 300)];

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as f32 / u32::MAX as f32
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next()
    }
}

#[test]
fn gpu_frustum_culling_matches_the_cpu_and_keeps_the_image() {
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

    // Three meshes: a cube, a tall box off its origin, a flat slab.
    let scaled = |scale: Vec3, offset: Vec3| -> OwnedMeshData {
        let mut mesh = cube();
        for p in &mut mesh.positions {
            *p = (Vec3::from(*p) * scale + offset).to_array();
        }
        mesh
    };
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let meshes = [
        cube(),
        scaled(Vec3::new(0.5, 2.0, 0.5), Vec3::new(0.0, 1.0, 0.0)),
        scaled(Vec3::new(2.0, 0.2, 2.0), Vec3::ZERO),
    ]
    .map(|mesh| geometry.add_mesh(&mesh.as_data()).unwrap());
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    // A material per instance: hundreds, beyond the table's first buffer.
    let mut materials = MaterialTable::new(1);
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);

    let eye = Vec3::new(0.0, 0.0, 10.0);
    let clip_from_world = vulkan_perspective(60f32.to_radians(), 1.0, 0.5, 60.0)
        * look_at(eye, Vec3::new(0.0, 0.0, 0.0), Vec3::Y);
    let frustum = Frustum::from_clip_from_world(clip_from_world);
    let camera = Camera {
        clip_from_world,
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.5),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.1),
    };

    // Scatter instances in a box around the frustum, away from its planes (so the CPU
    // and GPU agree despite rounding), and count the visible ones per batch.
    let mut rng = Rng(0x9e37_79b9);
    let mut draws: Vec<DrawItem> = Vec::new();
    let mut expected = Vec::new();
    for (mesh_index, count) in RUNS {
        let mesh = meshes[mesh_index];
        let info = *geometry.mesh(mesh).unwrap();
        let (lo, hi) = (info.min, info.max);
        let (center, extents) = ((lo + hi) * 0.5, (hi - lo) * 0.5);
        let mut visible = 0;
        for _ in 0..count {
            let (world_from_object, inside) = loop {
                let m = Mat4::from_scale_rotation_translation(
                    Vec3::splat(rng.range(0.3, 1.5)),
                    Quat::from_euler(
                        EulerRot::XYZ,
                        rng.range(0.0, 6.3),
                        rng.range(0.0, 6.3),
                        rng.range(0.0, 6.3),
                    ),
                    Vec3::new(
                        rng.range(-45.0, 45.0),
                        rng.range(-45.0, 45.0),
                        rng.range(-65.0, 15.0),
                    ),
                );
                let (c, e) = transform_box(&m, center, extents);
                let near_a_plane = frustum.planes.iter().any(|p| {
                    let n = p.truncate();
                    (n.dot(c) + n.abs().dot(e) + p.w).abs() < 0.05
                });
                if !near_a_plane {
                    break (m, frustum.intersects_box(c, e));
                }
            };
            visible += u32::from(inside);
            let color = [rng.next(), rng.next(), rng.next()];
            let metallic = rng.next();
            let material = materials.add(MaterialDesc::solid(color, metallic, rng.range(0.2, 1.0)));
            draws.push(DrawItem {
                mesh,
                material,
                world_from_object,
            });
        }
        expected.push(visible);
    }
    let total: u32 = expected.iter().sum();
    println!(
        "{} instances in {} batches; CPU reference: {total} visible {expected:?}",
        draws.len(),
        RUNS.len()
    );
    assert!(
        total > 20 && (total as usize) < draws.len() / 2,
        "{expected:?}"
    );

    let image_bytes = u64::from(SIZE * SIZE * 4);
    // VkDrawIndexedIndirectCommand: 5 words.
    let command_bytes = 20 * RUNS.len() as u64;
    let readback_size = image_bytes + command_bytes;
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
    // Culling on and off with the depth prepass, then culling on without it.
    for (frame, (culling, prepass)) in [(true, true), (false, true), (true, false)]
        .into_iter()
        .enumerate()
    {
        renderer.set_frustum_culling(culling);
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
                        occlusion: None,
                        temporal: None,
                        lights: &[],
                    },
                )
                .unwrap();
            assert_eq!(passes.instances as usize, draws.len());
            assert_eq!(passes.batches as usize, RUNS.len());
            let commands = passes.commands.unwrap();
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
                    let commands_region = vk::BufferCopy {
                        src_offset: 0,
                        dst_offset: image_bytes,
                        size: command_bytes,
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
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(commands),
                            ctx.buffer(out),
                            &[commands_region],
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
        println!(
            "culling={culling}: graph {:?}; ring used {} bytes",
            graph.stats(),
            ring.used()
        );

        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
        let word = |i: usize| {
            let at = image_bytes as usize + 4 * i;
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        // Commands are ordered front to back; each batch's is found by its first instance.
        let mut first = 0;
        let mut counted = Vec::new();
        for (mesh_index, count) in RUNS {
            // index_count, instance_count, first_index, vertex_offset, first_instance
            let batch = (0..RUNS.len())
                .find(|&b| word(b * 5 + 4) == first as u32)
                .unwrap();
            let info = geometry.mesh(meshes[mesh_index]).unwrap();
            assert_eq!(word(batch * 5), info.index_count);
            assert_eq!(word(batch * 5 + 2), info.first_index);
            assert_eq!(word(batch * 5 + 3), info.first_vertex);
            counted.push(word(batch * 5 + 1));
            first += count;
        }
        println!("culling={culling}: GPU counted {counted:?}");
        if culling {
            assert_eq!(counted, expected, "visible instances per batch");
        } else {
            let all: Vec<u32> = RUNS.iter().map(|&(_, count)| count as u32).collect();
            assert_eq!(counted, all, "every instance without culling");
        }
        images.push(bytes[..image_bytes as usize].to_vec());
    }
    let background = images[0]
        .chunks_exact(4)
        .filter(|p| *p == [0, 0, 64, 255])
        .count();
    let covered = (SIZE * SIZE) as usize - background;
    println!("{covered} of {} pixels covered", SIZE * SIZE);
    assert!(
        covered > 2000,
        "the visible instances should cover the image"
    );
    let differing = images[0]
        .chunks_exact(4)
        .zip(images[1].chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differing, 0, "culling changed {differing} pixels");
    // The prepass's depth equals the forward pass's bit for bit (`precise` positions):
    // shading with an equality test leaves no holes and changes nothing.
    let differing = images[0]
        .chunks_exact(4)
        .zip(images[2].chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differing, 0, "the depth prepass changed {differing} pixels");

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
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
