//! FSR 3.1's frame interpolation (ADR 0091) on the upscaling tests' scene, as the frame
//! renderer will run it: the tone mapper writes an sRGB image, which the SDK reads through
//! a UNORM view (the encoded values), and the generated frame is written UNORM.
//!
//! The camera pans at a constant speed. The frames rendered at every half step are the
//! truth; frame generation sees every second one only. A generated frame must be closer to
//! the frame rendered half-way than either of the frames it was made from, and the first
//! frame after a reset must be the new frame itself.

use super::*;
use engine_fsr::{
    CameraBasis, FrameInterpolation, InterpolationInput, InterpolationOutput, Transfer,
};
use engine_render::graph::{ImportedTexture, TextureState};
use engine_render::post::Tonemap;

/// The display size: the optical flow works on 8 × 8 blocks over 7 pyramid levels.
const DISPLAY: u32 = 256;
/// The tone mapper's output, as an SDR swapchain's.
const LDR: vk::Format = vk::Format::B8G8R8A8_SRGB;
/// The same bytes as the SDK reads and writes them.
const LDR_RAW: vk::Format = vk::Format::B8G8R8A8_UNORM;
/// The camera's step between rendered frames (frame generation sees every second one):
/// about 5 pixels at the scene's depth.
const STEP: f32 = 0.25;

struct Image {
    image: vk::Image,
    view: vk::ImageView,
    memory: Allocation,
}

/// The images a frame is drawn into and read back from.
struct Targets {
    /// The tone mapper's output: sRGB, viewed UNORM by the SDK.
    source: Image,
    /// The generated frame: UNORM storage, also sampled.
    generated: Image,
    readback: vk::Buffer,
    readback_memory: Allocation,
}

impl Targets {
    fn new(gpu: &mut Gpu) -> Self {
        let formats = [LDR, LDR_RAW];
        let mut list = vk::ImageFormatListCreateInfo::default().view_formats(&formats);
        let source = image(
            gpu,
            &vk::ImageCreateInfo::default()
                .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
                .format(LDR)
                .usage(
                    vk::ImageUsageFlags::COLOR_ATTACHMENT
                        | vk::ImageUsageFlags::SAMPLED
                        | vk::ImageUsageFlags::TRANSFER_SRC,
                )
                .push_next(&mut list),
        );
        let generated = image(
            gpu,
            &vk::ImageCreateInfo::default()
                .format(LDR_RAW)
                // The SDK's inpainting reads the output back.
                .usage(
                    vk::ImageUsageFlags::STORAGE
                        | vk::ImageUsageFlags::SAMPLED
                        | vk::ImageUsageFlags::TRANSFER_SRC,
                ),
        );
        let (readback, readback_memory) = gpu
            .allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(u64::from(2 * DISPLAY * DISPLAY * 4))
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();
        Self {
            source,
            generated,
            readback,
            readback_memory,
        }
    }

    fn destroy(self, gpu: &mut Gpu) {
        for image in [self.source, self.generated] {
            // SAFETY: the device is idle; each image came from `image` with its memory.
            unsafe {
                gpu.device.raw().destroy_image_view(image.view, None);
                gpu.allocator.destroy_image(image.image, image.memory);
            }
        }
        // SAFETY: as above.
        unsafe {
            gpu.allocator
                .destroy_buffer(self.readback, self.readback_memory);
        }
    }
}

/// A `DISPLAY`-sized 2D image of `info`'s format and usage, with a view in that format.
fn image(gpu: &mut Gpu, info: &vk::ImageCreateInfo<'_>) -> Image {
    let info = info
        .image_type(vk::ImageType::TYPE_2D)
        .extent(vk::Extent3D {
            width: DISPLAY,
            height: DISPLAY,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL);
    let (image, memory) = gpu
        .allocator
        .create_image(&info, MemoryLocation::Device)
        .unwrap();
    // SAFETY: a view of the whole new image, in its own format.
    let view = unsafe {
        gpu.device.raw().create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(info.format)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                }),
            None,
        )
    }
    .unwrap();
    Image {
        image,
        view,
        memory,
    }
}

fn import(
    fg: &mut engine_render::graph::FrameGraph<'_>,
    name: &'static str,
    image: &Image,
    format: vk::Format,
) -> Texture {
    fg.import_texture(
        name,
        ImportedTexture {
            image: image.image,
            view: image.view,
            desc: TextureDesc::new_2d(format, DISPLAY, DISPLAY),
            initial: TextureState::undefined(),
            final_state: Some(TextureState::transfer_source()),
            sampled_index: None,
            storage_index: None,
        },
    )
}

/// The camera at `x` (the upscaling tests' camera) and its basis.
fn camera_at(x: f32) -> (Camera, CameraBasis) {
    let eye = Vec3::new(x, 2.0, 7.0);
    let target = Vec3::new(x, 0.5, 0.0);
    let camera = Camera {
        clip_from_world: vulkan_perspective(FOV_Y, 1.0, NEAR, FAR) * look_at(eye, target, Vec3::Y),
        position: eye,
    };
    let forward = (target - eye).normalize();
    let right = forward.cross(Vec3::Y).normalize();
    let up = right.cross(forward);
    let basis = CameraBasis {
        position: eye.to_array(),
        up: up.to_array(),
        right: right.to_array(),
        forward: forward.to_array(),
    };
    (camera, basis)
}

/// Draws the frame seen from `x` and, with `interpolation`, generates the frame between the
/// previous one and it. Returns both images' encoded values (0..1).
fn frame(
    gpu: &mut Gpu,
    tonemap: &Tonemap,
    targets: &Targets,
    draws: &[DrawItem],
    temporal: &Temporal,
    interpolation: Option<&mut FrameInterpolation>,
    x: f32,
) -> (Vec<[f32; 3]>, Option<Vec<[f32; 3]>>) {
    let Gpu {
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
        frame_number,
        ..
    } = gpu;
    let d = device.raw();
    let cmd = *cmd;
    let (camera, basis) = camera_at(x);
    let light = DirectionalLight {
        direction: Vec3::new(-0.4, -1.0, -0.3),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.1),
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
    *frame_number += 1;
    let generate = interpolation.is_some();
    let bytes = u64::from(DISPLAY * DISPLAY * 4);
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
        let color = fg.create_texture("color", TextureDesc::new_2d(HDR, DISPLAY, DISPLAY));
        let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, DISPLAY, DISPLAY));
        let passes = renderer
            .add_passes(
                &mut fg,
                ring,
                bindless,
                &geo,
                &mats,
                draws,
                &ForwardView {
                    camera,
                    light,
                    color,
                    depth,
                    clear: Some([0.02, 0.02, 0.03, 1.0]),
                    occlusion: None,
                    temporal: Some(temporal),
                    shadow_cache: None,
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
        let motion = passes.motion.unwrap();
        let source = import(&mut fg, "source", &targets.source, LDR);
        tonemap.add_pass(&mut fg, bindless, color, source, 1.0);
        let generated = import(&mut fg, "generated", &targets.generated, LDR_RAW);
        if let Some(interpolation) = interpolation {
            interpolation
                .add_pass(
                    &mut fg,
                    InterpolationInput {
                        source,
                        source_format: LDR_RAW,
                        transfer: Transfer::Srgb,
                        depth,
                        motion,
                        camera: FsrCamera {
                            near: NEAR,
                            far: FAR,
                            fov_y: FOV_Y,
                            meters_per_unit: 1.0,
                        },
                        basis,
                        frame_time_ms: 1000.0 / 60.0,
                    },
                    InterpolationOutput {
                        texture: generated,
                        format: LDR_RAW,
                    },
                    *frame_number,
                )
                .unwrap();
        }
        let out = fg.import_buffer(
            "readback",
            ImportedBuffer {
                buffer: targets.readback,
                size: 2 * bytes,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        let mut pass = fg
            .add_pass("read back")
            .read(source, Access::TransferRead)
            .write(out, Access::TransferWrite);
        if generate {
            pass = pass.read(generated, Access::TransferRead);
        }
        pass.execute(move |ctx| {
            let region = |offset: u64| vk::BufferImageCopy {
                buffer_offset: offset,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_extent: vk::Extent3D {
                    width: DISPLAY,
                    height: DISPLAY,
                    depth: 1,
                },
                ..Default::default()
            };
            // SAFETY: recording; layouts arranged by the graph.
            unsafe {
                ctx.device.cmd_copy_image_to_buffer(
                    ctx.cmd,
                    ctx.image(source),
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ctx.buffer(out),
                    &[region(0)],
                );
                if generate {
                    ctx.device.cmd_copy_image_to_buffer(
                        ctx.cmd,
                        ctx.image(generated),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        ctx.buffer(out),
                        &[region(bytes)],
                    );
                }
            }
        });
        fg.execute_with_bindless(allocator, Some(bindless)).unwrap();
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
    let ptr = targets.readback_memory.mapped_ptr().unwrap();
    // SAFETY: the GPU is idle; the mapping covers both images.
    let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), 2 * bytes as usize) };
    let decode = |bytes: &[u8]| -> Vec<[f32; 3]> {
        bytes
            .chunks_exact(4)
            .map(|p| [p[2], p[1], p[0]].map(|v| f32::from(v) / 255.0))
            .collect()
    };
    let (source, generated) = raw.split_at(bytes as usize);
    (decode(source), generate.then(|| decode(generated)))
}

#[test]
fn a_generated_frame_is_closer_to_the_frame_between_than_its_sources() {
    let Some((instance, mut gpu, mesh, temporal)) = setup() else {
        return;
    };
    let bright = gpu
        .materials
        .add(MaterialDesc::solid([0.9, 0.8, 0.6], 0.0, 0.3));
    let slab = gpu
        .materials
        .add(MaterialDesc::solid([0.3, 0.35, 0.4], 0.0, 0.8));
    let mut draws = vec![ground(mesh, slab)];
    for i in 0..7 {
        let a = i as f32;
        draws.push(DrawItem {
            mesh,
            material: bright,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::new(0.8, 1.6 + 0.3 * (a % 3.0), 0.8),
                Quat::from_rotation_y(0.3 + a * 0.47) * Quat::from_rotation_z(0.05 * a),
                Vec3::new(-3.0 + a, 0.8, -1.0 - (a % 2.0) * 1.5),
            ),
        });
    }
    let tonemap = Tonemap::new(&gpu.device, &gpu.bindless, LDR).unwrap();
    let targets = Targets::new(&mut gpu);
    let mut interpolation = FrameInterpolation::new(&gpu.device, 1);
    // No jitter: the frames compared must differ only by the camera's position.
    temporal.set_jitter_phases(0);

    // The truth: a frame at every half step.
    let frames = 12;
    temporal.reset();
    let truth: Vec<Vec<[f32; 3]>> = (0..=2 * frames)
        .map(|k| {
            frame(
                &mut gpu,
                &tonemap,
                &targets,
                &draws,
                &temporal,
                None,
                k as f32 * STEP / 2.0,
            )
            .0
        })
        .collect();

    // Frame generation sees the whole steps only.
    temporal.reset();
    let mut scores = Vec::new();
    let mut previous: Option<Vec<[f32; 3]>> = None;
    for k in 0..=frames {
        let (real, generated) = frame(
            &mut gpu,
            &tonemap,
            &targets,
            &draws,
            &temporal,
            Some(&mut interpolation),
            k as f32 * STEP,
        );
        let generated = generated.unwrap();
        assert!(
            psnr(&real, &truth[2 * k]) > 40.0,
            "the same frame drawn twice"
        );
        match previous {
            // The first frame starts the interpolation: the SDK writes the frame itself.
            None => {
                let db = psnr(&generated, &real);
                println!("first frame: generated against the frame itself {db:.2} dB");
                assert!(
                    db > 40.0,
                    "the first generated frame is the frame: {db:.2} dB"
                );
            }
            Some(previous) => {
                let between = &truth[2 * k - 1];
                scores.push((
                    psnr(&generated, between),
                    psnr(&previous, between),
                    psnr(&real, between),
                ));
            }
        }
        previous = Some(real);
    }
    for (k, (generated, previous, next)) in scores.iter().enumerate() {
        println!(
            "between {} and {}: generated {generated:.2} dB, previous {previous:.2} dB, next {next:.2} dB",
            k,
            k + 1
        );
    }
    // 2026-10-11: 33.2–37.0 dB against 20.1–22.7 for the closer source, on both GPUs.
    for (generated, previous, next) in &scores {
        assert!(
            *generated > previous.max(*next) + 8.0,
            "generated frames are closer to the frame between: {scores:?}"
        );
    }
    println!(
        "frame interpolation: {} bytes of GPU memory",
        interpolation.gpu_memory()
    );
    // SAFETY: the device is idle.
    unsafe { interpolation.destroy() };
    assert_eq!(FrameInterpolation::dispatch_failures(), 0);
    targets.destroy(&mut gpu);
    drop(tonemap);
    teardown(instance, gpu, temporal);
}
