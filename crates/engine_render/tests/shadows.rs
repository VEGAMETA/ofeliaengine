//! Cascaded shadows (ADR 0044) against analytic shadows, under the validation layer with
//! synchronization validation.
//!
//! A 400 m ground and axis-aligned boxes under a directional light, seen from 5 m above
//! the ground. For every pixel showing the ground, a ray towards the light tells exactly
//! whether it is in shadow; the rendered pixel must agree:
//!
//! - near the camera, a thin pole's shadow: four cascades must make far fewer mistakes
//!   than one map covering the same distance;
//! - a box 90 m away still casts its shadow (the last cascade);
//! - a tall box outside the camera's view casts into it (casters are culled per cascade,
//!   not by the camera);
//! - lit ground stays lit (no acne) and shadows start at their casters (no gap).

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, Vec4, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
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

const WIDTH: u32 = 384;
const HEIGHT: u32 = 216;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const LIGHT: Vec3 = Vec3::new(-0.4, -1.0, -0.3);

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render shadow test"),
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

/// An axis-aligned box caster.
#[derive(Clone, Copy)]
struct Block {
    center: Vec3,
    size: Vec3,
}

impl Block {
    /// Distance along the ray to the box, if it hits it (slab test).
    fn hit(&self, origin: Vec3, direction: Vec3) -> Option<f32> {
        let (lo, hi) = (self.center - self.size * 0.5, self.center + self.size * 0.5);
        let (mut near, mut far) = (0.0f32, f32::MAX);
        for axis in 0..3 {
            let (o, d) = (origin[axis], direction[axis]);
            if d.abs() < 1e-9 {
                if o < lo[axis] || o > hi[axis] {
                    return None;
                }
                continue;
            }
            let (a, b) = ((lo[axis] - o) / d, (hi[axis] - o) / d);
            near = near.max(a.min(b));
            far = far.min(a.max(b));
        }
        (near <= far).then_some(near)
    }
}

/// What a pixel of the ground should be.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Truth {
    /// Not the ground (sky, a box), or too close to a shadow edge to judge.
    Skip,
    Lit,
    Shadow,
}

/// Mistakes over judged ground pixels, by distance from the camera and by caster.
#[derive(Debug, Default)]
struct Score {
    near_wrong: u32,
    near_total: u32,
    wrong: u32,
    total: u32,
    far_shadow_dark: u32,
    far_shadow_total: u32,
    outside_shadow_dark: u32,
    outside_shadow_total: u32,
    /// Mistakes by distance: < 12, < 40, < 100, < 150, farther; lit drawn dark, dark drawn lit.
    by_distance: [[u32; 2]; 5],
}

#[test]
fn cascaded_shadows_match_analytic_shadows() {
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
    let gray = materials.add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 1.0));
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
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

    let blocks = [
        // A thin pole 4 m in front of the camera.
        Block {
            center: Vec3::new(0.8, 1.0, -4.0),
            size: Vec3::new(0.15, 2.0, 0.15),
        },
        // A box 20 m away.
        Block {
            center: Vec3::new(-3.0, 1.0, -20.0),
            size: Vec3::splat(2.0),
        },
        // A box 90 m away.
        Block {
            center: Vec3::new(14.0, 6.0, -90.0),
            size: Vec3::splat(12.0),
        },
        // A tower right of the view: its shadow falls into it.
        Block {
            center: Vec3::new(16.0, 10.0, -12.0),
            size: Vec3::new(2.0, 20.0, 2.0),
        },
    ];
    const FAR: usize = 2;
    const OUTSIDE: usize = 3;
    let mut draws = vec![DrawItem {
        mesh,
        material: gray,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(400.0, 0.2, 400.0),
            Quat::IDENTITY,
            Vec3::new(0.0, -0.1, -150.0),
        ),
    }];
    for b in &blocks {
        draws.push(DrawItem {
            mesh,
            material: gray,
            world_from_object: Mat4::from_scale_rotation_translation(
                b.size,
                Quat::IDENTITY,
                b.center,
            ),
        });
    }
    let eye = Vec3::new(0.0, 5.0, 0.0);
    let clip_from_world = vulkan_perspective(1.0, WIDTH as f32 / HEIGHT as f32, 0.1, 500.0)
        * look_at(eye, Vec3::new(0.0, 0.0, -14.0), Vec3::Y);
    let camera = Camera {
        clip_from_world,
        position: eye,
    };
    let light = DirectionalLight {
        direction: LIGHT,
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.1),
    };

    // The analytic truth for every pixel: the ground point it shows, in shadow or not.
    // Points within `edge` of changing their answer are not judged (the filter blurs
    // edges by a texel or two).
    let inverse = clip_from_world.inverse();
    let to_light = -LIGHT.normalize();
    let truth: Vec<(Truth, f32, Option<usize>)> = (0..WIDTH * HEIGHT)
        .map(|i| {
            let (x, y) = (i % WIDTH, i / WIDTH);
            let ndc = [
                (x as f32 + 0.5) / WIDTH as f32 * 2.0 - 1.0,
                (y as f32 + 0.5) / HEIGHT as f32 * 2.0 - 1.0,
            ];
            let unproject = |z: f32| {
                let p = inverse * Vec4::new(ndc[0], ndc[1], z, 1.0);
                p.truncate() / p.w
            };
            let origin = unproject(0.0);
            let direction = (unproject(1.0) - origin).normalize();
            if direction.y >= -1e-4 {
                return (Truth::Skip, 0.0, None);
            }
            let t = -origin.y / direction.y;
            if blocks
                .iter()
                .any(|b| b.hit(origin, direction).is_some_and(|h| h < t))
            {
                return (Truth::Skip, 0.0, None);
            }
            let ground = origin + direction * t;
            // Beyond the slab: the background.
            if ground.x.abs() > 199.0 || !(-349.0..49.0).contains(&ground.z) {
                return (Truth::Skip, 0.0, None);
            }
            let distance = ground.distance(eye);
            let shadow_of = |p: Vec3| blocks.iter().position(|b| b.hit(p, to_light).is_some());
            let caster = shadow_of(ground);
            // Unjudged near an edge: 1.5 cm near the camera, growing with distance like
            // the cascades' texels.
            let edge = 0.015 + distance * 0.004;
            let ring = [
                Vec3::X,
                Vec3::NEG_X,
                Vec3::Z,
                Vec3::NEG_Z,
                Vec3::new(0.7, 0.0, 0.7),
                Vec3::new(-0.7, 0.0, 0.7),
                Vec3::new(0.7, 0.0, -0.7),
                Vec3::new(-0.7, 0.0, -0.7),
            ];
            if ring
                .iter()
                .any(|r| shadow_of(ground + *r * edge).is_some() != caster.is_some())
            {
                return (Truth::Skip, distance, caster);
            }
            let truth = if caster.is_some() {
                Truth::Shadow
            } else {
                Truth::Lit
            };
            (truth, distance, caster)
        })
        .collect();

    let mut render = |settings: ShadowSettings| -> Score {
        let renderer = ForwardRenderer::new(&device, &bindless, HDR, DEPTH, settings).unwrap();
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
                        light,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
                        lights: &[],
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
        // Lit ground: albedo / π × 3 × n·l + ambient; in shadow: ambient only. The
        // threshold lies halfway.
        let n_dot_l = to_light.y;
        let albedo = 0.5;
        let lit = albedo / core::f32::consts::PI * 3.0 * n_dot_l + 0.1 * albedo;
        let dark = 0.1 * albedo;
        let threshold = (lit + dark) * 0.5;
        let mut score = Score::default();
        for (i, &(truth, distance, caster)) in truth.iter().enumerate() {
            let red = half::to_f32(u16::from_le_bytes([raw[i * 8], raw[i * 8 + 1]]));
            let shadowed = red < threshold;
            let tally = |dark: &mut u32, total: &mut u32| {
                *total += 1;
                *dark += u32::from(shadowed);
            };
            match caster {
                Some(FAR) if truth == Truth::Shadow => {
                    tally(&mut score.far_shadow_dark, &mut score.far_shadow_total);
                }
                Some(OUTSIDE) if truth == Truth::Shadow => {
                    tally(
                        &mut score.outside_shadow_dark,
                        &mut score.outside_shadow_total,
                    );
                }
                _ => {}
            }
            if truth == Truth::Skip {
                continue;
            }
            let wrong = shadowed != (truth == Truth::Shadow);
            if wrong {
                let band = [12.0, 40.0, 100.0, 150.0]
                    .iter()
                    .position(|&d| distance < d)
                    .unwrap_or(4);
                score.by_distance[band][usize::from(truth == Truth::Shadow)] += 1;
            }
            score.total += 1;
            score.wrong += u32::from(wrong);
            if distance < 12.0 {
                score.near_total += 1;
                score.near_wrong += u32::from(wrong);
            }
        }
        score
    };

    let cascaded = render(ShadowSettings {
        resolution: 1024,
        ..ShadowSettings::default()
    });
    let single = render(ShadowSettings {
        resolution: 1024,
        cascades: 1,
        ..ShadowSettings::default()
    });
    println!("4 cascades: {cascaded:?}");
    println!("1 cascade:  {single:?}");
    let rate = |wrong: u32, total: u32| f64::from(wrong) / f64::from(total.max(1));
    assert!(cascaded.near_total > 1000 && cascaded.far_shadow_total > 10);
    assert!(cascaded.outside_shadow_total > 100, "{cascaded:?}");
    assert!(
        rate(cascaded.wrong, cascaded.total) < 0.002,
        "cascades agree with the analytic shadows: {cascaded:?}"
    );
    assert!(
        cascaded.near_wrong * 4 < single.near_wrong.max(1),
        "near the camera, cascades beat one map: {cascaded:?} vs {single:?}"
    );
    assert!(
        rate(cascaded.far_shadow_dark, cascaded.far_shadow_total) > 0.9,
        "the box 90 m away casts: {cascaded:?}"
    );
    assert!(
        rate(cascaded.outside_shadow_dark, cascaded.outside_shadow_total) > 0.95,
        "a caster outside the view casts into it: {cascaded:?}"
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
