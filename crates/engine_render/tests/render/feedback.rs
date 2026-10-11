//! Texture feedback (ADR 0076), under the validation layer with synchronization
//! validation: the levels the feedback pass reports are the levels the sampler uses.
//!
//! Four quads face the camera at a known distance, each with its own material and texture
//! coordinates repeating a different number of times, so each samples its texture at a
//! known level: `log2(texture size × texture coordinates per pixel)`. The texture's levels
//! hold their own number (red = 16 × level), and the materials show it as emission, so
//! the rendered image says which levels the hardware sampled (trilinear: fractions too). The
//! feedback must report each material's level within a tenth of a level of the analytic
//! one, and the levels the sampler read must be the one it asks for and the next. A fifth
//! quad hidden behind one of them must report nothing.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::feedback::{FeedbackSettings, TextureFeedback};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable, TextureRef};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData};
use engine_render::texture_pool::{TextureFormat, TextureLevels, TexturePool};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render feedback test"),
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

/// The view: SIZE² pixels, 60° vertical field of view.
const SIZE: u32 = 512;
/// The texture: TEXTURE² texels.
const TEXTURE: u32 = 256;
/// Distance of the quads (1 unit wide, facing the camera).
const DISTANCE: f32 = 4.0;
/// Texture-coordinate repeats across each quad: levels 0.5, 1.5, 2.5 and 3.5.
const REPEATS: [f32; 4] = [0.612_372_4, 1.224_744_9, 2.449_489_7, 4.898_979_5];
const FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// Positions, normals, texture coordinates and indices of a mesh.
type Quad = (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<[f32; 2]>, Vec<u32>);

/// A unit quad in the xy plane facing +z, its texture coordinates spanning `repeats`.
fn quad(repeats: f32) -> Quad {
    let positions = vec![
        [-0.5, -0.5, 0.0],
        [0.5, -0.5, 0.0],
        [0.5, 0.5, 0.0],
        [-0.5, 0.5, 0.0],
    ];
    let normals = vec![[0.0, 0.0, 1.0]; 4];
    let uvs = vec![
        [0.0, repeats],
        [repeats, repeats],
        [repeats, 0.0],
        [0.0, 0.0],
    ];
    (positions, normals, uvs, vec![0, 1, 2, 0, 2, 3])
}

#[test]
fn feedback_reports_the_levels_the_sampler_uses() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let depth_format = vk::Format::D32_SFLOAT;
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut feedback =
        // Every command in each pass (the default spreads them over four).
        TextureFeedback::new(
            &device,
            &bindless,
            FeedbackSettings {
                slices: 1,
                ..FeedbackSettings::default()
            },
            1,
        )
        .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mut textures = TexturePool::new(&device, 64 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let levels = 32 - TEXTURE.leading_zeros();
    let mut draws = Vec::new();
    let mut slots = Vec::new();
    // Quads on a 2 × 2 grid, and one hidden behind the second.
    let places = [
        Vec3::new(-0.7, -0.7, -DISTANCE),
        Vec3::new(0.7, -0.7, -DISTANCE),
        Vec3::new(-0.7, 0.7, -DISTANCE),
        Vec3::new(0.7, 0.7, -DISTANCE),
        Vec3::new(0.7, -0.7, -DISTANCE) * 1.5,
    ];
    for (k, place) in places.iter().enumerate() {
        let (positions, normals, uvs, indices) = quad(REPEATS[k.min(3)]);
        let mesh = geometry
            .add_mesh(&MeshData {
                positions: &positions,
                normals: &normals,
                tangents: &[],
                uvs: &uvs,
                indices: &indices,
            })
            .unwrap();
        let texture = textures
            .add(
                &device,
                &mut allocator,
                TextureLevels {
                    width: TEXTURE,
                    height: TEXTURE,
                    format: TextureFormat::Rgba8Unorm,
                    levels: (0..levels)
                        .map(|l| {
                            let s = (TEXTURE >> l).max(1) as usize;
                            [16 * l as u8, 0, 0, 255].repeat(s * s)
                        })
                        .collect(),
                },
            )
            .unwrap();
        let material = materials.add(MaterialDesc {
            base_color: [0.0, 0.0, 0.0, 1.0],
            emissive: [1.0; 3],
            emissive_texture: Some(TextureRef::new(texture)),
            ..MaterialDesc::default()
        });
        slots.push(materials.slot_of(material).unwrap());
        draws.push(DrawItem {
            mesh,
            material,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::ONE,
                Quat::IDENTITY,
                *place,
            ),
        });
    }
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let fov = 60f32.to_radians();
    let camera = Camera {
        clip_from_world: vulkan_perspective(fov, 1.0, 0.1, 100.0)
            * look_at(Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0), Vec3::Y),
        position: Vec3::ZERO,
    };
    let light = DirectionalLight {
        direction: Vec3::new(0.0, -1.0, 0.0),
        color: Vec3::ZERO,
        ambient: Vec3::ZERO,
    };
    let image_bytes = u64::from(SIZE * SIZE) * 8;
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(image_bytes)
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

    // Frame 1 uploads the textures (they are sampled from the frame they arrive in) and
    // draws with feedback; frame 2 polls it.
    for frame_number in 1..=3u64 {
        assert!(feedback.poll(frame_number).is_none() || frame_number >= 2);
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
            let color = fg.create_texture("color", TextureDesc::new_2d(FORMAT, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(depth_format, SIZE, SIZE));
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
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
                        lights: &[],
                        decals: &[],
                        environment: None,
                        reflections: Default::default(),
                        gi: None,
                    },
                )
                .unwrap();
            feedback
                .add_pass(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &bindless,
                    &geo,
                    &mats,
                    &passes,
                    (SIZE, SIZE),
                    0.0,
                    frame_number,
                )
                .unwrap();
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: readback,
                    size: image_bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            fg.add_pass("read back")
                .read(color, Access::TransferRead)
                .write(out, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; the image is in
                    // TRANSFER_SRC layout for this pass.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(color),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[vk::BufferImageCopy {
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
                            }],
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
    // The last frame's results (one frame in flight: frame 2's are ready at frame 3).
    assert_eq!(feedback.poll(4), Some(3));
    let results: Vec<_> = feedback.results().collect();
    println!("feedback: {results:?}");

    // The image: each quad's center pixel holds 16 × the level sampled.
    let ptr = readback_mem.mapped_ptr().unwrap();
    // SAFETY: the GPU is idle; the mapping covers the image.
    let pixels = unsafe {
        core::slice::from_raw_parts(ptr.as_ptr().cast::<u16>(), (SIZE * SIZE * 4) as usize)
    };
    // Pixels per unit at distance 1: the projection's scale × half the height.
    let pixels_per_unit = SIZE as f32 * 0.5 / (fov * 0.5).tan();
    let mut checks = Vec::new();
    for (k, &repeats) in REPEATS.iter().enumerate() {
        let analytic = (TEXTURE as f32 * repeats * DISTANCE / pixels_per_unit).log2();
        let p = places[k];
        let x = ((p.x / DISTANCE * pixels_per_unit) + SIZE as f32 * 0.5) as u32;
        // Vulkan's projection: y down.
        let y = ((-p.y / DISTANCE * pixels_per_unit) + SIZE as f32 * 0.5) as u32;
        let sampled = half_to_f32(pixels[((y * SIZE + x) * 4) as usize]) * 255.0 / 16.0;
        let reported = results
            .iter()
            .find(|r| r.slot == slots[k])
            .map(|r| r.level + (TEXTURE as f32).log2())
            .unwrap_or(f32::NAN);
        println!(
            "repeats {repeats}: analytic level {analytic:.3}, sampled {sampled:.3}, feedback {reported:.3}"
        );
        checks.push((reported, analytic, sampled));
    }
    for (reported, analytic, sampled) in checks {
        assert!(
            (reported - analytic).abs() < 0.1,
            "{reported} vs {analytic}"
        );
        // The sampler blends the two levels around its level (drivers differ in how:
        // AMD's here blends linearly, NVIDIA's reaches the coarser level halfway, a
        // "trilinear optimization"): what it reads lies between the level the feedback
        // asks for (rounded down) and the next.
        let asked = reported.floor();
        assert!(
            sampled >= asked - 0.02 && sampled <= asked + 1.02,
            "sampled {sampled} outside levels {asked} and {}",
            asked + 1.0
        );
    }
    assert!(
        results.iter().all(|r| r.slot != slots[4]),
        "the hidden quad reported nothing"
    );

    // SAFETY: the device is idle.
    unsafe {
        feedback.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
    drop(feedback);
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

/// An IEEE half's value.
fn half_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((bits >> 10) & 0x1F);
    let mantissa = f32::from(bits & 0x3FF);
    sign * match exponent {
        0 => mantissa / 1024.0 * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + mantissa / 1024.0) * 2f32.powi(e - 15),
    }
}
