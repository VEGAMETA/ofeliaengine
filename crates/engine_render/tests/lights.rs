//! Local lights (ADR 0045) against the brute-force reference, under the validation layer
//! with synchronization validation.
//!
//! A ground with cubes lit by hundreds of point and spot lights of many ranges, some
//! behind the camera, some around its near plane, some far away. Culled by z-bins and
//! tile bitmasks, every pixel must come out as when it evaluates every light; and the
//! lights must actually light the scene.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, LocalLight, ShadowSettings,
    Spot,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const WIDTH: u32 = 400;
const HEIGHT: u32 = 225;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render light test"),
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

/// A small deterministic generator (xorshift).
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 >> 8) as f32 / (1 << 24) as f32
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next()
    }
}

#[test]
fn culled_lights_match_every_light_for_every_pixel() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let gray = materials.add(MaterialDesc::solid([0.6, 0.6, 0.6], 0.0, 0.6));
    let metal = materials.add(MaterialDesc::solid([0.9, 0.7, 0.4], 1.0, 0.3));
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 8 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let bytes = u64::from(WIDTH * HEIGHT * 8);
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
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

    let mut rng = Rng(0x9E37_79B9);
    let mut draws = vec![DrawItem {
        mesh,
        material: gray,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(200.0, 0.2, 200.0),
            Quat::IDENTITY,
            Vec3::new(0.0, -0.1, -60.0),
        ),
    }];
    for k in 0..120 {
        draws.push(DrawItem {
            mesh,
            material: if k % 3 == 0 { metal } else { gray },
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::new(
                    rng.range(0.5, 2.0),
                    rng.range(0.5, 4.0),
                    rng.range(0.5, 2.0),
                ),
                Quat::from_rotation_y(rng.range(0.0, 3.0)),
                Vec3::new(rng.range(-30.0, 30.0), 1.0, rng.range(-90.0, 5.0)),
            ),
        });
    }
    // Hundreds of lights: small and large, point and spot, in front of the camera, behind
    // it, around its near plane and far away.
    let mut lights: Vec<LocalLight> = (0..600)
        .map(|k| {
            let position = Vec3::new(
                rng.range(-40.0, 40.0),
                rng.range(0.2, 6.0),
                rng.range(-120.0, 15.0),
            );
            let color = Vec3::new(
                rng.range(0.2, 1.0),
                rng.range(0.2, 1.0),
                rng.range(0.2, 1.0),
            ) * rng.range(2.0, 20.0);
            let spot = (k % 4 == 0).then(|| {
                let inner = rng.range(0.1, 0.5);
                Spot {
                    direction: Vec3::new(rng.range(-0.5, 0.5), -1.0, rng.range(-0.5, 0.5))
                        .normalize(),
                    inner,
                    outer: inner + rng.range(0.05, 0.4),
                }
            });
            LocalLight {
                position,
                range: if k % 50 == 0 {
                    rng.range(20.0, 40.0)
                } else {
                    rng.range(2.0, 12.0)
                },
                color,
                spot,
            }
        })
        .collect();
    // One around the camera's near plane, one right behind it.
    lights.push(LocalLight {
        position: Vec3::new(0.2, 2.0, -0.05),
        range: 5.0,
        color: Vec3::splat(3.0),
        spot: None,
    });
    lights.push(LocalLight {
        position: Vec3::new(0.0, 2.0, 3.0),
        range: 4.0,
        color: Vec3::splat(3.0),
        spot: None,
    });

    let eye = Vec3::new(0.0, 2.0, 0.0);
    let camera = Camera {
        clip_from_world: vulkan_perspective(1.0, WIDTH as f32 / HEIGHT as f32, 0.1, 300.0)
            * look_at(eye, Vec3::new(0.0, 0.5, -10.0), Vec3::Y),
        position: eye,
    };
    let sun = DirectionalLight {
        direction: Vec3::new(-0.4, -1.0, -0.3),
        color: Vec3::splat(0.2),
        ambient: Vec3::splat(0.02),
    };

    let mut render = |lights: &[LocalLight], culling: bool| -> Vec<[f32; 3]> {
        let mut renderer =
            ForwardRenderer::new(&device, &bindless, HDR, DEPTH, ShadowSettings::default())
                .unwrap();
        renderer.set_light_culling(culling);
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
                        light: sun,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
                        lights,
                        environment: None,
                    },
                )
                .unwrap();
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
        drop(renderer);
        let ptr = readback_memory.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), bytes as usize) };
        raw.chunks_exact(8)
            .map(|p| {
                let h = |k: usize| half::to_f32(u16::from_le_bytes([p[2 * k], p[2 * k + 1]]));
                [h(0), h(1), h(2)]
            })
            .collect()
    };

    let culled = render(&lights, true);
    let reference = render(&lights, false);
    let unlit = render(&[], true);
    let (mut worst, mut differing, mut lit) = (0.0f32, 0, 0);
    for ((c, r), u) in culled.iter().zip(&reference).zip(&unlit) {
        for k in 0..3 {
            // Relative to the pixel's brightness: half floats round at 1/1024.
            let error = (c[k] - r[k]).abs() / (1.0 + r[k].abs());
            worst = worst.max(error);
            differing += usize::from(error > 2.0 / 1024.0);
        }
        lit += usize::from(r[0] + r[1] + r[2] > (u[0] + u[1] + u[2]) * 1.5 + 0.01);
    }
    println!(
        "{} lights: worst relative difference {worst:.5}, {differing} channels beyond 2/1024, \
         {lit} of {} pixels lit by them",
        lights.len(),
        WIDTH * HEIGHT
    );
    assert!(culled.iter().flatten().all(|v| v.is_finite()));
    assert_eq!(differing, 0, "culling changed the image (worst {worst})");
    assert!(
        lit > (WIDTH * HEIGHT * 15 / 100) as usize,
        "the lights light the scene: {lit}"
    );

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(readback, readback_memory);
        d.destroy_command_pool(pool, None);
    }
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
