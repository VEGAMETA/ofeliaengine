//! What playing a video costs a frame (ADR 0100, 8b-3): a WebM file played through
//! `VideoPlayer` at 60 simulated frames a second, each frame's work timed: the CPU's
//! `update` (clock, decoding ahead, the picture due) and `add_pass` (the decode wait's
//! submission, the conversion's declaration), the conversion pass's GPU time, and how many
//! pictures were shown or skipped. Prints Markdown.
//!
//! `cargo run -p engine_video --release --example player_bench -- <files> [--software]`
//! (`--software`: rav1d instead of the GPU's video engine; `ENGINE_ADAPTER` picks the
//! GPU). The device idles after every frame, so a picture decoded in one frame is shown in
//! the next at the latest, and the GPU times are the frames' own.

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::undocumented_unsafe_blocks
)]

use std::sync::Arc;
use std::time::Instant;

use engine_render::graph::RenderGraph;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, ExtraQueues, Instance, InstanceDesc,
};
use engine_video::convert::Converter;
use engine_video::player::{Tick, VideoDesc, VideoEvent, VideoPlayer};

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let mut files = Vec::new();
    let mut software = false;
    let mut load = 0;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--software" => software = true,
            "--load" => load = args.next().expect("--load 8").parse().expect("a number"),
            _ => files.push(arg),
        }
    }
    assert!(!files.is_empty(), "give WebM files to play");
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("player_bench"),
        validation: false,
        ..InstanceDesc::default()
    })
    .unwrap();
    let adapter = instance
        .select_adapter(
            &AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance),
        )
        .unwrap();
    let device = Device::with_extras(
        &instance,
        &adapter,
        &[],
        ExtraQueues {
            video_decode: !software,
            ..ExtraQueues::default()
        },
    )
    .unwrap();
    println!("{}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let converter = Converter::new(&device, &bindless).unwrap();
    let d = device.raw();
    let pool = unsafe {
        d.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(device.graphics.family),
            None,
        )
    }
    .unwrap();
    let cmd = unsafe {
        d.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .command_buffer_count(1),
        )
    }
    .unwrap()[0];

    println!(
        "| file | decoder | pictures shown / skipped | conversion GPU µs, median / max | update CPU µs, median / p99 / max | add_pass CPU µs, median / max |"
    );
    println!("|---|---|---:|---:|---:|---:|");
    let mut frame = 0u64;
    for file in &files {
        let data: Arc<[u8]> = std::fs::read(file).unwrap().into();
        let desc = VideoDesc {
            sound: false,
            gpu: !software,
            ..VideoDesc::default()
        };
        let mut video =
            VideoPlayer::open(&device, &mut allocator, &bindless, data, &desc, 1).unwrap();
        let decoder = if video.decodes_on_gpu() {
            "GPU"
        } else {
            "rav1d"
        };
        let (mut update_us, mut pass_us, mut convert_us) = (Vec::new(), Vec::new(), Vec::new());
        let mut shown = 0usize;
        let mut last: Option<f64> = None;
        let mut skipped = 0usize;
        // The pictures' spacing (the clips are 30 a second), for counting skipped ones.
        let spacing = 1.0 / 30.0;
        loop {
            frame += 1;
            let t = Instant::now();
            video
                .update(
                    &device,
                    &mut allocator,
                    &bindless,
                    Tick {
                        frame,
                        in_flight: 1,
                        dt: 1.0 / 60.0,
                        audio: None,
                        latency: 0.0,
                    },
                )
                .unwrap();
            update_us.push(t.elapsed().as_secs_f64() * 1e6);
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
            {
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                let t = Instant::now();
                video
                    .add_pass(&mut fg, &converter, &bindless, &device, frame, 0)
                    .unwrap();
                pass_us.push(t.elapsed().as_secs_f64() * 1e6);
                // After the conversion (a fence: it runs first), so its time is its own.
                add_load(&mut fg, load);
                fg.execute_with_bindless(&mut allocator, Some(&bindless))
                    .unwrap();
            }
            // The previous frame's pass times, collected as its slot came around.
            convert_us.extend(conversion_us(&graph));
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
            let mut ended = false;
            for event in video.events() {
                match event {
                    VideoEvent::Shown(time) => {
                        if let Some(previous) = last {
                            skipped +=
                                (((time - previous) / spacing).round() as usize).saturating_sub(1);
                        }
                        last = Some(time);
                        shown += 1;
                    }
                    VideoEvent::Ended => ended = true,
                    VideoEvent::Looped => {}
                }
            }
            if ended {
                break;
            }
        }
        // The last frame's pass, read by a frame of nothing.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
            graph
                .begin_slot(0, cmd)
                .execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
            d.end_command_buffer(cmd).unwrap();
        }
        convert_us.extend(conversion_us(&graph));
        let file_name = std::path::Path::new(file)
            .file_name()
            .map_or_else(|| file.clone(), |n| n.to_string_lossy().into_owned());
        let (update_median, update_p99, update_max) = (
            percentile(&mut update_us, 0.5),
            percentile(&mut update_us, 0.99),
            percentile(&mut update_us, 1.0),
        );
        let (pass_median, pass_max) =
            (percentile(&mut pass_us, 0.5), percentile(&mut pass_us, 1.0));
        let (convert_median, convert_max) = (
            percentile(&mut convert_us, 0.5),
            percentile(&mut convert_us, 1.0),
        );
        println!(
            "| {file_name} | {decoder} | {shown} / {skipped} | {convert_median:.1} / {convert_max:.1} | {update_median:.1} / {update_p99:.1} / {update_max:.1} | {pass_median:.1} / {pass_max:.1} |"
        );
        unsafe { video.destroy(&device, &mut allocator, &bindless, None, frame) };
    }
    unsafe {
        device.wait_idle().unwrap();
        graph.destroy(&mut allocator, Some(&bindless));
        d.destroy_command_pool(pool, None);
    }
    drop(converter);
    drop(bindless);
    drop(allocator);
    // `ENGINE_PIPELINE_STATS=1`: the conversion's registers and instructions.
    for record in engine_rhi::take_pipeline_records() {
        if record
            .stages
            .iter()
            .any(|(entry, _)| entry == "convertPicture")
        {
            for statistic in &record.statistics {
                println!("{}: {}", statistic.name, statistic.value);
            }
        }
    }
}

/// `count` clears of a 4096 × 4096 half-float image after the conversion: work that keeps
/// the GPU's clocks up from frame to frame, as a game's does (`--load`).
fn add_load(fg: &mut engine_render::graph::FrameGraph<'_>, count: u32) {
    if count == 0 {
        return;
    }
    let image = fg.create_texture(
        "load",
        engine_render::graph::TextureDesc::new_2d(vk::Format::R16G16B16A16_SFLOAT, 4096, 4096),
    );
    for k in 0..count {
        fg.add_pass("load")
            .write(image, engine_render::graph::Access::TransferWrite)
            .side_effects()
            .execute(move |ctx| {
                let value = vk::ClearColorValue {
                    float32: [k as f32, 0.0, 0.0, 1.0],
                };
                let range = vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                unsafe {
                    ctx.device.cmd_clear_color_image(
                        ctx.cmd,
                        ctx.image(image),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &value,
                        &[range],
                    );
                }
            });
    }
}

/// The conversion pass's GPU time in the frame graph's last completed frame, µs.
fn conversion_us(graph: &RenderGraph) -> Option<f64> {
    graph
        .gpu_pass_times()
        .iter()
        .find(|t| t.name == "video conversion")
        .map(|t| f64::from(t.gpu_us))
}
