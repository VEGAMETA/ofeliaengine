//! Does a device that draws with MSAA survive another device working in the same process?
//! On the Radeon iGPU (AMD 26.8.1) a golden shot drawing with MSAA and texture streaming
//! loses its device when a second test's device works in the same process, while separate
//! processes and the RTX are fine (`docs/status.md` §6). Each thread here makes its own
//! instance and device, as those tests do, and draws the same frame again and again: a
//! triangle into a 1920 × 1080 RGBA16F target, multisampled and resolved (`msaa4`,
//! `msaa8`), single-sampled (`plain`), or a clear only (`clear`).
//!
//! On 2026-10-14 every mix passed on the Radeon (600 frames each): a bare MSAA pass with
//! a resolve is not enough, so the golden shots' loss needs more of the engine's path
//! (it does not happen without their texture streaming).
//!
//! ```text
//! cargo run -p engine_rhi --release --example multi_device_msaa -- [first] [second] [frames]
//!     first, second: msaa4 | msaa8 | plain | clear | none (default: msaa4 clear 600)
//! ```
//!
//! (`ENGINE_ADAPTER=radeon` for the iGPU.) Each frame is submitted and waited for, so the
//! frame a device was lost in is the one printed.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Device, GraphicsPipelineDesc, Instance, InstanceDesc,
    MemoryLocation, Pipeline, ShaderStageDesc, spirv_words,
};
use std::sync::Barrier;
use std::time::Instant;

const VERTEX: &[u8] = include_bytes!("../shaders/triangle.vert.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/triangle.frag.spv");
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// What a thread's device does every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    /// A triangle at this many samples, resolved into the target.
    Msaa(u32),
    /// A triangle, single-sampled.
    Plain,
    /// A clear of the target only.
    Clear,
    /// No thread.
    Nothing,
}

fn work(name: &str) -> Work {
    match name {
        "msaa4" => Work::Msaa(4),
        "msaa8" => Work::Msaa(8),
        "plain" => Work::Plain,
        "clear" => Work::Clear,
        "none" => Work::Nothing,
        other => panic!("unknown work `{other}`: msaa4 | msaa8 | plain | clear | none"),
    }
}

fn subresource() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// Draws `frames` frames of `work` on a device of its own; the frames drawn, or where the
/// device was lost.
fn run(name: &str, work: Work, frames: u32, start: &Barrier) -> Result<u32, String> {
    let fail = |what: &str, e: &dyn std::fmt::Display| format!("{name}: {what}: {e}");
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from(name),
        validation: false,
        debug_utils: false,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .map_err(|e| fail("instance", &e))?;
    let adapter = instance
        .select_adapter(
            &AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance),
        )
        .map_err(|e| fail("adapter", &e))?;
    let device = Device::new(&instance, &adapter).map_err(|e| fail("device", &e))?;
    let mut allocator = Allocator::new(&device);
    let d = device.raw().clone();
    let samples = match work {
        Work::Msaa(n) => vk::SampleCountFlags::from_raw(n),
        _ => vk::SampleCountFlags::TYPE_1,
    };
    let image_info = |samples: vk::SampleCountFlags, usage: vk::ImageUsageFlags| {
        vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width: WIDTH,
                height: HEIGHT,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(samples)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED)
    };
    let (target, target_memory) = allocator
        .create_image(
            &image_info(
                vk::SampleCountFlags::TYPE_1,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
            ),
            MemoryLocation::Device,
        )
        .map_err(|e| fail("target", &e))?;
    let multisampled = match work {
        Work::Msaa(_) => Some(
            allocator
                .create_image(
                    &image_info(samples, vk::ImageUsageFlags::COLOR_ATTACHMENT),
                    MemoryLocation::Device,
                )
                .map_err(|e| fail("multisampled target", &e))?,
        ),
        _ => None,
    };
    let view = |image: vk::Image| {
        // SAFETY: a live image of this device.
        unsafe {
            d.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(FORMAT)
                    .subresource_range(subresource()),
                None,
            )
        }
        .unwrap()
    };
    let target_view = view(target);
    let multisampled_view = multisampled.as_ref().map(|(image, _)| view(*image));
    // SAFETY: an empty layout on a live device.
    let layout =
        unsafe { d.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None) }
            .unwrap();
    let (vertex, fragment) = (spirv_words(VERTEX).unwrap(), spirv_words(FRAGMENT).unwrap());
    let formats = [FORMAT];
    let pipeline = Pipeline::graphics(
        &device,
        layout,
        &GraphicsPipelineDesc {
            color_formats: &formats,
            depth_test: false,
            depth_write: false,
            cull_mode: vk::CullModeFlags::NONE,
            samples,
            ..GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: &vertex,
                    entry: c"vertexMain",
                },
                Some(ShaderStageDesc {
                    spirv: &fragment,
                    entry: c"fragmentMain",
                }),
            )
        },
    )
    .map_err(|e| fail("pipeline", &e))?;
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
    start.wait();
    let started = Instant::now();
    let mut result = Ok(frames);
    for frame in 0..frames {
        // SAFETY: the previous frame finished (waited below); every handle is this device's.
        let done = unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
            let to_attachment = |image: vk::Image| {
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                    .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .image(image)
                    .subresource_range(subresource())
            };
            let mut barriers = vec![to_attachment(target)];
            if let Some((image, _)) = &multisampled {
                barriers.push(to_attachment(*image));
            }
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&barriers),
            );
            let clear = vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.1, 0.1, 0.1, 1.0],
                },
            };
            let attachment = match multisampled_view {
                Some(ms) => vk::RenderingAttachmentInfo::default()
                    .image_view(ms)
                    .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .resolve_mode(vk::ResolveModeFlags::AVERAGE)
                    .resolve_image_view(target_view)
                    .resolve_image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::CLEAR)
                    .store_op(vk::AttachmentStoreOp::DONT_CARE)
                    .clear_value(clear),
                None => vk::RenderingAttachmentInfo::default()
                    .image_view(target_view)
                    .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .load_op(vk::AttachmentLoadOp::CLEAR)
                    .store_op(vk::AttachmentStoreOp::STORE)
                    .clear_value(clear),
            };
            let area = vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: WIDTH,
                    height: HEIGHT,
                },
            };
            let attachments = [attachment];
            d.cmd_begin_rendering(
                cmd,
                &vk::RenderingInfo::default()
                    .render_area(area)
                    .layer_count(1)
                    .color_attachments(&attachments),
            );
            if work != Work::Clear {
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline.raw());
                d.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: WIDTH as f32,
                        height: HEIGHT as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                d.cmd_set_scissor(cmd, 0, &[area]);
                d.cmd_set_cull_mode(cmd, vk::CullModeFlags::NONE);
                d.cmd_set_front_face(cmd, vk::FrontFace::COUNTER_CLOCKWISE);
                d.cmd_draw(cmd, 3, 1, 0, 0);
            }
            d.cmd_end_rendering(cmd);
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                vk::Fence::null(),
            )
            .and_then(|()| d.device_wait_idle())
        };
        if let Err(e) = done {
            result = Err(format!("{name} ({work:?}): frame {frame}: {e}"));
            break;
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    println!("{name} ({work:?}): {:.2} s", seconds);
    // SAFETY: the device is idle (or lost); everything below was made from it.
    unsafe {
        let _ = d.device_wait_idle();
        d.destroy_command_pool(pool, None);
        drop(pipeline);
        d.destroy_pipeline_layout(layout, None);
        d.destroy_image_view(target_view, None);
        if let Some(view) = multisampled_view {
            d.destroy_image_view(view, None);
        }
        if let Some((image, memory)) = multisampled {
            allocator.destroy_image(image, memory);
        }
        allocator.destroy_image(target, target_memory);
    }
    drop(allocator);
    drop(device);
    result
}

fn main() {
    let mut args = std::env::args().skip(1);
    let first = work(&args.next().unwrap_or_else(|| String::from("msaa4")));
    let second = work(&args.next().unwrap_or_else(|| String::from("clear")));
    let frames: u32 = args.next().map_or(600, |f| f.parse().unwrap());
    let threads = [first, second]
        .into_iter()
        .filter(|w| *w != Work::Nothing)
        .count();
    let start = Barrier::new(threads.max(1));
    let results: Vec<Result<u32, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = [("first", first), ("second", second)]
            .into_iter()
            .filter(|(_, w)| *w != Work::Nothing)
            .map(|(name, w)| {
                let start = &start;
                scope.spawn(move || run(name, w, frames, start))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for r in &results {
        match r {
            Ok(n) => println!("ok: {n} frames"),
            Err(e) => println!("LOST: {e}"),
        }
    }
}
