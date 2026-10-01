//! The persistent GPU scene (ADR 0064), under the validation layer with synchronization
//! validation:
//! - a scene draws the same pixels and motion vectors as the same draw list, with every
//!   pipeline state (opaque, masked, blended, double-sided, mirrored) and two meshes
//!   interleaved, with and without occlusion culling;
//! - slots are stable, reused lowest first after removals, and removed instances vanish;
//!   frames without changes upload nothing;
//! - motion vectors of moved, still (settled), teleported, new and removed instances;
//! - occlusion culling keys off slots: instances spawned in a shuffled order, and
//!   removed and spawned again, are culled as well as a fixed draw list is.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::too_many_lines)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_orthographic, vulkan_perspective};
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Occlusion, ShadowSettings,
    Temporal,
};
use engine_render::gpu_scene::{GpuScene, InstanceId, SceneInstance, SceneStats};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance as VkInstance,
    InstanceDesc, MemoryLocation,
};

const COLOR: vk::Format = vk::Format::R8G8B8A8_UNORM;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const COMMAND_BYTES: u64 = 64 * 1024;
const DEBUG_BYTES: u64 = 256 * 1024;
const PROBE_BYTES: u64 = 256 * 1024;

fn gpu() -> Option<(VkInstance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(VkInstance, Device), engine_rhi::RhiError> {
        let instance = VkInstance::new(&InstanceDesc {
            app_name: String::from("engine_render gpu scene test"),
            validation: std::env::var("STRESS_NO_VALIDATION").is_err(),
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: std::env::var("STRESS_NO_VALIDATION").is_err(),
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

/// What one frame draws.
enum Draw<'a> {
    List(&'a [DrawItem]),
    Scene(&'a mut GpuScene),
}

/// What one frame produced.
struct Shot {
    debug: Vec<u8>,
    image: Vec<u8>,
    depth: Vec<u8>,
    /// Motion vectors in pixels, with a temporal state.
    motion: Vec<[f32; 2]>,
    /// `instance_count` of every command of the forward lists (early, then late).
    counts: Vec<u32>,
    list_commands: u32,
    batches: u32,
    occlusion: bool,
    scene: Option<SceneStats>,
}

impl Shot {
    /// Instances drawn by phase `phase` (0: the only or early one, 1: late), the sink
    /// excluded.
    fn drawn(&self, phase: usize, draw_commands: usize) -> u32 {
        let list = self.list_commands as usize;
        self.counts[phase * list..phase * list + draw_commands]
            .iter()
            .sum()
    }
}

/// A device and everything a forward frame needs, rendering into `size`² targets.
/// Fields drop in declaration order: the device and the instance last, even when a
/// failed assertion unwinds.
struct Bed {
    renderer: ForwardRenderer,
    bindless: Bindless,
    allocator: Allocator,
    geometry: GeometryPool,
    textures: TexturePool,
    materials: MaterialTable,
    ring: UploadRing,
    graph: RenderGraph,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
    size: u32,
    frame_number: u64,
    /// Sky light, atmosphere and fog for the frames, when set (ADR 0069).
    environment: Option<Environment>,
    device: Device,
    instance: VkInstance,
}

impl Bed {
    fn new(size: u32) -> Option<Self> {
        let (instance, device) = gpu()?;
        println!("adapter: {}", device.adapter().summary());
        let d = device.raw();
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let renderer = ForwardRenderer::new(
            &device,
            &bindless,
            COLOR,
            DEPTH,
            ShadowSettings {
                resolution: std::env::var("STRESS_SHADOW_RES").ok().and_then(|v| v.parse().ok()).unwrap_or(512),
                cascades: std::env::var("STRESS_CASCADES").ok().and_then(|v| v.parse().ok()).unwrap_or(4),
                ..ShadowSettings::default()
            },
        )
        .unwrap();
        let geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let textures = TexturePool::new(&device, 1 << 20, 1);
        let materials = MaterialTable::new(1);
        let ring = UploadRing::new(&device, &mut allocator, 1, 8 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let bytes = u64::from(size * size * 4) * 3 + COMMAND_BYTES + 3 * DEBUG_BYTES + PROBE_BYTES;
        let (readback, readback_memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                if std::env::var("ENGINE_DEBUG_UNCACHED_READBACK").is_ok() {
                    MemoryLocation::Upload
                } else {
                    MemoryLocation::Readback
                },
            )
            .unwrap();
        eprintln!(
            "readback memory type {}",
            readback_memory.memory_type()
        );
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
            renderer,
            geometry,
            textures,
            materials,
            ring,
            graph,
            pool,
            cmd,
            readback,
            readback_memory: Some(readback_memory),
            size,
            frame_number: 0,
            environment: None,
        })
    }

    /// Renders one frame and reads it back.
    fn frame(
        &mut self,
        draw: Draw<'_>,
        camera: Camera,
        occlusion: Option<&Occlusion>,
        temporal: Option<&Temporal>,
    ) -> Shot {
        let Self {
            device,
            allocator,
            bindless,
            renderer,
            geometry,
            textures,
            materials,
            ring,
            graph,
            cmd,
            readback,
            readback_memory,
            size,
            frame_number,
            environment,
            ..
        } = self;
        let (d, cmd, size) = (device.raw(), *cmd, *size);
        let image_bytes = u64::from(size * size * 4);
        let total = image_bytes * 3 + COMMAND_BYTES + 3 * DEBUG_BYTES + PROBE_BYTES;
        let probe_offset = image_bytes * 3 + COMMAND_BYTES + 3 * DEBUG_BYTES;
        let probe_words = (PROBE_BYTES / 4) as usize;
        let probe_on = std::env::var("STRESS_PROBE").is_ok();
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
        *frame_number += 1;
        if std::env::var("ENGINE_DEBUG_SENTINEL").is_ok() {
            let ptr = readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping covers the buffer.
            unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0xCD, total as usize) };
        }
        let light = DirectionalLight {
            direction: Vec3::new(-0.3, -1.0, -0.4),
            color: Vec3::splat(2.5),
            ambient: Vec3::splat(0.15),
        };
        let shot;
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            textures
                .begin_frame(&mut fg, allocator, bindless, *frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, device, allocator, textures, *frame_number)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, device, allocator, *frame_number)
                .unwrap();
            let probe = if probe_on {
                let (out, staged) = ring.alloc::<u32>(probe_words).unwrap();
                let tag = (*frame_number as u32) << 20;
                for (i, w) in out.iter_mut().enumerate() {
                    w.write(tag | i as u32);
                }
                Some(staged)
            } else {
                None
            };
            let color = fg.create_texture("color", TextureDesc::new_2d(COLOR, size, size));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, size, size));
            let view = ForwardView {
                camera,
                light,
                color,
                depth,
                clear: Some([0.0, 0.0, 0.25, 1.0]),
                occlusion,
                temporal,
                lights: &[],
                environment: environment.as_ref(),
            };
            let (passes, scene) = match draw {
                Draw::List(draws) => (
                    renderer
                        .add_passes(&mut fg, ring, bindless, &geo, &mats, draws, &view)
                        .unwrap(),
                    None,
                ),
                Draw::Scene(scene) => {
                    let frame = scene
                        .add_passes(
                            &mut fg,
                            device,
                            allocator,
                            ring,
                            bindless,
                            &geo,
                            &mats,
                            *frame_number,
                        )
                        .unwrap()
                        .unwrap();
                    let stats = frame.stats();
                    (
                        renderer
                            .add_scene_passes(&mut fg, ring, bindless, &geo, &mats, &frame, &view)
                            .unwrap(),
                        Some(stats),
                    )
                }
            };
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: *readback,
                    size: total,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            let phases: u64 = if passes.occlusion { 2 } else { 1 };
            let command_bytes = phases * u64::from(passes.list_commands) * 20;
            assert!(command_bytes <= COMMAND_BYTES);
            let velocity = passes.motion.map(|m| m.velocity);
            let commands = passes.commands;
            if let Some(staged) = probe {
                fg.add_pass("probe")
                    .write(out, Access::TransferWrite)
                    .execute(move |ctx| {
                        // SAFETY: recording; in bounds.
                        unsafe {
                            ctx.device.cmd_copy_buffer(
                                ctx.cmd,
                                staged.buffer,
                                ctx.buffer(out),
                                &[vk::BufferCopy { src_offset: staged.offset, dst_offset: probe_offset, size: PROBE_BYTES }],
                            );
                        }
                    });
            }
            let read_depth = std::env::var("ENGINE_DEBUG_DEPTH_STORE").is_ok();
            let mut pass = fg
                .add_pass("read back")
                .read(color, Access::TransferRead)
                .write(out, Access::TransferWrite);
            if let Some(v) = velocity {
                pass = pass.read(v, Access::TransferRead);
            }
            if let Some(c) = commands {
                pass = pass.read(c, Access::TransferRead);
            }
            if read_depth {
                pass = pass.read(depth, Access::TransferRead);
            }
            let debug = passes.debug;
            let inst = u64::from(passes.instances);
            let casc: u64 = std::env::var("STRESS_CASCADES").ok().and_then(|v| v.parse().ok()).unwrap_or(4); let debug_sizes = [phases * inst * 4, casc * u64::from(passes.list_commands) * 20, casc * inst * 4];
            if let Some((a, b, c)) = debug {
                pass = pass
                    .read(a, Access::TransferRead)
                    .read(b, Access::TransferRead)
                    .read(c, Access::TransferRead);
            }
            pass.execute(move |ctx| {
                let region = |offset| vk::BufferImageCopy {
                    buffer_offset: offset,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width: size,
                        height: size,
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
                        &[region(0)],
                    );
                    if let Some(v) = velocity {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(v),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[region(image_bytes)],
                        );
                    }
                    if let Some((a, b, c)) = debug {
                        for (i, buf) in [a, b, c].into_iter().enumerate() {
                            let size = debug_sizes[i].min(DEBUG_BYTES);
                            ctx.device.cmd_copy_buffer(
                                ctx.cmd,
                                ctx.buffer(buf),
                                ctx.buffer(out),
                                &[vk::BufferCopy {
                                    src_offset: 0,
                                    dst_offset: 3 * image_bytes + COMMAND_BYTES + i as u64 * DEBUG_BYTES,
                                    size,
                                }],
                            );
                        }
                    }
                    if read_depth {
                        let mut r = region(2 * image_bytes + COMMAND_BYTES);
                        r.image_subresource.aspect_mask = vk::ImageAspectFlags::DEPTH;
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(depth),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[r],
                        );
                    }
                    if let Some(c) = commands.filter(|_| command_bytes > 0) {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(c),
                            ctx.buffer(out),
                            &[vk::BufferCopy {
                                src_offset: 0,
                                dst_offset: 2 * image_bytes,
                                size: command_bytes,
                            }],
                        );
                    }
                }
            });
            fg.execute_with_bindless(allocator, Some(bindless)).unwrap();
            shot = (passes, scene, command_bytes, velocity.is_some());
        }
        // SAFETY: as above.
        if let Some(us) = std::env::var("STRESS_SLEEP_US").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_micros(us));
        }
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
        if std::env::var("STRESS_RING_HASH").is_ok() {
            let bytes = ring.debug_used_bytes();
            let mut h: u64 = 0xcbf29ce484222325;
            for &b in &bytes {
                h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
            }
            eprintln!("RING frame {frame_number}: {} bytes hash {h:016x}", bytes.len());
        }
        let (passes, scene, command_bytes, has_motion) = shot;
        let ptr = readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), total as usize) };
        let image = bytes[..image_bytes as usize].to_vec();
        let sentinel = image.chunks_exact(4).filter(|p| *p == [0xCD; 4]).count();
        if sentinel > 0 {
            eprintln!("DIFF sentinel pixels: {sentinel}");
        }
        let motion = if has_motion {
            bytes[image_bytes as usize..2 * image_bytes as usize]
                .chunks_exact(4)
                .map(|p| {
                    let h = |a: u8, b: u8| half::to_f32(u16::from_le_bytes([a, b])) * size as f32;
                    [h(p[0], p[1]), h(p[2], p[3])]
                })
                .collect()
        } else {
            Vec::new()
        };
        let commands = &bytes[2 * image_bytes as usize..(2 * image_bytes + command_bytes) as usize];
        let debug_bytes = bytes[(3 * image_bytes + COMMAND_BYTES) as usize..probe_offset as usize].to_vec();
        if probe_on {
            let tag = (*frame_number as u32) << 20;
            let words = &bytes[probe_offset as usize..(probe_offset + PROBE_BYTES) as usize];
            let wrong: Vec<(usize, u32)> = words
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .enumerate()
                .filter(|&(i, w)| w != tag | i as u32)
                .collect();
            if !wrong.is_empty() {
                eprintln!("PROBE frame {frame_number}: {} words stale, first {:x?}", wrong.len(), &wrong[..wrong.len().min(4)]);
            }
        }
        let depth_bytes = bytes[(2 * image_bytes + COMMAND_BYTES) as usize
            ..(3 * image_bytes + COMMAND_BYTES) as usize]
            .to_vec();
        // `instance_count`, the second word of each 20-byte command.
        let counts = commands
            .chunks_exact(20)
            .map(|c| u32::from_le_bytes([c[4], c[5], c[6], c[7]]))
            .collect();
        Shot {
            debug: debug_bytes,
            image,
            depth: depth_bytes,
            motion,
            counts,
            list_commands: passes.list_commands,
            batches: passes.batches,
            occlusion: passes.occlusion,
            scene,
        }
    }

    fn scene(&self, capacity: u32) -> GpuScene {
        GpuScene::new(&self.device, &self.bindless, capacity, 1).unwrap()
    }

    fn finish(mut self, scenes: Vec<GpuScene>, states: (Vec<Occlusion>, Vec<Temporal>)) {
        let allocator = &mut self.allocator;
        // SAFETY: the device is idle.
        unsafe {
            for mut scene in scenes {
                scene.destroy(allocator);
            }
            for mut o in states.0 {
                o.destroy(allocator);
            }
            for mut t in states.1 {
                t.destroy(allocator);
            }
            self.graph.destroy(allocator, Some(&self.bindless));
            self.ring.destroy(allocator);
            self.geometry.destroy(allocator);
            self.materials.destroy(allocator);
            self.textures.destroy(allocator, &self.bindless);
            if let Some(environment) = self.environment.as_mut() {
                environment.destroy(allocator, &self.bindless);
            }
            if let Some(memory) = self.readback_memory.take() {
                allocator.destroy_buffer(self.readback, memory);
            }
            self.device.raw().destroy_command_pool(self.pool, None);
        }
        let Self {
            instance,
            renderer,
            bindless,
            allocator,
            ..
        } = self;
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
}

fn differing(a: &[u8], b: &[u8]) -> usize {
    a.chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count()
}

/// A perspective camera at `eye` looking at the origin.
fn perspective_camera(eye: Vec3) -> Camera {
    Camera {
        clip_from_world: vulkan_perspective(60f32.to_radians(), 1.0, 0.5, 100.0)
            * look_at(eye, Vec3::ZERO, Vec3::Y),
        position: eye,
    }
}

/// A mixed scene: two meshes interleaved, every pipeline state, distinct distances.
fn mixed_items(meshes: [MeshId; 2], materials: &[MaterialId], t: f32) -> Vec<DrawItem> {
    let mut items = Vec::new();
    for i in 0..60 {
        let (x, z) = ((i % 10) as f32 - 4.5, (i / 10) as f32 - 2.5);
        // Every third one moves with `t`; every seventh is mirrored.
        let lift = if i % 3 == 0 {
            t * (1.0 + i as f32 * 0.01)
        } else {
            0.0
        };
        let scale = if i % 7 == 0 {
            Vec3::new(-0.6, 0.6, 0.6)
        } else {
            Vec3::splat(0.6)
        };
        items.push(DrawItem {
            mesh: meshes[i % 2],
            material: materials[i % materials.len()],
            world_from_object: Mat4::from_scale_rotation_translation(
                scale,
                Quat::from_rotation_y(i as f32 * 0.37),
                Vec3::new(x * 1.1 + i as f32 * 0.013, 0.3 + lift, z * 1.3),
            ),
        });
    }
    // The ground.
    items.push(DrawItem {
        mesh: meshes[0],
        material: materials[0],
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(14.0, 0.2, 10.0),
            Quat::IDENTITY,
            Vec3::new(0.0, -0.4, 0.0),
        ),
    });
    items
}

#[test]
fn a_scene_draws_what_the_same_draw_list_draws() {
    let Some(mut bed) = Bed::new(128) else {
        return;
    };
    let cube_a = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let cube_b = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let mut palette = Vec::new();
    for i in 0..9 {
        let c = [(i % 3) as f32 / 3.0 + 0.2, (i % 4) as f32 / 4.0 + 0.1, 0.5];
        palette.push(bed.materials.add(MaterialDesc::solid(c, 0.0, 0.5)));
    }
    palette.push(bed.materials.add(MaterialDesc {
        alpha: AlphaMode::Mask { cutoff: 0.5 },
        ..MaterialDesc::solid([0.9, 0.9, 0.2], 0.0, 0.4)
    }));
    palette.push(bed.materials.add(MaterialDesc {
        double_sided: true,
        ..MaterialDesc::solid([0.2, 0.9, 0.9], 0.0, 0.4)
    }));
    palette.push(bed.materials.add(MaterialDesc {
        alpha: AlphaMode::Blend,
        base_color: [0.9, 0.3, 0.3, 0.5],
        ..MaterialDesc::default()
    }));
    let camera = perspective_camera(Vec3::new(2.0, 7.0, 9.0));
    let mut scene = bed.scene(16);
    let items = mixed_items([cube_a, cube_b], &palette, 0.0);
    let ids: Vec<InstanceId> = items
        .iter()
        .map(|d| {
            scene.insert(SceneInstance {
                mesh: d.mesh,
                material: d.material,
                world_from_object: d.world_from_object,
            })
        })
        .collect();
    let occlusion_list = Occlusion::new(&bed.device, &mut bed.allocator, 1024).unwrap();
    let occlusion_scene = Occlusion::new(&bed.device, &mut bed.allocator, 1024).unwrap();
    let temporal_list = Temporal::new(&bed.device, &mut bed.allocator, 1024).unwrap();
    let temporal_scene = Temporal::new(&bed.device, &mut bed.allocator, 1024).unwrap();
    // The last round under the atmosphere and height fog (ADR 0069): its passes come from
    // the shared declaration, the same on both paths.
    for (prepass, occlusion, atmosphere) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, true, true),
    ] {
        if atmosphere {
            let mut environment = Environment::new(
                &bed.device,
                &mut bed.allocator,
                &bed.bindless,
                Sky::default(),
            )
            .unwrap();
            environment
                .set_atmosphere(
                    &bed.device,
                    &mut bed.allocator,
                    &bed.bindless,
                    Some(Atmosphere {
                        aerial_perspective_scale: 1000.0,
                        ..Atmosphere::earth()
                    }),
                )
                .unwrap();
            environment.set_fog(Some(HeightFog {
                density: 0.05,
                ..HeightFog::default()
            }));
            bed.environment = Some(environment);
        }
        bed.renderer.set_depth_prepass(prepass);
        let (ol, os) = if occlusion {
            (Some(&occlusion_list), Some(&occlusion_scene))
        } else {
            (None, None)
        };
        for t in [0.0f32, 0.25, 0.5] {
            let items = mixed_items([cube_a, cube_b], &palette, t);
            for (id, item) in ids.iter().zip(&items) {
                scene.set_transform(*id, &item.world_from_object);
            }
            let list = bed.frame(Draw::List(&items), camera, ol, Some(&temporal_list));
            let shot = bed.frame(Draw::Scene(&mut scene), camera, os, Some(&temporal_scene));
            let label =
                format!("prepass {prepass}, occlusion {occlusion}, atmosphere {atmosphere}, t {t}");
            if differing(&list.image, &shot.image) != 0 {
                let mut n = 0;
                for (i, (a, b)) in list
                    .image
                    .chunks_exact(4)
                    .zip(shot.image.chunks_exact(4))
                    .enumerate()
                {
                    if a != b && n < 40 {
                        eprintln!(
                            "DIFF px ({}, {}): list {:?} scene {:?}",
                            i % 128,
                            i / 128,
                            a,
                            b
                        );
                        n += 1;
                    }
                }
                eprintln!("DIFF list counts {:?}", list.counts);
                eprintln!("DIFF scene counts {:?}", shot.counts);
                let again = bed.frame(Draw::List(&items), camera, ol, Some(&temporal_list));
                eprintln!(
                    "DIFF list again vs list: {}, vs scene: {}",
                    differing(&list.image, &again.image),
                    differing(&shot.image, &again.image)
                );
                eprintln!("DIFF again counts {:?}", again.counts);
                if let Ok(dir) = std::env::var("ENGINE_DIFF_DIR") {
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    for (name, img) in [
                        ("list", &list.image),
                        ("scene", &shot.image),
                        ("again", &again.image),
                    ] {
                        let png = engine_render::png::encode_png(
                            128,
                            128,
                            engine_render::png::PngFormat::Rgba,
                            img,
                        )
                        .unwrap();
                        std::fs::write(format!("{dir}/{stamp}_{name}.png"), png).unwrap();
                    }
                    eprintln!("DIFF images {dir}/{stamp}_*.png");
                }
            }
            assert_eq!(
                differing(&list.image, &shot.image),
                0,
                "{label}: the scene's image differs from the draw list's"
            );
            assert!(
                list.motion == shot.motion,
                "{label}: the scene's motion vectors differ from the draw list's"
            );
            let moved = shot.motion.iter().filter(|m| m[1] != 0.0).count();
            if t > 0.0 {
                assert!(moved > 0, "{label}: something moved");
            }
            // Fewer batches: one per (mesh, state) instead of per run of draws.
            assert!(shot.batches < list.batches, "{label}");
            let stats = shot.scene.unwrap();
            println!(
                "{label}: list {} batches, scene {} batches, uploaded {}, settled {}, {} bytes",
                list.batches, shot.batches, stats.uploaded, stats.settled, stats.upload_bytes
            );
        }
    }
    bed.finish(
        vec![scene],
        (
            vec![occlusion_list, occlusion_scene],
            vec![temporal_list, temporal_scene],
        ),
    );
}

/// When most slots change, the scene copies the whole array and swaps its two buffers
/// instead of scattering: the frames must still match the draw list, through every
/// transition (full, scattered, full with new instances, settling, growth).
#[test]
fn full_uploads_draw_what_the_draw_list_draws() {
    const N: usize = 4800;
    let Some(mut bed) = Bed::new(128) else {
        return;
    };
    let mesh = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let palette: Vec<MaterialId> = (0..16)
        .map(|i| {
            bed.materials.add(MaterialDesc::solid(
                [(i % 4) as f32 / 4.0 + 0.2, (i / 4) as f32 / 4.0 + 0.1, 0.5],
                0.0,
                0.5,
            ))
        })
        .collect();
    let place = |i: usize, t: f32| {
        let (x, z) = ((i % 80) as f32 - 40.0, (i / 80) as f32 - 30.0);
        Mat4::from_scale_rotation_translation(
            Vec3::splat(0.35),
            Quat::from_rotation_y(t + i as f32 * 0.1),
            Vec3::new(x * 0.5, 0.2 + (t * 3.0 + i as f32).sin() * 0.1, z * 0.5),
        )
    };
    let camera = perspective_camera(Vec3::new(0.0, 14.0, 16.0));
    let temporal_list = Temporal::new(&bed.device, &mut bed.allocator, 8192).unwrap();
    let temporal_scene = Temporal::new(&bed.device, &mut bed.allocator, 8192).unwrap();
    // Starts small: the first upload grows the buffers, during a full copy.
    let mut scene = bed.scene(64);
    let mut list: Vec<DrawItem> = Vec::new();
    let mut ids = Vec::new();
    let add = |scene: &mut GpuScene,
               list: &mut Vec<DrawItem>,
               ids: &mut Vec<InstanceId>,
               i: usize,
               t: f32| {
        let item = DrawItem {
            mesh,
            material: palette[i % palette.len()],
            world_from_object: place(i, t),
        };
        list.push(item);
        ids.push(scene.insert(SceneInstance {
            mesh,
            material: item.material,
            world_from_object: item.world_from_object,
        }));
    };
    for i in 0..N {
        add(&mut scene, &mut list, &mut ids, i, 0.0);
    }
    // (moved instances, new instances, expect a full copy)
    let steps: [(usize, usize, bool); 6] = [
        (0, 0, true), // all new: full, growing from 64 slots
        (N, 0, true), // all moved
        (N / 10, 0, false),
        (N, 40, true), // all moved, 40 new: their previous transform is their own
        (0, 0, false), // nothing moved: everything settles
        (0, 0, false), // nothing at all
    ];
    for (k, &(moved, added, full)) in steps.iter().enumerate() {
        let t = k as f32 * 0.05;
        let stride = if moved == 0 { usize::MAX } else { N / moved };
        let ids_now = ids.len();
        for i in (0..ids_now).step_by(stride.max(1)).take(moved) {
            list[i].world_from_object = place(i, t);
            scene.set_transform(ids[i], &list[i].world_from_object);
        }
        for i in ids_now..ids_now + added {
            add(&mut scene, &mut list, &mut ids, i, t);
        }
        let a = bed.frame(Draw::List(&list), camera, None, Some(&temporal_list));
        let b = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal_scene));
        let stats = b.scene.unwrap();
        println!("step {k}: {stats:?}");
        assert_eq!(stats.full, full, "step {k}");
        assert_eq!(differing(&a.image, &b.image), 0, "step {k}: image");
        assert!(a.motion == b.motion, "step {k}: motion vectors");
        if k == 0 || k == 3 {
            assert!(stats.grew, "step {k}");
        }
        if k == 5 {
            assert_eq!(stats.upload_bytes, 0, "nothing to do");
        }
        if k == 4 {
            // Still: no motion beyond rounding (the pixel center against the
            // interpolated previous position).
            let most = b
                .motion
                .iter()
                .map(|m| m[0].abs().max(m[1].abs()))
                .fold(0.0, f32::max);
            assert!(most < 0.01, "settled: {most} px");
            // The slots moved by the full copy before settle with 16 bytes each.
            assert_eq!(
                (stats.settled, stats.upload_bytes),
                (N as u32, N as u64 * 16)
            );
        }
    }
    bed.finish(
        vec![scene],
        (Vec::new(), vec![temporal_list, temporal_scene]),
    );
}

#[test]
fn slots_are_stable_reused_lowest_first_and_removed_instances_vanish() {
    let Some(mut bed) = Bed::new(64) else {
        return;
    };
    let mesh = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let material = bed.materials.default_material();
    let camera = perspective_camera(Vec3::new(0.0, 4.0, 8.0));
    let at = |x: f32| Mat4::from_translation(Vec3::new(x, 0.0, 0.0));
    let item = |x: f32| DrawItem {
        mesh,
        material,
        world_from_object: at(x),
    };
    let instance = |x: f32| SceneInstance {
        mesh,
        material,
        world_from_object: at(x),
    };
    let mut scene = bed.scene(2);
    let mut ids: Vec<InstanceId> = [-3.0, -1.0]
        .into_iter()
        .map(|x| scene.insert(instance(x)))
        .collect();
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, None);
    assert_eq!(shot.scene.unwrap().uploaded, 2);
    // Two more: the buffers grow from 2 slots to 4, keeping the first two (copied, not
    // uploaded again).
    ids.extend([1.0, 3.0].into_iter().map(|x| scene.insert(instance(x))));
    assert_eq!(
        ids.iter().map(|i| i.slot()).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, None);
    let stats = shot.scene.unwrap();
    assert_eq!((stats.instances, stats.slots, stats.uploaded), (4, 4, 2));
    assert!(stats.grew, "from 2 slots to 4");
    let list = bed.frame(
        Draw::List(&[item(-3.0), item(-1.0), item(1.0), item(3.0)]),
        camera,
        None,
        None,
    );
    assert_eq!(differing(&list.image, &shot.image), 0);

    // A frame without changes uploads nothing.
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, None);
    let stats = shot.scene.unwrap();
    assert_eq!(
        (stats.uploaded, stats.settled, stats.upload_bytes),
        (0, 0, 0)
    );
    assert_eq!(differing(&list.image, &shot.image), 0);

    // Remove the second: a hole the cull visits and nothing draws.
    assert!(scene.remove(ids[1]));
    assert!(!scene.remove(ids[1]), "stale ids are ignored");
    assert!(!scene.set_transform(ids[1], &at(9.0)));
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, None);
    let stats = shot.scene.unwrap();
    assert_eq!((stats.instances, stats.slots, stats.uploaded), (3, 4, 1));
    assert_eq!(shot.drawn(0, 1), 3);
    let list = bed.frame(
        Draw::List(&[item(-3.0), item(1.0), item(3.0)]),
        camera,
        None,
        None,
    );
    assert_eq!(differing(&list.image, &shot.image), 0);
    // Culling off: the hole goes to the sink's command (counted there, drawn by none).
    bed.renderer.set_frustum_culling(false);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, None);
    assert_eq!(shot.drawn(0, 1), 3);
    assert_eq!(shot.counts[1], 1, "the sink");
    assert_eq!(differing(&list.image, &shot.image), 0);
    bed.renderer.set_frustum_culling(true);

    // A new instance takes the lowest free slot; the removed one's id stays stale.
    let new = scene.insert(instance(0.0));
    assert_eq!(new.slot(), 1);
    assert_ne!(new, ids[1]);
    assert!(scene.get(ids[1]).is_none());
    assert_eq!(scene.get(new).unwrap().world_from_object, at(0.0));
    // Removing the highest slot lowers what culling visits.
    scene.remove(ids[3]);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, None);
    let stats = shot.scene.unwrap();
    assert_eq!((stats.instances, stats.slots), (3, 3));
    let list = bed.frame(
        Draw::List(&[item(-3.0), item(0.0), item(1.0)]),
        camera,
        None,
        None,
    );
    assert_eq!(differing(&list.image, &shot.image), 0);

    // An unchanged transform uploads nothing; a moved one uploads one instance, and its
    // previous transform settles in the frame after.
    assert!(!scene.set_transform(ids[0], &at(-3.0)));
    assert!(scene.set_transform(ids[0], &at(-2.5)));
    let stats = bed
        .frame(Draw::Scene(&mut scene), camera, None, None)
        .scene
        .unwrap();
    assert_eq!((stats.uploaded, stats.settled), (1, 0));
    assert_eq!(stats.upload_bytes, 16 + 64);
    let stats = bed
        .frame(Draw::Scene(&mut scene), camera, None, None)
        .scene
        .unwrap();
    assert_eq!((stats.uploaded, stats.settled), (0, 1));
    assert_eq!(stats.upload_bytes, 16);

    // Everything removed: nothing drawn, the clear color everywhere.
    scene.clear();
    assert!(scene.is_empty());
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, None);
    assert!(shot.image.chunks_exact(4).all(|p| p == [0, 0, 64, 255]));
    bed.finish(vec![scene], (Vec::new(), Vec::new()));
}

#[test]
fn motion_vectors_of_moved_still_teleported_new_and_removed_instances() {
    const SIZE: u32 = 64;
    // The orthographic view spans 8 units: one unit is 8 pixels.
    let Some(mut bed) = Bed::new(SIZE) else {
        return;
    };
    let mesh = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let material = bed.materials.default_material();
    let eye = Vec3::new(0.0, 0.0, 10.0);
    let camera = Camera {
        clip_from_world: vulkan_orthographic(-4.0, 4.0, -4.0, 4.0, 0.1, 50.0)
            * look_at(eye, Vec3::ZERO, Vec3::Y),
        position: eye,
    };
    let at = |x: f32, y: f32| {
        Mat4::from_scale_rotation_translation(
            Vec3::splat(1.5),
            Quat::IDENTITY,
            Vec3::new(x, y, 0.0),
        )
    };
    let temporal = Temporal::new(&bed.device, &mut bed.allocator, 16).unwrap();
    let mut scene = bed.scene(16);
    // Pixel of world (x, y): the image's center is (32, 32), y grows downwards.
    let pixel = |v: &[[f32; 2]], x: f32, y: f32| {
        let (px, py) = ((32.0 + x * 8.0) as u32, (32.0 - y * 8.0) as u32);
        v[(py * SIZE + px) as usize]
    };
    let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.05 && (a[1] - b[1]).abs() < 0.05;

    let a = scene.insert(SceneInstance {
        mesh,
        material,
        world_from_object: at(-2.0, 1.5),
    });
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert!(
        near(pixel(&shot.motion, -2.0, 1.5), [0.0; 2]),
        "first frame"
    );

    // Moved right by 0.5 units: 4 pixels.
    scene.set_transform(a, &at(-1.5, 1.5));
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    let m = pixel(&shot.motion, -1.5, 1.5);
    println!("moved: {m:?}");
    assert!(near(m, [4.0, 0.0]), "moved 4 px right: {m:?}");

    // Still: its previous transform settled, no motion.
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert_eq!(shot.scene.unwrap().settled, 1);
    assert!(near(pixel(&shot.motion, -1.5, 1.5), [0.0; 2]), "still");

    // Teleported: no motion in its first frame there.
    scene.teleport(a, &at(1.5, -1.5));
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert!(near(pixel(&shot.motion, 1.5, -1.5), [0.0; 2]), "teleported");

    // A new instance in a reused slot (the removed one's) is still in its first frame,
    // whatever the slot held before; the moving one keeps moving.
    let b = scene.insert(SceneInstance {
        mesh,
        material,
        world_from_object: at(-2.0, -1.5),
    });
    scene.remove(b);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert!(
        near(pixel(&shot.motion, -2.0, -1.5), [0.0; 2]),
        "removed: background"
    );
    let c = scene.insert(SceneInstance {
        mesh,
        material,
        world_from_object: at(-1.5, 1.5),
    });
    assert_eq!(c.slot(), b.slot(), "the freed slot");
    scene.set_transform(a, &at(1.5, -1.0));
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert!(near(pixel(&shot.motion, -1.5, 1.5), [0.0; 2]), "new: still");
    let m = pixel(&shot.motion, 1.5, -1.0);
    assert!(near(m, [0.0, -4.0]), "moved 4 px up: {m:?}");

    // Removed: the background behind it shows the camera's motion (none).
    scene.remove(a);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert!(near(pixel(&shot.motion, 1.5, -1.0), [0.0; 2]), "removed");

    // After a reset nothing has a previous transform.
    scene.set_transform(c, &at(0.0, 1.5));
    temporal.reset();
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert!(near(pixel(&shot.motion, 0.0, 1.5), [0.0; 2]), "reset");
    bed.finish(vec![scene], (Vec::new(), vec![temporal]));
}

#[test]
fn occlusion_culling_keys_off_slots_whatever_the_spawn_order() {
    const BEHIND: usize = 200;
    const IN_FRONT: usize = 20;
    let Some(mut bed) = Bed::new(256) else {
        return;
    };
    let mesh = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let palette: Vec<MaterialId> = (0..12)
        .map(|i| {
            bed.materials.add(MaterialDesc::solid(
                [(i % 3) as f32 / 3.0, (i % 4) as f32 / 4.0, 0.4],
                0.0,
                0.6,
            ))
        })
        .collect();
    // As in tests/occlusion.rs: a wall filling the view, cubes behind and in front.
    let eye = Vec3::new(0.0, 0.0, 12.0);
    let camera = Camera {
        clip_from_world: vulkan_perspective(60f32.to_radians(), 1.0, 0.5, 100.0)
            * look_at(eye, Vec3::ZERO, Vec3::Y),
        position: eye,
    };
    let wall = SceneInstance {
        mesh,
        material: bed.materials.default_material(),
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(12.0, 12.0, 0.5),
            Quat::IDENTITY,
            Vec3::new(0.0, 0.0, 5.0),
        ),
    };
    let behind = |i: usize| SceneInstance {
        mesh,
        material: palette[i % palette.len()],
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::splat(0.5),
            Quat::IDENTITY,
            Vec3::new(
                (i % 10) as f32 * 0.6 - 2.7,
                ((i / 10) % 5) as f32 * 0.6 - 1.2,
                -((i / 50) as f32) * 5.0,
            ),
        ),
    };
    let front = |i: usize| SceneInstance {
        mesh,
        material: palette[(i + 3) % palette.len()],
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::splat(0.3),
            Quat::IDENTITY,
            Vec3::new((i % 5) as f32 * 0.8 - 1.6, (i / 5) as f32 * 0.8 - 1.2, 8.0),
        ),
    };
    let all: Vec<SceneInstance> = core::iter::once(wall)
        .chain((0..BEHIND).map(behind))
        .chain((0..IN_FRONT).map(front))
        .collect();
    let list: Vec<DrawItem> = all
        .iter()
        .map(|i| DrawItem {
            mesh: i.mesh,
            material: i.material,
            world_from_object: i.world_from_object,
        })
        .collect();
    let reference = bed.frame(Draw::List(&list), camera, None, None).image;

    // Spawned in a shuffled order (a fixed permutation), half in each of two frames.
    let mut order: Vec<usize> = (0..all.len()).collect();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for i in (1..order.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        order.swap(i, (seed % (i as u64 + 1)) as usize);
    }
    let mut scene = bed.scene(64);
    let occlusion = Occlusion::new(&bed.device, &mut bed.allocator, 1024).unwrap();
    let mut ids = vec![None; all.len()];
    for &k in &order[..all.len() / 2] {
        ids[k] = Some(scene.insert(all[k]));
    }
    bed.frame(Draw::Scene(&mut scene), camera, Some(&occlusion), None);
    for &k in &order[all.len() / 2..] {
        ids[k] = Some(scene.insert(all[k]));
    }
    let in_frustum = all.len() as u32;
    let visible = (1 + IN_FRONT) as u32;
    let mut counts = Vec::new();
    for _ in 0..3 {
        let shot = bed.frame(Draw::Scene(&mut scene), camera, Some(&occlusion), None);
        assert!(shot.occlusion);
        assert_eq!(shot.batches, 1);
        assert_eq!(
            differing(&shot.image, &reference),
            0,
            "the image is unchanged"
        );
        counts.push((shot.drawn(0, 1), shot.drawn(1, 1)));
    }
    println!("early/late per frame: {counts:?}");
    // The second half was new in the first of these frames: tested late. Instances
    // visible in the frame before (behind the wall, which was not spawned yet) are drawn
    // early for nothing: a wrong guess only costs time.
    assert!(counts[0].0 + counts[0].1 >= visible);
    assert_eq!(
        counts[1],
        (visible, 0),
        "only the wall and what is in front of it"
    );
    assert_eq!(counts[2], (visible, 0));

    // Churn: 50 hidden cubes removed and spawned again (in reverse, into reused slots),
    // the wall moved to the last slot. Nothing visible goes missing, and the hidden
    // cubes stay culled.
    for k in (1..=50).rev() {
        scene.remove(ids[k].unwrap());
    }
    scene.remove(ids[0].unwrap());
    for k in 1..=50 {
        ids[k] = Some(scene.insert(all[k]));
    }
    ids[0] = Some(scene.insert(wall));
    // The wall's new slot was not visible last frame, so the first frame draws it late,
    // after a pyramid without it: everything behind counts as visible once (a wrong
    // guess, drawn early in the next frame), and the frame after is back to the wall and
    // what is in front of it.
    for frame in 0..3 {
        let shot = bed.frame(Draw::Scene(&mut scene), camera, Some(&occlusion), None);
        assert_eq!(
            differing(&shot.image, &reference),
            0,
            "after churn, frame {frame}"
        );
        let (early, late) = (shot.drawn(0, 1), shot.drawn(1, 1));
        println!("after churn, frame {frame}: early {early}, late {late}");
        assert!((visible..=in_frustum).contains(&(early + late)));
        if frame == 2 {
            assert_eq!((early, late), (visible, 0));
        }
    }
    bed.finish(vec![scene], (vec![occlusion], Vec::new()));
}

/// Debug: the same frame rendered again and again gives the same bytes.
#[test]
#[ignore = "stress"]
fn repeated_frames_are_identical() {
    let Some(mut bed) = Bed::new(128) else {
        return;
    };
    let cube_a = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let cube_b = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let mut palette = Vec::new();
    for i in 0..9 {
        let c = [(i % 3) as f32 / 3.0 + 0.2, (i % 4) as f32 / 4.0 + 0.1, 0.5];
        palette.push(bed.materials.add(MaterialDesc::solid(c, 0.0, 0.5)));
    }
    let mode = std::env::var("STRESS_MODE").unwrap_or_default();
    let n: usize = std::env::var("STRESS_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let camera = perspective_camera(Vec3::new(2.0, 7.0, 9.0));
    let mut items = mixed_items([cube_a, cube_b], &palette, 0.25);
    if let Some(count) = std::env::var("STRESS_ITEMS").ok().and_then(|v| v.parse::<usize>().ok()) {
        items.truncate(count);
    }
    bed.renderer.set_frustum_culling(!mode.contains("noculling"));
    let other = mixed_items([cube_a, cube_b], &palette, 0.5);
    let temporal = Temporal::new(&bed.device, &mut bed.allocator, 1024).unwrap();
    temporal.set_jitter_phases(0);
    let use_temporal = mode.contains("temporal");
    bed.renderer.set_depth_prepass(mode.contains("prepass"));
    let first = bed.frame(
        Draw::List(&items),
        camera,
        None,
        use_temporal.then_some(&temporal),
    );
    let reference = bed.frame(
        Draw::List(&items),
        camera,
        None,
        use_temporal.then_some(&temporal),
    );
    let _ = first;
    if let Ok(dir) = std::env::var("STRESS_PROBE_PIPE") {
        let pl = make_probe_pipeline_with(
            bed.device.raw(),
            &dir,
            std::env::var("STRESS_BINDLESS_LAYOUT").is_ok().then(|| bed.bindless.pipeline_layout()),
        );
        if let Ok(path) = std::env::var("STRESS_ISA") {
            dump_isa(&bed.instance, &bed.device, pl.0, &path);
            return;
        }
        *engine_render::forward::DEBUG_PROBE.lock().unwrap() = Some(pl);
    }
    let mut hashes = Vec::with_capacity(n);
    let st = bed.graph.stats();
    eprintln!("STRESS graph: {st:?}");
    let mut bad = 0;
    for k in 0..n {
        if mode.contains("same") {
            let _ = bed.frame(
                Draw::List(&other),
                camera,
                None,
                use_temporal.then_some(&temporal),
            );
        } else if mode.contains("alternate") {
            let _ = bed.frame(
                Draw::List(&items[..30]),
                camera,
                None,
                use_temporal.then_some(&temporal),
            );
            if k < 3 {
                eprintln!(
                    "STRESS created after other: {}",
                    bed.graph.stats().created_objects
                );
            }
        }
        let shot = bed.frame(
            Draw::List(&items),
            camera,
            None,
            use_temporal.then_some(&temporal),
        );
        if k < 3 {
            eprintln!("STRESS created: {}", bed.graph.stats().created_objects);
        }
        hashes.push({
            let mut h: u64 = 0xcbf29ce484222325;
            for &b in &shot.image {
                h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
            }
            h
        });
        let d = differing(&reference.image, &shot.image);
        let dd = differing(&reference.depth, &shot.depth);
        for (i, name) in ["visible", "shadow commands", "casters"].iter().enumerate() {
            let r = &reference.debug[i * DEBUG_BYTES as usize..(i + 1) * DEBUG_BYTES as usize];
            let g = &shot.debug[i * DEBUG_BYTES as usize..(i + 1) * DEBUG_BYTES as usize];
            let wrong: Vec<usize> = r.chunks_exact(4).zip(g.chunks_exact(4)).enumerate().filter(|(_, (a, b))| a != b).map(|(i, _)| i).collect();
            if !wrong.is_empty() {
                let w = |b: &[u8], i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
                eprintln!(
                    "STRESS frame {k}: {name} differs in {} words, first {:?}",
                    wrong.len(),
                    wrong.iter().take(6).map(|&i| (i, w(r, i), w(g, i))).collect::<Vec<_>>()
                );
            }
        }
        if dd != 0 {
            eprintln!("STRESS frame {k}: {dd} depth texels differ");
            let mut m = 0;
            for (i, (a, b)) in reference
                .depth
                .chunks_exact(4)
                .zip(shot.depth.chunks_exact(4))
                .enumerate()
            {
                if a != b && m < 12 {
                    let (x, y) = (
                        u32::from_le_bytes(a.try_into().unwrap()),
                        u32::from_le_bytes(b.try_into().unwrap()),
                    );
                    eprintln!(
                        "STRESS   depth px ({}, {}): ref {x:08x} {} got {y:08x} {} xor {:08x}",
                        i % 128,
                        i / 128,
                        f32::from_bits(x),
                        f32::from_bits(y),
                        x ^ y
                    );
                    m += 1;
                }
            }
        }
        if d != 0 {
            bad += 1;
            let mut m = 0;
            for (i, (a, b)) in reference
                .image
                .chunks_exact(4)
                .zip(shot.image.chunks_exact(4))
                .enumerate()
            {
                if a != b && m < 8 {
                    let dr =
                        f32::from_le_bytes(reference.depth[i * 4..i * 4 + 4].try_into().unwrap());
                    let dg = f32::from_le_bytes(shot.depth[i * 4..i * 4 + 4].try_into().unwrap());
                    eprintln!(
                        "STRESS frame {k} px ({}, {}): ref {:?} got {:?} depth ref {dr} got {dg}",
                        i % 128,
                        i / 128,
                        a,
                        b
                    );
                    m += 1;
                }
            }
            if let Ok(dir) = std::env::var("ENGINE_DIFF_DIR") {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis();
                std::fs::write(format!("{dir}/{stamp}_refdepth.bin"), &reference.depth).unwrap();
                std::fs::write(format!("{dir}/{stamp}_gotdepth.bin"), &shot.depth).unwrap();
                for (name, img) in [("ref", &reference.image), ("got", &shot.image)] {
                    let png = engine_render::png::encode_png(
                        128,
                        128,
                        engine_render::png::PngFormat::Rgba,
                        img,
                    )
                    .unwrap();
                    std::fs::write(format!("{dir}/{stamp}_{name}.png"), png).unwrap();
                }
            }
        }
    }
    let mut counts: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    for h in &hashes {
        *counts.entry(*h).or_default() += 1;
    }
    let most = counts.values().copied().max().unwrap_or(0);
    eprintln!("STRESS {mode}: {} of {n} frames off the majority ({} distinct images)", n - most, counts.len());
    eprintln!("STRESS {mode}: {bad} of {n} frames differ");
    bed.finish(Vec::new(), (Vec::new(), vec![temporal]));
    assert_eq!(bad, 0);
}

/// Debug: buffer copies through device memory return what the host wrote.
#[test]
#[ignore = "stress"]
fn copies_are_exact() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    let mut allocator = Allocator::new(&device);
    let d = device.raw();
    let size: u64 = 16 << 20;
    let n: usize = std::env::var("STRESS_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    let mk = |allocator: &mut Allocator, usage: vk::BufferUsageFlags, loc: MemoryLocation| {
        allocator
            .create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage), loc)
            .unwrap()
    };
    let (u, mut um) = mk(&mut allocator, vk::BufferUsageFlags::TRANSFER_SRC, MemoryLocation::Upload);
    let (a, am) = mk(&mut allocator, vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST, MemoryLocation::Device);
    let (b, bm) = mk(&mut allocator, vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST, MemoryLocation::Device);
    let (r, rm) = mk(&mut allocator, vk::BufferUsageFlags::TRANSFER_DST, MemoryLocation::Readback);
    // SAFETY: test setup.
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
            .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1))
            .unwrap()[0];
        (pool, cmd)
    };
    let words = (size / 4) as usize;
    let mut bad = 0;
    for k in 0..n {
        let mut x = (k as u32).wrapping_mul(0x9E37_79B9) | 1;
        let pattern: Vec<u32> = (0..words)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x
            })
            .collect();
        um.mapped_slice_mut().unwrap().copy_from_slice(bytemuck_cast(&pattern));
        let barrier = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)];
        let host = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::HOST)
            .dst_access_mask(vk::AccessFlags2::HOST_READ)];
        let region = [vk::BufferCopy { src_offset: 0, dst_offset: 0, size }];
        // SAFETY: one-shot recording and submission, then wait.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()).unwrap();
            d.cmd_copy_buffer(cmd, u, a, &region);
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&barrier));
            d.cmd_copy_buffer(cmd, a, b, &region);
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&barrier));
            d.cmd_copy_buffer(cmd, b, r, &region);
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&host));
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(device.graphics.raw, &[vk::SubmitInfo2::default().command_buffer_infos(&infos)], vk::Fence::null())
                .unwrap();
            device.wait_idle().unwrap();
        }
        let ptr = rm.mapped_ptr().unwrap();
        // SAFETY: idle GPU; the mapping covers the buffer.
        let got = unsafe { core::slice::from_raw_parts(ptr.as_ptr().cast::<u32>(), words) };
        let wrong = got.iter().zip(&pattern).filter(|(a, b)| a != b).count();
        if wrong > 0 {
            bad += 1;
            let first = got.iter().zip(&pattern).position(|(a, b)| a != b).unwrap();
            eprintln!("STRESS copy {k}: {wrong} words wrong, first at {first}: {:08x} vs {:08x}", got[first], pattern[first]);
        }
    }
    eprintln!("STRESS copies: {bad} of {n} wrong");
    // SAFETY: idle.
    unsafe {
        d.destroy_command_pool(pool, None);
        allocator.destroy_buffer(u, um);
        allocator.destroy_buffer(a, am);
        allocator.destroy_buffer(b, bm);
        allocator.destroy_buffer(r, rm);
    }
    drop(allocator);
    drop(device);
    drop(instance);
    assert_eq!(bad, 0);
}

fn bytemuck_cast(v: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding; u8 alignment is 1.
    unsafe { core::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len() * 4) }
}


/// Debug: a minimal raw-Vulkan rasterization probe (no engine renderer): 60 cubes placed
/// by arithmetic, depth tested, read back; every frame must be identical.
#[test]
#[ignore = "stress"]
fn raster_probe_is_deterministic() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    let dir = std::env::var("PROBE_SHADERS").unwrap();
    let mut allocator = Allocator::new(&device);
    let d = device.raw();
    let n: usize = std::env::var("STRESS_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    let mode: u32 = std::env::var("PROBE_MODE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let size = 128u32;
    let repeat: u32 = std::env::var("PROBE_REPEAT").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let image = |allocator: &mut Allocator, format: vk::Format, usage: vk::ImageUsageFlags| {
        allocator
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D { width: size, height: size, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage | vk::ImageUsageFlags::TRANSFER_SRC),
                MemoryLocation::Device,
            )
            .unwrap()
    };
    let (color, color_mem) = image(&mut allocator, COLOR, vk::ImageUsageFlags::COLOR_ATTACHMENT);
    let (depth, depth_mem) = image(&mut allocator, DEPTH, vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT);
    let view = |image, format, aspect| {
        // SAFETY: valid image.
        unsafe {
            d.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: aspect,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        }
        .unwrap()
    };
    let color_view = view(color, COLOR, vk::ImageAspectFlags::COLOR);
    let depth_view = view(depth, DEPTH, vk::ImageAspectFlags::DEPTH);
    let image_bytes = u64::from(size * size * 4);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default().size(image_bytes * 2).usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
    // Positions of the 36 vertices, for mode 1.
    let corners = [
        [-0.5f32, -0.5, -0.5], [0.5, -0.5, -0.5], [0.5, 0.5, -0.5], [-0.5, 0.5, -0.5],
        [-0.5, -0.5, 0.5], [0.5, -0.5, 0.5], [0.5, 0.5, 0.5], [-0.5, 0.5, 0.5],
    ];
    let indices = [0usize, 2, 1, 0, 3, 2, 4, 5, 6, 4, 6, 7, 0, 1, 5, 0, 5, 4, 3, 6, 2, 3, 7, 6, 0, 4, 7, 0, 7, 3, 1, 2, 6, 1, 6, 5];
    let positions: Vec<u32> = indices.iter().flat_map(|&i| corners[i]).map(f32::to_bits).collect();
    let (pbuf, mut pmem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(positions.len() as u64 * 4)
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS),
            MemoryLocation::Dynamic,
        )
        .unwrap();
    pmem.mapped_slice_mut().unwrap()[..positions.len() * 4].copy_from_slice(bytemuck_cast(&positions));
    let corner_bits: Vec<u32> = corners.iter().flatten().map(|f| f.to_bits()).collect();
    let mk_pos = |allocator: &mut Allocator, data: &[u32]| {
        let (b, mut m) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(data.len() as u64 * 4)
                    .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS),
                MemoryLocation::Dynamic,
            )
            .unwrap();
        m.mapped_slice_mut().unwrap()[..data.len() * 4].copy_from_slice(bytemuck_cast(data));
        // SAFETY: valid buffer with device address usage.
        let a = unsafe { d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(b)) };
        (b, m, a)
    };
    let (real_buf, real_mem, real_addr) = mk_pos(&mut allocator, &corner_bits);
    let (decoy_buf, decoy_mem, decoy_addr) = mk_pos(&mut allocator, &vec![0u32; 24]);
    let push_of = |addr: u64, mode: u32| {
        let mut push = [0u8; 112];
        push[..8].copy_from_slice(&addr.to_le_bytes());
        push[8..12].copy_from_slice(&mode.to_le_bytes());
        push[56..64].copy_from_slice(&addr.to_le_bytes());
        push
    };
    let (ibuf, mut imem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default().size(36 * 4).usage(vk::BufferUsageFlags::INDEX_BUFFER),
            MemoryLocation::Dynamic,
        )
        .unwrap();
    let idx: Vec<u32> = indices.iter().map(|&i| i as u32).collect();
    imem.mapped_slice_mut().unwrap()[..36 * 4].copy_from_slice(bytemuck_cast(&idx));
    // SAFETY: valid buffer with device address usage.
    let paddr = unsafe { d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(pbuf)) };
    let module = |name: &str| {
        let bytes = std::fs::read(format!("{dir}/raster.{name}.spv")).unwrap();
        let words: Vec<u32> = bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        // SAFETY: valid SPIR-V.
        unsafe { d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None) }.unwrap()
    };
    let vs = module("rasterVertex");
    let fs = module("rasterFragment");
    let ranges = [vk::PushConstantRange { stage_flags: vk::ShaderStageFlags::ALL_GRAPHICS, offset: 0, size: 112 }];
    // SAFETY: plain object creation.
    let layout = unsafe { d.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&ranges), None) }.unwrap();
    let stages = [
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"rasterVertex"),
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"rasterFragment"),
    ];
    let formats = [COLOR];
    let mut rendering = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats).depth_attachment_format(DEPTH);
    let vi = vk::PipelineVertexInputStateCreateInfo::default();
    let ia = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let vp = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
    let cull = std::env::var("PROBE_CULL").unwrap_or_default();
    let rs = vk::PipelineRasterizationStateCreateInfo::default()
        .cull_mode(if cull == "static" { vk::CullModeFlags::BACK } else { vk::CullModeFlags::NONE })
        .front_face(vk::FrontFace::CLOCKWISE)
        .line_width(1.0);
    let ms = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let ds = vk::PipelineDepthStencilStateCreateInfo::default().depth_test_enable(true).depth_write_enable(true).depth_compare_op(vk::CompareOp::LESS);
    let blend = [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
    let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend);
    let dyns = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR, vk::DynamicState::CULL_MODE, vk::DynamicState::FRONT_FACE];
    let dyn_cull = cull == "dynamic";
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(if dyn_cull { &dyns[..] } else { &dyns[..2] });
    let info = vk::GraphicsPipelineCreateInfo::default()
        .push_next(&mut rendering)
        .stages(&stages)
        .vertex_input_state(&vi)
        .input_assembly_state(&ia)
        .viewport_state(&vp)
        .rasterization_state(&rs)
        .multisample_state(&ms)
        .depth_stencil_state(&ds)
        .color_blend_state(&cb)
        .dynamic_state(&dynamic)
        .layout(layout);
    // SAFETY: valid create info.
    let pipeline = unsafe { d.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }.unwrap()[0];
    let (pipeline, layout) = if std::env::var("STRESS_PROBE_FILE").is_ok() {
        let pl = make_probe_pipeline(d, &dir);
        if let Ok(path) = std::env::var("STRESS_ISA") {
            dump_isa(&instance, &device, pl.0, &path);
        }
        pl
    } else {
        (pipeline, layout)
    };
    // SAFETY: test setup.
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
            .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1))
            .unwrap()[0];
        (pool, cmd)
    };
    let range = |aspect| vk::ImageSubresourceRange { aspect_mask: aspect, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
    let mut hashes: Vec<u64> = Vec::new();
    let use_graph = std::env::var("PROBE_GRAPH").is_ok();
    let mut saved = 0;
    let mut graph = RenderGraph::new(&device, 1);
    for _ in 0..n {
        if use_graph {
            // SAFETY: one-shot recording and submission, then wait.
            unsafe {
                d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).unwrap();
                d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()).unwrap();
            }
            {
                // SAFETY: nothing in flight.
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                let gc = fg.create_texture("color", TextureDesc::new_2d(COLOR, size, size));
                let gd = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, size, size));
                let out = fg.import_buffer(
                    "readback",
                    ImportedBuffer {
                        buffer: readback,
                        size: image_bytes * 2,
                        address: 0,
                        initial: BufferState::idle(),
                        final_state: Some(BufferState::host_read()),
                    },
                );
                fg.add_pass("probe")
                    .color(gc, engine_render::graph::LoadOp::Clear([0.0, 0.0, 0.25, 1.0]), engine_render::graph::StoreOp::Store)
                    .depth(gd, engine_render::graph::LoadOp::Clear([1.0, 0.0, 0.0, 0.0]), engine_render::graph::StoreOp::Store)
                    .execute(move |ctx| {
                        let area = ctx.render_area();
                        // SAFETY: recording inside the pass's rendering scope.
                        unsafe {
                            ctx.device.cmd_bind_pipeline(ctx.cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
                            ctx.device.cmd_set_viewport(ctx.cmd, 0, &[vk::Viewport { x: 0.0, y: 0.0, width: size as f32, height: size as f32, min_depth: 0.0, max_depth: 1.0 }]);
                            ctx.device.cmd_set_scissor(ctx.cmd, 0, &[area]);
            if dyn_cull {
                ctx.device.cmd_set_cull_mode(ctx.cmd, vk::CullModeFlags::BACK);
                ctx.device.cmd_set_front_face(ctx.cmd, vk::FrontFace::CLOCKWISE);
            }
                            let mut push = [0u8; 16];
                            push[..8].copy_from_slice(&paddr.to_le_bytes());
                            push[8..12].copy_from_slice(&mode.to_le_bytes());
                            ctx.device.cmd_push_constants(ctx.cmd, layout, vk::ShaderStageFlags::ALL_GRAPHICS, 0, &push_of(paddr, mode));
                            if mode >= 2 {
                                ctx.device.cmd_bind_index_buffer(ctx.cmd, ibuf, 0, vk::IndexType::UINT32);
                            }
                            for _ in 0..repeat {
                                if mode >= 2 {
                                    if mode == 5 || mode == 6 {
                    for i in 0..60 {
                        ctx.device.cmd_push_constants(ctx.cmd, layout, vk::ShaderStageFlags::ALL_GRAPHICS, 0, &push_of(decoy_addr, mode));
                        ctx.device.cmd_draw_indexed(ctx.cmd, 36, 1, 0, 0, i);
                        ctx.device.cmd_push_constants(ctx.cmd, layout, vk::ShaderStageFlags::ALL_GRAPHICS, 0, &push_of(real_addr, mode));
                        ctx.device.cmd_draw_indexed(ctx.cmd, 36, 1, 0, 0, i);
                    }
                } else if mode == 3 {
                    for i in 0..60 {
                        ctx.device.cmd_draw_indexed(ctx.cmd, 36, 1, 0, 0, i);
                    }
                } else {
                    ctx.device.cmd_draw_indexed(ctx.cmd, 36, 60, 0, 0, 0);
                }
                                } else {
                                    ctx.device.cmd_draw(ctx.cmd, 36, 60, 0, 0);
                                }
                            }
                        }
                    });
                fg.add_pass("read back")
                    .read(gc, Access::TransferRead)
                    .read(gd, Access::TransferRead)
                    .write(out, Access::TransferWrite)
                    .execute(move |ctx| {
                        let region = |offset, aspect| vk::BufferImageCopy {
                            buffer_offset: offset,
                            image_subresource: vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: 0, base_array_layer: 0, layer_count: 1 },
                            image_extent: vk::Extent3D { width: size, height: size, depth: 1 },
                            ..Default::default()
                        };
                        // SAFETY: recording; layouts arranged by the graph.
                        unsafe {
                            ctx.device.cmd_copy_image_to_buffer(ctx.cmd, ctx.image(gc), vk::ImageLayout::TRANSFER_SRC_OPTIMAL, ctx.buffer(out), &[region(0, vk::ImageAspectFlags::COLOR)]);
                            ctx.device.cmd_copy_image_to_buffer(ctx.cmd, ctx.image(gd), vk::ImageLayout::TRANSFER_SRC_OPTIMAL, ctx.buffer(out), &[region(image_bytes, vk::ImageAspectFlags::DEPTH)]);
                        }
                    });
                fg.execute(&mut allocator).unwrap();
            }
            // SAFETY: as above.
            unsafe {
                d.end_command_buffer(cmd).unwrap();
                let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
                d.queue_submit2(device.graphics.raw, &[vk::SubmitInfo2::default().command_buffer_infos(&infos)], vk::Fence::null()).unwrap();
                device.wait_idle().unwrap();
            }
        } else {
        // SAFETY: one-shot recording and submission, then wait.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()).unwrap();
            let to_attach = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                    .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::ATTACHMENT_OPTIMAL)
                    .image(color)
                    .subresource_range(range(vk::ImageAspectFlags::COLOR)),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_stage_mask(vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS)
                    .dst_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::ATTACHMENT_OPTIMAL)
                    .image(depth)
                    .subresource_range(range(vk::ImageAspectFlags::DEPTH)),
            ];
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&to_attach));
            let colors = [vk::RenderingAttachmentInfo::default()
                .image_view(color_view)
                .image_layout(vk::ImageLayout::ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.25, 1.0] } })];
            let depth_info = vk::RenderingAttachmentInfo::default()
                .image_view(depth_view)
                .image_layout(vk::ImageLayout::ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue { depth_stencil: vk::ClearDepthStencilValue { depth: 1.0, stencil: 0 } });
            let area = vk::Rect2D { offset: vk::Offset2D::default(), extent: vk::Extent2D { width: size, height: size } };
            d.cmd_begin_rendering(
                cmd,
                &vk::RenderingInfo::default().render_area(area).layer_count(1).color_attachments(&colors).depth_attachment(&depth_info),
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
            d.cmd_set_viewport(cmd, 0, &[vk::Viewport { x: 0.0, y: 0.0, width: size as f32, height: size as f32, min_depth: 0.0, max_depth: 1.0 }]);
            d.cmd_set_scissor(cmd, 0, &[area]);
            if dyn_cull {
                d.cmd_set_cull_mode(cmd, vk::CullModeFlags::BACK);
                d.cmd_set_front_face(cmd, vk::FrontFace::CLOCKWISE);
            }
            let mut push = [0u8; 16];
            push[..8].copy_from_slice(&paddr.to_le_bytes());
            push[8..12].copy_from_slice(&mode.to_le_bytes());
            d.cmd_push_constants(cmd, layout, vk::ShaderStageFlags::ALL_GRAPHICS, 0, &push_of(paddr, mode));
            if mode >= 2 {
                d.cmd_bind_index_buffer(cmd, ibuf, 0, vk::IndexType::UINT32);
            }
            for _ in 0..repeat {
                if mode >= 2 {
                    if mode == 5 || mode == 6 {
                    for i in 0..60 {
                        d.cmd_push_constants(cmd, layout, vk::ShaderStageFlags::ALL_GRAPHICS, 0, &push_of(decoy_addr, mode));
                        d.cmd_draw_indexed(cmd, 36, 1, 0, 0, i);
                        d.cmd_push_constants(cmd, layout, vk::ShaderStageFlags::ALL_GRAPHICS, 0, &push_of(real_addr, mode));
                        d.cmd_draw_indexed(cmd, 36, 1, 0, 0, i);
                    }
                } else if mode == 3 {
                    for i in 0..60 {
                        d.cmd_draw_indexed(cmd, 36, 1, 0, 0, i);
                    }
                } else {
                    d.cmd_draw_indexed(cmd, 36, 60, 0, 0, 0);
                }
                } else {
                    d.cmd_draw(cmd, 36, 60, 0, 0);
                }
            }
            d.cmd_end_rendering(cmd);
            let to_copy = [
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                    .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::ATTACHMENT_OPTIMAL)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .image(color)
                    .subresource_range(range(vk::ImageAspectFlags::COLOR)),
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS)
                    .src_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::ATTACHMENT_OPTIMAL)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .image(depth)
                    .subresource_range(range(vk::ImageAspectFlags::DEPTH)),
            ];
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&to_copy));
            let region = |offset, aspect| vk::BufferImageCopy {
                buffer_offset: offset,
                image_subresource: vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: 0, base_array_layer: 0, layer_count: 1 },
                image_extent: vk::Extent3D { width: size, height: size, depth: 1 },
                ..Default::default()
            };
            d.cmd_copy_image_to_buffer(cmd, color, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, readback, &[region(0, vk::ImageAspectFlags::COLOR)]);
            d.cmd_copy_image_to_buffer(cmd, depth, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, readback, &[region(image_bytes, vk::ImageAspectFlags::DEPTH)]);
            let host = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ)];
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&host));
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(device.graphics.raw, &[vk::SubmitInfo2::default().command_buffer_infos(&infos)], vk::Fence::null()).unwrap();
            device.wait_idle().unwrap();
        }
        }
        let ptr = readback_mem.mapped_ptr().unwrap();
        // SAFETY: idle; in bounds.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), (image_bytes * 2) as usize) };
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
        }
        hashes.push(h);
        if hashes.len() > 1 && h != hashes[0] && saved < 12 {
            if let Ok(dir) = std::env::var("ENGINE_DIFF_DIR") {
                std::fs::write(format!("{dir}/probe_diff_{saved}.bin"), bytes).unwrap();
                saved += 1;
            }
        }
        if hashes.len() == 1 {
            if let Ok(dir) = std::env::var("ENGINE_DIFF_DIR") {
                std::fs::write(format!("{dir}/probe_first.bin"), bytes).unwrap();
            }
            if let Ok(dir) = std::env::var("ENGINE_DIFF_DIR") {
                let png = engine_render::png::encode_png(size, size, engine_render::png::PngFormat::Rgba, &bytes[..image_bytes as usize]).unwrap();
                std::fs::write(format!("{dir}/probe_first.png"), png).unwrap();
            }
        }
    }
    let mut counts: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    for h in &hashes {
        *counts.entry(*h).or_default() += 1;
    }
    let most = counts.values().copied().max().unwrap_or(0);
    eprintln!("STRESS probe mode {mode}: {} of {n} frames off the majority ({} distinct images)", n - most, counts.len());
    // SAFETY: idle.
    unsafe {
        graph.destroy(&mut allocator, None);
        d.destroy_pipeline(pipeline, None);
        d.destroy_pipeline_layout(layout, None);
        d.destroy_shader_module(vs, None);
        d.destroy_shader_module(fs, None);
        d.destroy_command_pool(pool, None);
        d.destroy_image_view(color_view, None);
        d.destroy_image_view(depth_view, None);
        allocator.destroy_image(color, color_mem);
        allocator.destroy_image(depth, depth_mem);
        allocator.destroy_buffer(readback, readback_mem);
        allocator.destroy_buffer(pbuf, pmem);
        allocator.destroy_buffer(ibuf, imem);
        allocator.destroy_buffer(real_buf, real_mem);
        allocator.destroy_buffer(decoy_buf, decoy_mem);
    }
    drop(allocator);
    drop(device);
    drop(instance);
    assert_eq!(n - most, 0);
}

fn dump_isa(instance: &VkInstance, device: &Device, pipeline: vk::Pipeline, path: &str) {
    let loader = engine_rhi::ash::khr::pipeline_executable_properties::Device::new(instance.raw(), device.raw());
    let info = vk::PipelineInfoKHR::default().pipeline(pipeline);
    // SAFETY: valid pipeline created with capture flags.
    let execs = unsafe { loader.get_pipeline_executable_properties(&info) }.unwrap();
    let mut out = String::new();
    for (i, e) in execs.iter().enumerate() {
        out += &format!("=== executable {i}: {:?}
", e.name_as_c_str());
        let ei = vk::PipelineExecutableInfoKHR::default().pipeline(pipeline).executable_index(i as u32);
        // SAFETY: as above.
        let n = unsafe { (loader.fp().get_pipeline_executable_internal_representations_khr)(device.raw().handle(), &ei, &mut 0u32 as *mut u32, core::ptr::null_mut()) };
        let _ = n;
        let mut count = 0u32;
        // SAFETY: as above.
        unsafe { (loader.fp().get_pipeline_executable_internal_representations_khr)(device.raw().handle(), &ei, &mut count, core::ptr::null_mut()) };
        let mut reps = vec![vk::PipelineExecutableInternalRepresentationKHR::default(); count as usize];
        // SAFETY: as above.
        unsafe { (loader.fp().get_pipeline_executable_internal_representations_khr)(device.raw().handle(), &ei, &mut count, reps.as_mut_ptr()) };
        let mut bufs: Vec<Vec<u8>> = reps.iter().map(|r| vec![0u8; r.data_size]).collect();
        for (r, b) in reps.iter_mut().zip(bufs.iter_mut()) {
            r.p_data = b.as_mut_ptr().cast();
        }
        // SAFETY: as above.
        unsafe { (loader.fp().get_pipeline_executable_internal_representations_khr)(device.raw().handle(), &ei, &mut count, reps.as_mut_ptr()) };
        for (k, (r, b)) in reps.iter().zip(&bufs).enumerate() {
            out += &format!("--- {:?}
{}
", r.name_as_c_str(), String::from_utf8_lossy(b));
            if r.is_text == vk::FALSE {
                std::fs::write(format!("{path}.{i}.{k}.bin"), b).unwrap();
            }
        }
    }
    std::fs::write(path, out).unwrap();
}

fn make_probe_pipeline(d: &engine_rhi::ash::Device, dir: &str) -> (vk::Pipeline, vk::PipelineLayout) {
    make_probe_pipeline_with(d, dir, None)
}

fn make_probe_pipeline_with(d: &engine_rhi::ash::Device, dir: &str, given_layout: Option<vk::PipelineLayout>) -> (vk::Pipeline, vk::PipelineLayout) {
    let module = |name: &str| {
        let bytes = std::fs::read(format!("{dir}/{name}.spv")).unwrap();
        let words: Vec<u32> = bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        // SAFETY: valid SPIR-V.
        unsafe { d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None) }.unwrap()
    };
    let engine_push = std::env::var("STRESS_PROBE_FILE").is_ok();
    let (vs, fs, vname, fname) = if engine_push {
        {
            let e = std::env::var("STRESS_PROBE_ENTRY").unwrap_or_default();
            let name: &'static str = Box::leak(format!("probeVertex{e}").into_boxed_str());
            let cname: &'static std::ffi::CStr = Box::leak(std::ffi::CString::new(name).unwrap().into_boxed_c_str());
            if let Ok(vsname) = std::env::var("STRESS_ENGINE_VS") {
                let cv: &'static std::ffi::CStr = Box::leak(std::ffi::CString::new(vsname.clone()).unwrap().into_boxed_c_str());
                let fsn = std::env::var("STRESS_ENGINE_FS").unwrap_or_else(|_| "J".into());
                let fname: &'static str = Box::leak(format!("probeFragment{fsn}").into_boxed_str());
                let cf: &'static std::ffi::CStr = Box::leak(std::ffi::CString::new(fname).unwrap().into_boxed_c_str());
                (module(&format!("engine.{vsname}")), module(&format!("probe2.{fname}")), cv, cf)
            } else if let Ok(fsn) = std::env::var("STRESS_PROBE_FS_NAME") {
                let fname: &'static str = Box::leak(format!("probeFragment{fsn}").into_boxed_str());
                let cf: &'static std::ffi::CStr = Box::leak(std::ffi::CString::new(fname).unwrap().into_boxed_c_str());
                (module(&format!("probe2.{name}")), module(&format!("probe2.{fname}")), cname, cf)
            } else if std::env::var("STRESS_PROBE_FS").is_ok() {
                (module(&format!("probe2.{name}")), module("probe2.probeFragmentI"), cname, c"probeFragmentI")
            } else {
                (module(&format!("probe2.{name}")), module("probe2.probeFragment"), cname, c"probeFragment")
            }
        }
    } else {
        (module("raster.rasterVertex"), module("raster.rasterFragment"), c"rasterVertex", c"rasterFragment")
    };
    let ranges = [vk::PushConstantRange { stage_flags: vk::ShaderStageFlags::ALL_GRAPHICS, offset: 0, size: 112 }];
    // SAFETY: plain object creation.
    let layout = given_layout.unwrap_or_else(|| unsafe { d.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&ranges), None) }.unwrap());
    let stages = [
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(vname),
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(fname),
    ];
    let formats = [COLOR];
    let mut rendering = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats).depth_attachment_format(DEPTH);
    let vi = vk::PipelineVertexInputStateCreateInfo::default();
    let ia = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let vp = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
    let cull = std::env::var("PROBE_CULL").unwrap_or_default();
    let rs = vk::PipelineRasterizationStateCreateInfo::default()
        .cull_mode(if cull == "static" { vk::CullModeFlags::BACK } else { vk::CullModeFlags::NONE })
        .front_face(vk::FrontFace::CLOCKWISE)
        .line_width(1.0);
    let ms = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let ds = vk::PipelineDepthStencilStateCreateInfo::default().depth_test_enable(true).depth_write_enable(true).depth_compare_op(vk::CompareOp::LESS);
    let blend = [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
    let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend);
    let dyns = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR, vk::DynamicState::CULL_MODE, vk::DynamicState::FRONT_FACE];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(if cull == "dynamic" { &dyns[..] } else { &dyns[..2] });
    let info = vk::GraphicsPipelineCreateInfo::default()
        .push_next(&mut rendering)
        .stages(&stages)
        .vertex_input_state(&vi)
        .input_assembly_state(&ia)
        .viewport_state(&vp)
        .rasterization_state(&rs)
        .multisample_state(&ms)
        .depth_stencil_state(&ds)
        .color_blend_state(&cb)
        .dynamic_state(&dynamic)
        .layout(layout)
        .flags(if std::env::var("STRESS_ISA").is_ok() { vk::PipelineCreateFlags::CAPTURE_INTERNAL_REPRESENTATIONS_KHR } else { vk::PipelineCreateFlags::empty() });
    // SAFETY: valid create info.
    let pipeline = unsafe { d.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }.unwrap()[0];
    (pipeline, layout)
}

/// Debug: a compute kernel (integer and float chain per thread) gives the same results
/// every run.
#[test]
#[ignore = "stress"]
fn compute_is_deterministic() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    let dir = std::env::var("PROBE_SHADERS").unwrap();
    let mut allocator = Allocator::new(&device);
    let d = device.raw();
    let n: usize = std::env::var("STRESS_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    let threads: u32 = 1 << 18;
    let iterations: u32 = std::env::var("ALU_ITERATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(256);
    let bytes = u64::from(threads) * 4;
    let (out, out_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes)
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS | vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Device,
        )
        .unwrap();
    let (rb, rb_mem) = allocator
        .create_buffer(&vk::BufferCreateInfo::default().size(bytes).usage(vk::BufferUsageFlags::TRANSFER_DST), MemoryLocation::Readback)
        .unwrap();
    // SAFETY: valid buffer with device address usage.
    let addr = unsafe { d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(out)) };
    let lds = std::env::var("ALU_LDS").is_ok();
    let code = std::fs::read(if lds { format!("{dir}/lds.ldsMain.spv") } else { format!("{dir}/alu.aluMain.spv") }).unwrap();
    let words: Vec<u32> = code.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    // SAFETY: plain object creation.
    let (module, layout, pipeline, pool, cmd) = unsafe {
        let module = d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None).unwrap();
        let ranges = [vk::PushConstantRange { stage_flags: vk::ShaderStageFlags::COMPUTE, offset: 0, size: 16 }];
        let layout = d.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&ranges), None).unwrap();
        let stage = vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::COMPUTE).module(module).name(if lds { c"ldsMain" } else { c"aluMain" });
        let pipeline = d
            .create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(stage)
                    .layout(layout)
                    .flags(if std::env::var("STRESS_ISA").is_ok() { vk::PipelineCreateFlags::CAPTURE_INTERNAL_REPRESENTATIONS_KHR } else { vk::PipelineCreateFlags::empty() })],
                None,
            )
            .unwrap()[0];
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
            .unwrap();
        let cmd = d
            .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1))
            .unwrap()[0];
        (module, layout, pipeline, pool, cmd)
    };
    if let Ok(path) = std::env::var("STRESS_ISA") {
        dump_isa(&instance, &device, pipeline, &path);
    }
    // The LDS kernel's exact results, computed on the CPU.
    let reference: Option<Vec<u32>> = lds.then(|| {
        let mut out = vec![0u32; threads as usize];
        for g in 0..(threads / 256) as usize {
            let mut x: Vec<u32> = (0..256u32).map(|l| (g as u32 * 256 + l).wrapping_mul(2654435761).wrapping_add(1)).collect();
            for i in 0..iterations {
                let y: Vec<u32> = (0..256u32).map(|l| x[((l + 1 + i * 37) & 255) as usize]).collect();
                for l in 0..256 {
                    x[l] ^= y[l].wrapping_add(x[l] << 6).wrapping_add(x[l] >> 2);
                }
            }
            out[g * 256..(g + 1) * 256].copy_from_slice(&x);
        }
        out
    });
    let mut first: Option<Vec<u32>> = reference;
    let mut bad = 0;
    for k in 0..n {
        // SAFETY: one-shot recording and submission, then wait.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()).unwrap();
            d.cmd_fill_buffer(cmd, out, 0, vk::WHOLE_SIZE, 0);
            let b = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)];
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&b));
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
            let mut push = [0u8; 16];
            push[..8].copy_from_slice(&addr.to_le_bytes());
            push[8..12].copy_from_slice(&iterations.to_le_bytes());
            d.cmd_push_constants(cmd, layout, vk::ShaderStageFlags::COMPUTE, 0, &push);
            d.cmd_dispatch(cmd, threads / if lds { 256 } else { 64 }, 1, 1);
            let b = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)];
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&b));
            d.cmd_copy_buffer(cmd, out, rb, &[vk::BufferCopy { src_offset: 0, dst_offset: 0, size: bytes }]);
            let b = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ)];
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&b));
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(device.graphics.raw, &[vk::SubmitInfo2::default().command_buffer_infos(&infos)], vk::Fence::null()).unwrap();
            device.wait_idle().unwrap();
        }
        let ptr = rb_mem.mapped_ptr().unwrap();
        // SAFETY: idle; in bounds.
        let got = unsafe { core::slice::from_raw_parts(ptr.as_ptr().cast::<u32>(), threads as usize) };
        match &first {
            None => first = Some(got.to_vec()),
            Some(f) => {
                let wrong: Vec<usize> = f.iter().zip(got).enumerate().filter(|(_, (a, b))| a != b).map(|(i, _)| i).collect();
                if !wrong.is_empty() {
                    bad += 1;
                    let i = wrong[0];
                    eprintln!("ALU run {k}: {} threads differ, first {i}: {:08x} vs {:08x} (xor {:08x})", wrong.len(), f[i], got[i], f[i] ^ got[i]);
                    if iterations == 1 && lds {
                        // With one round, a thread's result is x ^ (y + (x << 6) + (x >> 2)) of
                        // the y it read: recover y and find which thread's initial value it is.
                        for &w in wrong.iter().take(8) {
                            let x = (w as u32).wrapping_mul(2654435761).wrapping_add(1);
                            let y = (got[w] ^ x).wrapping_sub(x << 6).wrapping_sub(x >> 2);
                            let src = (0..threads).find(|&t| t.wrapping_mul(2654435761).wrapping_add(1) == y);
                            eprintln!("  thread {w} (group {}, lane {}): read {y:08x}, from thread {:?} (expected {})", w / 256, w % 256, src.map(|t| (t, t / 256, t % 256)), (w / 256) * 256 + ((w % 256 + 1) & 255));
                        }
                    }
                }
            }
        }
    }
    eprintln!("STRESS alu: {bad} of {n} runs wrong");
    // SAFETY: idle.
    unsafe {
        d.destroy_pipeline(pipeline, None);
        d.destroy_pipeline_layout(layout, None);
        d.destroy_shader_module(module, None);
        d.destroy_command_pool(pool, None);
        allocator.destroy_buffer(out, out_mem);
        allocator.destroy_buffer(rb, rb_mem);
    }
    drop(allocator);
    drop(device);
    drop(instance);
    assert_eq!(bad, 0);
}
