//! The persistent GPU scene (ADR 0064), under the validation layer with synchronization
//! validation:
//! - a scene draws the same pixels and motion vectors as the same draw list, with every
//!   pipeline state (opaque, masked, blended, double-sided, mirrored) and two meshes
//!   interleaved, with and without occlusion culling;
//! - slots are stable, reused lowest first after removals, and removed instances vanish;
//!   frames without changes upload nothing;
//! - motion vectors of moved, still (settled), teleported, new and removed instances;
//! - occlusion culling keys off slots: instances spawned in a shuffled order, and
//!   removed and spawned again, are culled as well as a fixed draw list is;
//! - cached shadow cascades (ADR 0093) follow the scene's uploads: nothing drawn while
//!   nothing changes, an instance's old and new places when it moves.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::too_many_lines)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_orthographic, vulkan_perspective};
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, CascadeCache, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Occlusion,
    ShadowSettings, Temporal, ViewFeatures,
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

fn gpu() -> Option<(VkInstance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(VkInstance, Device), engine_rhi::RhiError> {
        let instance = VkInstance::new(&InstanceDesc {
            app_name: String::from("engine_render gpu scene test"),
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

/// What one frame draws.
enum Draw<'a> {
    List(&'a [DrawItem]),
    Scene(&'a mut GpuScene),
}

/// What one frame produced.
struct Shot {
    image: Vec<u8>,
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
    /// The view's cached shadow cascades, when set (ADR 0093).
    shadow_cache: Option<CascadeCache>,
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
                resolution: 512,
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
        let bytes = u64::from(size * size * 4) * 2 + COMMAND_BYTES;
        let (readback, readback_memory) = allocator
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
            shadow_cache: None,
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
            shadow_cache,
            ..
        } = self;
        let (d, cmd, size) = (device.raw(), *cmd, *size);
        let image_bytes = u64::from(size * size * 4);
        let total = image_bytes * 2 + COMMAND_BYTES;
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
                shadow_cache: shadow_cache.as_ref(),
                virtual_shadows: None,
                lights: &[],
                decals: &[],
                environment: environment.as_ref(),
                reflections: Default::default(),
                gi: None,
                features: ViewFeatures::ALL,
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
        let (passes, scene, command_bytes, has_motion) = shot;
        let ptr = readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), total as usize) };
        let image = bytes[..image_bytes as usize].to_vec();
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
        // `instance_count`, the second word of each 20-byte command.
        let counts = commands
            .chunks_exact(20)
            .map(|c| u32::from_le_bytes([c[4], c[5], c[6], c[7]]))
            .collect();
        Shot {
            image,
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
            if let Some(cache) = self.shadow_cache.as_mut() {
                cache.destroy(allocator, &self.bindless);
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

/// The first differing pixels of two `width`-wide RGBA8 images, for assertion messages.
/// A mismatch only on the Radeon iGPU of ref-1 while the CPU is busy is a hardware fault
/// first (its groupshared memory flips bits): run the `gpu_selfcheck` test before looking
/// for an engine bug.
fn explain(a: &[u8], b: &[u8], width: usize) -> String {
    let pixels: Vec<String> = a
        .chunks_exact(4)
        .zip(b.chunks_exact(4))
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .take(8)
        .map(|(i, (a, b))| format!("({}, {}) {a:?} vs {b:?}", i % width, i / width))
        .collect();
    let hint = "on the Radeon iGPU under CPU load, run the gpu_selfcheck test first \
                (docs/research/radeon-igpu-lds-faults.md)";
    format!(
        "{} pixels differ, first: {}; {hint}",
        differing(a, b),
        pixels.join(", ")
    )
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
            assert!(
                differing(&list.image, &shot.image) == 0,
                "{label}: the scene's image differs from the draw list's: {}",
                explain(&list.image, &shot.image, 128)
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
        assert!(
            differing(&a.image, &b.image) == 0,
            "step {k}: image: {}",
            explain(&a.image, &b.image, 128)
        );
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

/// Motion between ticks on the GPU (ADR 0064 addendum): an instance given its two ticks'
/// transforms once per tick (`set_motion`) is drawn at each frame's interpolation factor
/// (`set_alpha`) with nothing uploaded in between, and its motion vectors follow the
/// frames' placements; stopping, teleporting and removing it end the motion.
#[test]
fn instances_move_between_ticks_on_the_gpu() {
    use engine_core::math::Affine3A;
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
        Affine3A::from_scale_rotation_translation(
            Vec3::splat(1.5),
            Quat::IDENTITY,
            Vec3::new(x, y, 0.0),
        )
    };
    let temporal = Temporal::new(&bed.device, &mut bed.allocator, 16).unwrap();
    let mut scene = bed.scene(16);
    let index = |x: f32, y: f32| {
        let (px, py) = ((32.0 + x * 8.0) as u32, (32.0 - y * 8.0) as u32);
        (py * SIZE + px) as usize
    };
    let motion = |shot: &Shot, x: f32, y: f32| shot.motion[index(x, y)];
    // Drawn there: its pixel differs from the background's (a corner).
    let covered = |shot: &Shot, x: f32, y: f32| {
        let i = index(x, y) * 4;
        shot.image[i..i + 3] != shot.image[0..3]
    };
    let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.05 && (a[1] - b[1]).abs() < 0.05;

    let a = scene.insert(SceneInstance {
        mesh,
        material,
        world_from_object: Mat4::from(at(-2.0, 1.5)),
    });
    bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));

    // A tick moved it 1 unit right; the frame is a quarter into the next tick: -1.75,
    // 2 px from where it was.
    assert!(scene.set_motion(a, &at(-2.0, 1.5), &at(-1.0, 1.5)));
    scene.set_alpha(0.25);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    let stats = shot.scene.unwrap();
    assert_eq!((stats.moving, stats.motion_updates), (1, 1));
    let m = motion(&shot, -1.75, 1.5);
    assert!(near(m, [2.0, 0.0]), "a quarter in: {m:?}");
    assert!(covered(&shot, -1.75, 1.5) && !covered(&shot, -2.9, 1.5));

    // Three quarters in, nothing uploaded: 4 px more.
    scene.set_alpha(0.75);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    let stats = shot.scene.unwrap();
    assert_eq!(
        (stats.moving, stats.motion_updates, stats.uploaded),
        (1, 0, 0)
    );
    let m = motion(&shot, -1.25, 1.5);
    assert!(near(m, [4.0, 0.0]), "three quarters in: {m:?}");

    // The next tick, from -1 to 0, a quarter in: -0.75, 4 px from -1.25.
    assert!(scene.set_motion(a, &at(-1.0, 1.5), &at(0.0, 1.5)));
    scene.set_alpha(0.25);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    let m = motion(&shot, -0.75, 1.5);
    assert!(near(m, [4.0, 0.0]), "next tick: {m:?}");

    // It stops at its last tick's transform: 6 px from -0.75, then still.
    assert!(scene.set_affine(a, &at(0.0, 1.5)));
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert_eq!(shot.scene.unwrap().moving, 0);
    let m = motion(&shot, 0.0, 1.5);
    assert!(near(m, [6.0, 0.0]), "stopped: {m:?}");
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert!(near(motion(&shot, 0.0, 1.5), [0.0; 2]), "settled");

    // A new instance moving from its first frame: halfway, without motion.
    let b = scene.insert(SceneInstance {
        mesh,
        material,
        world_from_object: Mat4::from(at(2.0, -1.5)),
    });
    assert!(scene.set_motion(b, &at(2.0, -1.5), &at(2.0, -0.5)));
    scene.set_alpha(0.5);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert!(covered(&shot, 2.0, -1.0));
    let m = motion(&shot, 2.0, -1.0);
    assert!(near(m, [0.0; 2]), "new: {m:?}");

    // Teleported while moving: no motion where it lands, and no longer moving.
    scene.teleport(b, &Mat4::from(at(-2.0, -1.5)));
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert_eq!(shot.scene.unwrap().moving, 0);
    assert!(covered(&shot, -2.0, -1.5));
    assert!(near(motion(&shot, -2.0, -1.5), [0.0; 2]), "teleported");

    // `step_motion` goes from the transform the instance was last given; two ticks before
    // an upload move it over the last one only (from -1 to 0, halfway: -0.5).
    assert!(scene.step_motion(a, &at(-1.0, 1.5)));
    assert!(scene.step_motion(a, &at(0.0, 1.5)));
    scene.set_alpha(0.5);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert!(covered(&shot, -0.5, 1.5) && !covered(&shot, 0.9, 1.5));
    // Since last frame (at 0) it moved back 4 px: the first frame of a motion starts
    // from what the instance showed.
    let m = motion(&shot, -0.5, 1.5);
    assert!(near(m, [-4.0, 0.0]), "two ticks: {m:?}");
    // The next tick steps on from 0 to 1, a quarter in: 0.25, 6 px on.
    assert!(scene.step_motion(a, &at(1.0, 1.5)));
    scene.set_alpha(0.25);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    let m = motion(&shot, 0.25, 1.5);
    assert!(near(m, [6.0, 0.0]), "stepped: {m:?}");
    assert!(scene.set_affine(a, &at(1.0, 1.5)));

    // Removed while moving: gone.
    assert!(scene.set_motion(b, &at(-2.0, -1.5), &at(-1.0, -1.5)));
    scene.set_alpha(0.5);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert_eq!(shot.scene.unwrap().moving, 1);
    assert!(covered(&shot, -1.5, -1.5));
    scene.remove(b);
    let shot = bed.frame(Draw::Scene(&mut scene), camera, None, Some(&temporal));
    assert_eq!(shot.scene.unwrap().moving, 0);
    assert!(!covered(&shot, -1.5, -1.5), "removed");
    bed.finish(vec![scene], (Vec::new(), vec![temporal]));
}

/// (Instance, transform) pairs for `GpuScene::step_motions`, and runs of them.
struct Pairs(Vec<(InstanceId, *const engine_core::math::Affine3A)>);
struct Runs<'a>(Vec<&'a [(InstanceId, *const engine_core::math::Affine3A)]>);
// SAFETY: the pointers are only read, while the transforms they point at live.
unsafe impl Sync for Runs<'_> {}

impl<'a> Runs<'a> {
    fn run(&self, k: usize) -> &'a [(InstanceId, *const engine_core::math::Affine3A)] {
        self.0[k]
    }
}

/// The work of instances whose only change is their motion between ticks, spread over a
/// thread pool (`GpuScene::set_jobs`), gives the same frames as on one thread: pixels,
/// motion vectors, statistics and bounds, with explicit starts, two ticks before an
/// upload, mirroring that flips (another batch, which culling reads from the instance
/// data: every instance is counted in its batch), material edits while moving, and stops.
#[test]
fn moving_instances_apply_the_same_on_one_thread_and_on_a_pool() {
    use engine_core::jobs::ThreadPool;
    use engine_core::math::Affine3A;
    use std::sync::Arc;
    const N: usize = 6000;
    let Some(mut bed) = Bed::new(128) else {
        return;
    };
    let mesh = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let paint = [
        bed.materials.default_material(),
        bed.materials
            .add(MaterialDesc::solid([0.9, 0.3, 0.2], 0.0, 0.5)),
    ];
    let eye = Vec3::new(0.0, 0.0, 10.0);
    let camera = Camera {
        clip_from_world: vulkan_orthographic(-4.0, 4.0, -4.0, 4.0, 0.1, 50.0)
            * look_at(eye, Vec3::ZERO, Vec3::Y),
        position: eye,
    };
    // A grid of small cubes turning and swaying, apart (where they overlapped at one
    // depth, the order the culling appends them in would pick the pixel); every 97th
    // mirrors at odd ticks.
    let place = |i: usize, tick: u32| {
        let (column, row) = ((i % 80) as f32, (i / 80) as f32);
        let a = tick as f32 * 0.3 + i as f32 * 0.01;
        let x = if i.is_multiple_of(97) && tick % 2 == 1 {
            -0.03
        } else {
            0.03
        };
        Affine3A::from_scale_rotation_translation(
            Vec3::new(x, 0.03, 0.03),
            Quat::from_rotation_z(a),
            Vec3::new(
                -3.95 + column * 0.1 + 0.005 * a.sin(),
                -3.75 + row * 0.1,
                0.0,
            ),
        )
    };
    let mut scenes = [bed.scene(N as u32), bed.scene(N as u32)];
    scenes[1].set_jobs(Arc::new(ThreadPool::builder().workers(4).build().unwrap()));
    let temporal = [
        Temporal::new(&bed.device, &mut bed.allocator, 8192).unwrap(),
        Temporal::new(&bed.device, &mut bed.allocator, 8192).unwrap(),
    ];
    let mut ids: [Vec<InstanceId>; 2] = [Vec::new(), Vec::new()];
    for (scene, ids) in scenes.iter_mut().zip(&mut ids) {
        for i in 0..N {
            ids.push(scene.insert(SceneInstance {
                mesh,
                material: paint[0],
                world_from_object: Mat4::from(place(i, 0)),
            }));
        }
    }
    // The tick whose transform each instance was last given.
    let mut given = vec![0u32; N];
    for tick in 1..8u32 {
        for (i, given) in given.iter_mut().enumerate() {
            *given = match (i % 211, tick) {
                (2, 4) => 4,
                (3..=9, 5) => 6,
                _ => tick,
            };
        }
        // Drawn in the mirrored batch: every instance in the batch its handedness says
        // (one left with a stale batch on the GPU would be counted in the other).
        let mirrored = (0..N)
            .filter(|&i| place(i, given[i]).matrix3.determinant() < 0.0)
            .count() as u32;
        for (k, (scene, ids)) in scenes.iter_mut().zip(&ids).enumerate() {
            // The plain steps last: one by one on one thread, at once (in runs) on the pool.
            let mut steps = Vec::new();
            for (i, &id) in ids.iter().enumerate() {
                match (i % 211, tick) {
                    // Both ticks given: an explicit start.
                    (0, _) => assert!(scene.set_motion(id, &place(i, tick - 1), &place(i, tick))),
                    // A material edit while it moves.
                    (1, 3) => {
                        scene.set_material(id, paint[1]);
                        steps.push((id, place(i, tick)));
                    }
                    // Stopped at a tick, moving again from the next.
                    (2, 4) => assert!(scene.set_affine(id, &place(i, tick))),
                    // Two ticks before an upload.
                    (3..=9, 5) => {
                        assert!(scene.step_motion(id, &place(i, tick)));
                        steps.push((id, place(i, tick + 1)));
                    }
                    _ => steps.push((id, place(i, tick))),
                }
            }
            if k == 0 {
                for (id, to) in &steps {
                    assert!(scene.step_motion(*id, to));
                }
            } else {
                let pairs = Pairs(
                    steps
                        .iter()
                        .map(|(id, to)| (*id, to as *const Affine3A))
                        .collect(),
                );
                let runs: Vec<_> = pairs.0.chunks(997).collect();
                let runs = Runs(runs);
                // SAFETY: every instance once; the transforms live in `steps`.
                unsafe { scene.step_motions(runs.0.len(), &|k| runs.run(k)) };
            }
        }
        for alpha in [0.25, 0.75] {
            let mut shots = Vec::new();
            for (scene, temporal) in scenes.iter_mut().zip(&temporal) {
                scene.set_alpha(alpha);
                shots.push(bed.frame(Draw::Scene(scene), camera, None, Some(temporal)));
            }
            let (a, b) = (&shots[0], &shots[1]);
            let expected: &[u32] = if mirrored > 0 {
                &[N as u32 - mirrored, mirrored]
            } else {
                &[N as u32]
            };
            for shot in &shots {
                assert_eq!(shot.drawn(0, expected.len()), N as u32, "tick {tick}");
                assert_eq!(&shot.counts[..expected.len()], expected, "tick {tick}");
            }
            assert_eq!(a.scene, b.scene, "tick {tick}, alpha {alpha}");
            assert!(a.scene.unwrap().moving > 5000, "{:?}", a.scene);
            assert_eq!(scenes[0].bounds(), scenes[1].bounds(), "tick {tick}");
            assert_eq!(
                differing(&a.image, &b.image),
                0,
                "tick {tick}, alpha {alpha}"
            );
            assert!(
                a.motion == b.motion,
                "tick {tick}, alpha {alpha}: motion vectors"
            );
        }
    }
    let [a, b] = scenes;
    let [t0, t1] = temporal;
    bed.finish(vec![a, b], (Vec::new(), vec![t0, t1]));
}

/// Cached shadow cascades (ADR 0093) on the scene path: frames that upload nothing draw no
/// shadows, an upload that moves an instance draws its old and new places, a moving camera
/// only strips, and what the cache holds shades as drawing it all does.
#[test]
fn cached_cascades_follow_the_scenes_uploads() {
    let Some(mut bed) = Bed::new(128) else {
        return;
    };
    let mesh = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let gray = bed
        .materials
        .add(MaterialDesc::solid([0.6, 0.6, 0.6], 0.0, 0.8));
    let mut scene = bed.scene(16);
    let place = |size: Vec3, at: Vec3| SceneInstance {
        mesh,
        material: gray,
        world_from_object: Mat4::from_scale_rotation_translation(size, Quat::IDENTITY, at),
    };
    scene.insert(place(Vec3::new(30.0, 0.2, 30.0), Vec3::new(0.0, -0.1, 0.0)));
    let tower = scene.insert(place(Vec3::new(1.0, 4.0, 1.0), Vec3::new(-1.0, 2.0, -1.0)));
    scene.insert(place(Vec3::new(2.0, 1.0, 1.0), Vec3::new(2.0, 0.5, 1.0)));
    let settings = *bed.renderer.shadow_settings();
    bed.shadow_cache =
        Some(CascadeCache::new(&bed.device, &mut bed.allocator, &bed.bindless, &settings).unwrap());
    let layers = u64::from(settings.cascades * settings.resolution * settings.resolution);
    let mut camera = perspective_camera(Vec3::new(2.0, 7.0, 9.0));
    let frame = |bed: &mut Bed, scene: &mut GpuScene, camera: Camera| {
        let shot = bed.frame(Draw::Scene(scene), camera, None, None);
        let texels = bed.shadow_cache.as_ref().unwrap().stats().last_texels;
        (shot.image, texels)
    };
    assert_eq!(
        frame(&mut bed, &mut scene, camera).1,
        layers,
        "the first frame"
    );
    assert_eq!(frame(&mut bed, &mut scene, camera).1, 0, "nothing uploaded");
    assert!(scene.set_transform(
        tower,
        &Mat4::from_scale_rotation_translation(
            Vec3::new(1.0, 4.0, 1.0),
            Quat::IDENTITY,
            Vec3::new(-1.5, 2.0, -1.0)
        )
    ));
    let moved = frame(&mut bed, &mut scene, camera).1;
    assert!(
        moved > 0 && moved < layers / 4,
        "an instance moved: {moved} texels"
    );
    assert_eq!(frame(&mut bed, &mut scene, camera).1, 0, "nothing since");
    // A first step may outgrow the reach the cascades were split over (a new view's is just
    // what it needs): it is chosen again with room, and everything drawn once more.
    camera = perspective_camera(Vec3::new(2.2, 7.0, 8.9));
    frame(&mut bed, &mut scene, camera);
    // The scene is small, its cascades too: a step is many of their texels.
    camera = perspective_camera(Vec3::new(2.4, 7.0, 8.8));
    let strips = frame(&mut bed, &mut scene, camera).1;
    assert!(
        strips > 0 && strips < layers / 2,
        "the camera moved: {strips} texels"
    );
    let (kept, none) = frame(&mut bed, &mut scene, camera);
    assert_eq!(none, 0);
    bed.shadow_cache.as_ref().unwrap().set_redraw_always(true);
    let (whole, all) = frame(&mut bed, &mut scene, camera);
    assert_eq!(all, layers);
    assert_eq!(
        differing(&kept, &whole),
        0,
        "what the cache holds shades as drawing everything: {}",
        explain(&kept, &whole, 128)
    );
    bed.finish(vec![scene], (Vec::new(), Vec::new()));
}
