//! The sun's cached cascades (ADR 0093) against the same cascades drawn whole every frame,
//! under the validation layer with synchronization validation.
//!
//! A ground and a field of poles and boxes under a directional light, seen by a camera
//! that walks, strafes, turns, stands still and jumps far, then waits while a box moves,
//! then walks on with it. Every frame draws the view twice with the same camera: through a
//! cache, which draws only the strips its cascades' windows newly cover and the moved
//! box's old and new places, and through one drawn whole every frame (with the same fit).
//! The images must match, and the cache must have drawn less: nothing while nothing moves,
//! a little when the box does, everything again after the jump. Once with instanced draws,
//! once by meshlet (with spheres for the poles), where a cascade's pieces are drawn at once
//! when vertex shaders can pick viewports.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_assets::meshlet::build_meshlets;
use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, CascadeCache, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
    ViewFeatures,
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

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const LIGHT: Vec3 = Vec3::new(-0.4, -1.0, -0.3);

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render cached shadow test"),
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

/// Where the camera is in a frame, and where it looks (yaw about +Y, from −Z).
#[derive(Clone, Copy)]
struct Pose {
    eye: Vec3,
    yaw: f32,
}

/// The frames a box moves in: the camera still, then walking too.
const MOVING: core::ops::Range<usize> = 36..44;

/// The frame after the box stops (the camera still).
const STOPPED: usize = MOVING.end;

/// The walk: forward, strafing, turning (also on the spot), standing still, then a jump
/// far away, a few steps there, a wait while a box moves, and walking while it does.
fn path() -> Vec<Pose> {
    let mut poses = Vec::new();
    let mut eye = Vec3::new(0.0, 3.0, 0.0);
    let mut yaw = 0.0f32;
    for k in 0..STOPPED + 1 {
        match k {
            // Walking forward at 5 m/s (60 fps), then faster.
            0..8 => eye.z -= 0.083,
            8..12 => eye.z -= 0.4,
            // Strafing and turning together.
            12..20 => {
                eye.x += 0.12;
                yaw += 0.03;
            }
            // Standing still.
            20..23 => {}
            // Turning on the spot, fast.
            23..28 => yaw -= 0.12,
            // A jump far away: every cascade starts again.
            28 => eye += Vec3::new(260.0, 0.0, -180.0),
            29..36 => {
                eye.x -= 0.2;
                eye.z -= 0.1;
            }
            // Walking while the box moves (after three frames of standing).
            39..44 => {
                eye.x += 0.06;
                eye.z -= 0.05;
            }
            // Still while the box moves, and once it stopped.
            _ => {}
        }
        poses.push(Pose { eye, yaw });
    }
    poses
}

#[test]
fn cached_cascades_match_cascades_drawn_whole() {
    run(false);
}

#[test]
fn cached_cascades_by_meshlet_match_cascades_drawn_whole() {
    run(true);
}

fn run(meshlets: bool) {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    // By meshlet: the cube's one meshlet, and spheres of many for the poles.
    let (mesh, pole) = if meshlets {
        let data = cube();
        let mut indices = data.indices.clone();
        let cube_meshlets = build_meshlets(&data.positions, &mut indices);
        let mesh = geometry
            .add_mesh(&MeshData {
                indices: &indices,
                meshlets: bytemuck::cast_slice(&cube_meshlets),
                ..data.as_data()
            })
            .unwrap();
        // Half a unit across, as the cube.
        let (unit, mut indices) = super::meshlets::sphere(24, 48);
        let positions: Vec<[f32; 3]> = unit.iter().map(|p| p.map(|c| c * 0.5)).collect();
        let sphere_meshlets = build_meshlets(&positions, &mut indices);
        assert!(
            sphere_meshlets.len() > 10,
            "{} meshlets",
            sphere_meshlets.len()
        );
        let pole = geometry
            .add_mesh(&MeshData {
                positions: &positions,
                normals: &unit,
                tangents: &[],
                uvs: &[],
                indices: &indices,
                meshlets: bytemuck::cast_slice(&sphere_meshlets),
            })
            .unwrap();
        (mesh, pole)
    } else {
        let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
        (mesh, mesh)
    };
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let gray = materials.add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 1.0));
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 8 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let settings = ShadowSettings::default();
    let mut renderer = ForwardRenderer::new(&device, &bindless, HDR, DEPTH, settings).unwrap();
    if meshlets
        && !renderer
            .set_meshlet_culling(&device, &bindless, true)
            .unwrap()
    {
        println!("skipping: no drawIndirectCount or shaderInt64");
        return;
    }
    let cached = CascadeCache::new(&device, &mut allocator, &bindless, &settings).unwrap();
    let whole = CascadeCache::new(&device, &mut allocator, &bindless, &settings).unwrap();
    whole.set_redraw_always(true);

    // A ground 800 m across and a field of poles and boxes of every size.
    let mut draws = vec![DrawItem {
        mesh,
        material: gray,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(800.0, 0.2, 800.0),
            Quat::IDENTITY,
            Vec3::new(100.0, -0.1, -100.0),
        ),
    }];
    let mut seed = 7u32;
    let mut random = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1 << 24) as f32
    };
    // Spread over the whole ground, and dense around the walk and where it jumps to.
    let spots = [Vec3::new(0.0, 0.0, -20.0), Vec3::new(255.0, 0.0, -200.0)];
    for k in 0..700 {
        let at = match k % 7 {
            0..3 => Vec3::new(random() * 460.0 - 120.0, 0.0, -random() * 400.0 + 60.0),
            n => spots[n % 2] + Vec3::new(random() * 60.0 - 30.0, 0.0, random() * 60.0 - 30.0),
        };
        let (size, mesh) = if random() < 0.7 {
            // A pole.
            let size = Vec3::new(
                0.2 + random() * 0.4,
                1.0 + random() * 6.0,
                0.2 + random() * 0.4,
            );
            (size, pole)
        } else {
            let size = Vec3::new(
                1.0 + random() * 6.0,
                0.5 + random() * 8.0,
                1.0 + random() * 6.0,
            );
            (size, mesh)
        };
        draws.push(DrawItem {
            mesh,
            material: gray,
            world_from_object: Mat4::from_scale_rotation_translation(
                size,
                Quat::from_rotation_y(random() * 3.0),
                at + Vec3::Y * (size.y * 0.5),
            ),
        });
    }
    let light = DirectionalLight {
        direction: LIGHT,
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.1),
    };

    let bytes = u64::from(WIDTH * HEIGHT * 8);
    let mut readback = || {
        allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap()
    };
    let (readbacks, memories): (Vec<_>, Vec<_>) = [readback(), readback()].into_iter().unzip();
    let d = device.raw();
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

    // The box that moves: in front of the camera where it jumped.
    let moving = draws.len();
    draws.push(DrawItem {
        mesh,
        material: gray,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(2.0, 4.0, 2.0),
            Quat::IDENTITY,
            Vec3::new(254.0, 2.0, -192.0),
        ),
    });
    let poses = path();
    let (mut worst, mut shadowed, mut cached_texels, mut whole_texels) =
        (0usize, 0usize, 0u64, 0u64);
    for (frame, pose) in poses.iter().enumerate() {
        if MOVING.contains(&frame) {
            draws[moving].world_from_object.w_axis.x += 0.5;
        }
        let forward = Quat::from_rotation_y(pose.yaw) * Vec3::NEG_Z;
        let clip_from_world = vulkan_perspective(1.0, WIDTH as f32 / HEIGHT as f32, 0.1, 400.0)
            * look_at(pose.eye, pose.eye + forward - Vec3::Y * 0.25, Vec3::Y);
        let camera = Camera {
            clip_from_world,
            position: pose.eye,
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
            for (cache, &target) in [&cached, &whole].into_iter().zip(&readbacks) {
                let color = fg.create_texture("color", TextureDesc::new_2d(HDR, WIDTH, HEIGHT));
                let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, WIDTH, HEIGHT));
                renderer
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
                            shadow_cache: Some(cache),
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
                let out = fg.import_buffer(
                    "readback",
                    ImportedBuffer {
                        buffer: target,
                        size: bytes,
                        address: 0,
                        initial: BufferState::idle(),
                        final_state: Some(BufferState::host_read()),
                    },
                );
                fg.add_pass("read back")
                    .read(color, Access::TransferRead)
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
                                width: WIDTH,
                                height: HEIGHT,
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
        let image = |k: usize| -> Vec<f32> {
            let ptr = memories[k].mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping covers the image.
            let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), bytes as usize) };
            raw.chunks_exact(8)
                .map(|p| half::to_f32(u16::from_le_bytes([p[0], p[1]])))
                .collect()
        };
        let (a, b) = (image(0), image(1));
        let differ = a
            .iter()
            .zip(&b)
            .filter(|(x, y)| (*x - *y).abs() > 1e-3)
            .count();
        // In shadow: ambient only (0.1 × albedo 0.5), well under the lit ground.
        shadowed = shadowed.max(b.iter().filter(|&&v| v > 0.0 && v < 0.08).count());
        let (c, w) = (cached.stats(), whole.stats());
        println!(
            "frame {frame:2}: {differ} pixels differ; texels drawn cached {:8}, whole {:8}",
            c.last_texels, w.last_texels
        );
        worst = worst.max(differ);
        cached_texels += c.last_texels;
        whole_texels += w.last_texels;
        let layers = u64::from(settings.cascades * settings.resolution * settings.resolution);
        assert_eq!(w.last_texels, layers, "the reference draws everything");
        match frame {
            // The first frame: everything.
            0 => assert_eq!(c.last_texels, layers),
            // The jump: every cascade it took out of its window (the coarsest may still
            // overlap and only draw what it newly covers).
            28 => assert!(c.last_texels > layers / 2, "frame 28: {}", c.last_texels),
            // A caster moved: its old and new places, with the strips while walking.
            f if MOVING.contains(&f) => assert!(
                c.last_texels > 0 && c.last_texels < layers / 8,
                "frame {f}: {}",
                c.last_texels
            ),
            // Nothing moved since.
            STOPPED => assert_eq!(c.last_texels, 0, "frame {STOPPED}"),
            // Standing still (the frame before moved): nothing.
            21 | 22 => assert_eq!(c.last_texels, 0, "frame {frame}"),
            _ => assert!(
                c.last_texels < layers / 4,
                "frame {frame}: {}",
                c.last_texels
            ),
        }
    }
    println!(
        "cached drew {:.1} % of the texels; at most {worst} pixels differed; {shadowed} in shadow",
        100.0 * cached_texels as f64 / whole_texels as f64
    );
    assert!(
        shadowed > 5000,
        "the view shows shadows ({shadowed} pixels)"
    );
    // The same projections, drawn in pieces or whole: the same image, up to the odd
    // pixel the rasterizer snaps apart across a viewport offset.
    assert!(worst <= 8, "{worst} pixels differ");
    // By meshlet, where vertex shaders pick viewports, the walk's strips were drawn at once.
    let together = cached.stats().together;
    println!("cascades drawn in pieces at once: {together}");
    if meshlets && device.adapter().caps.viewport_index {
        assert!(together > 20, "{together} cascades drawn in pieces at once");
    } else {
        assert_eq!(together, 0);
    }

    // SAFETY: the device is idle.
    unsafe {
        let (mut cached, mut whole) = (cached, whole);
        cached.destroy(&mut allocator, &bindless);
        whole.destroy(&mut allocator, &bindless);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        for (buffer, memory) in readbacks.into_iter().zip(memories) {
            allocator.destroy_buffer(buffer, memory);
        }
        d.destroy_command_pool(pool, None);
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
