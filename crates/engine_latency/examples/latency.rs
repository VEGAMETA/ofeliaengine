//! GPU-bound frames with latency reduction off and on, for each available backend.
//! Prints frame time and input-to-display latency per configuration (Markdown).
//!
//! `cargo run -p engine_latency --example latency --release [frames] [fills]`
//!
//! The GPU load is `fills` fills of a 32 MiB buffer per frame (real memory traffic,
//! unlike clears that GPUs can do as metadata). The CPU work is tiny, so without
//! latency control the CPU runs ahead and frames queue up; with it, the queue stays
//! short. Input-to-display is measured with `VK_KHR_present_wait`, polled once per frame
//! (so it can read up to one frame interval high). On NVIDIA, Reflex's own report adds
//! input-to-GPU-end.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

#[cfg(not(windows))]
fn main() {
    println!("This example runs on Windows only.");
}

#[cfg(windows)]
fn main() {
    use engine_latency::{Backend, Latency, LatencySettings, LowLatency, Marker, NvFrameReport};
    use engine_platform::window::{WindowDesc, WindowEventKind, WindowSystem};
    use engine_rhi::ash::vk;
    use engine_rhi::{
        AdapterPreference, Allocator, Device, Frames, Instance, InstanceDesc, MemoryLocation,
        PresentMode, RhiError, Surface, Swapchain, SwapchainDesc,
    };
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    let mut args = std::env::args().skip(1);
    let frames_per_run: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(300);
    let fills: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(48);
    let warmup = 30;

    let mut windows = WindowSystem::start(4096).unwrap();
    let window = windows
        .create_window(&WindowDesc {
            title: String::from("engine latency"),
            ..WindowDesc::default()
        })
        .unwrap();
    let instance = Instance::new(&InstanceDesc::default()).unwrap();
    let adapter = instance
        .select_adapter(&AdapterPreference::HighPerformance)
        .unwrap();
    println!("adapter: {}", adapter.summary());
    let device = Device::new(&instance, &adapter).unwrap();
    println!("latency extensions: {:?}", device.latency_extensions());
    let surface = Surface::from_win32(&instance, window.hinstance, window.hwnd).unwrap();
    let modes = Swapchain::supported_present_modes(&device, &surface).unwrap();
    let present_mode = [PresentMode::Immediate, PresentMode::Mailbox]
        .into_iter()
        .find(|m| modes.contains(m))
        .unwrap_or(PresentMode::Fifo);
    let mut swapchain = Swapchain::new(
        &device,
        &surface,
        &SwapchainDesc {
            width: 1280,
            height: 720,
            present_mode,
            srgb: true,
            low_latency: true,
        },
    )
    .unwrap();
    println!("swapchain: {swapchain:?}");
    let mut frames = Frames::new(&device, device.graphics.family, 2).unwrap();
    let mut allocator = Allocator::new(&device);
    const LOAD_BYTES: u64 = 32 << 20;
    let (load, load_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(LOAD_BYTES)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Device,
        )
        .unwrap();

    // Configurations: off, the vendor path (if any) on, the generic path on.
    let mut configs: Vec<(&str, Option<Backend>, LowLatency)> =
        vec![("off", Some(Backend::Generic), LowLatency::Off)];
    let vendor = Latency::new(&device, &swapchain, LatencySettings::default())
        .unwrap()
        .backend();
    match vendor {
        Backend::NvidiaReflex => {
            configs.push(("Reflex on", Some(Backend::NvidiaReflex), LowLatency::On));
            configs.push((
                "Reflex boost",
                Some(Backend::NvidiaReflex),
                LowLatency::Boost,
            ));
        }
        Backend::AmdAntiLag => {
            configs.push(("Anti-Lag 2 on", Some(Backend::AmdAntiLag), LowLatency::On))
        }
        Backend::Generic => {}
    }
    configs.push((
        "generic on (present_wait, 1 queued)",
        Some(Backend::Generic),
        LowLatency::On,
    ));

    let median = |v: &mut Vec<f64>| -> f64 {
        if v.is_empty() {
            return f64::NAN;
        }
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let mut rows = Vec::new();
    let mut reports: Vec<NvFrameReport> = Vec::new();
    let mut close = false;
    'configs: for (name, backend, mode) in configs {
        let settings = LatencySettings {
            mode,
            fps_limit: None,
            max_queued_presents: 1,
        };
        let mut latency = Latency::with_backend(&device, &swapchain, settings, backend).unwrap();
        // Reflex keeps reports of earlier configurations; only this run's ids count.
        let first_id = swapchain.last_present_id() + 1;
        let mut interval = Vec::new();
        let mut input_to_present_call = Vec::new();
        let mut input_to_display = Vec::new();
        let mut pending: VecDeque<(u64, Instant)> = VecDeque::new();
        let mut last_begin: Option<Instant> = None;
        for n in 0..frames_per_run + warmup {
            let id = latency.begin_frame(&swapchain).unwrap();
            let begin = Instant::now();
            windows.drain_events(|e| {
                if e.kind == WindowEventKind::CloseRequested {
                    close = true;
                }
            });
            if close {
                break 'configs;
            }
            latency.mark(&swapchain, Marker::InputSample);
            let input = Instant::now();
            latency.mark(&swapchain, Marker::SimulationStart);
            let sim_end = Instant::now() + Duration::from_micros(300);
            while Instant::now() < sim_end {
                core::hint::spin_loop();
            }
            latency.mark(&swapchain, Marker::SimulationEnd);

            let frame = frames.begin().unwrap();
            let acquired = match swapchain.acquire(frame.image_available, u64::MAX) {
                Ok(a) => a,
                Err(RhiError::Vulkan { result, .. }) => panic!("acquire: {result:?}"),
                Err(e) => panic!("{e}"),
            };
            latency.mark(&swapchain, Marker::RenderSubmitStart);
            let d = device.raw();
            let image = swapchain.images()[acquired.index as usize];
            let fill_barrier = vk::BufferMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .buffer(load)
                .size(vk::WHOLE_SIZE);
            let to_clear = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(image)
                .subresource_range(engine_rhi::color_range());
            let to_present = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .image(image)
                .subresource_range(engine_rhi::color_range());
            let shade = (n % 64) as f32 / 64.0;
            // SAFETY: the frame's command buffer is recording; the image was acquired;
            // the load buffer is ours; barriers order every write.
            unsafe {
                for i in 0..fills {
                    if i > 0 {
                        d.cmd_pipeline_barrier2(
                            frame.cmd,
                            &vk::DependencyInfo::default().buffer_memory_barriers(&[fill_barrier]),
                        );
                    }
                    d.cmd_fill_buffer(frame.cmd, load, 0, vk::WHOLE_SIZE, n as u32 ^ i);
                }
                d.cmd_pipeline_barrier2(
                    frame.cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&[to_clear]),
                );
                d.cmd_clear_color_image(
                    frame.cmd,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &vk::ClearColorValue {
                        float32: [shade, 0.2, 1.0 - shade, 1.0],
                    },
                    &[engine_rhi::color_range()],
                );
                d.cmd_pipeline_barrier2(
                    frame.cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&[to_present]),
                );
            }
            frames
                .submit_tagged(
                    device.graphics,
                    frame,
                    true,
                    Some(swapchain.render_finished(acquired.index)),
                    latency.submit_present_id(),
                )
                .unwrap();
            latency.mark(&swapchain, Marker::RenderSubmitEnd);
            latency.mark(&swapchain, Marker::PresentStart);
            latency.before_present();
            swapchain
                .present_with_id(device.graphics, acquired.index, latency.present_id())
                .unwrap();
            latency.mark(&swapchain, Marker::PresentEnd);
            let presented = Instant::now();

            if n >= warmup {
                if let Some(last) = last_begin {
                    interval.push((begin - last).as_secs_f64() * 1e3);
                }
                input_to_present_call.push((presented - input).as_secs_f64() * 1e3);
                pending.push_back((id, input));
            }
            last_begin = Some(begin);
            // Displayed presents, observed once per frame.
            while let Some(&(old, at)) = pending.front() {
                if swapchain.wait_for_present(old, 0).unwrap_or(false) {
                    input_to_display.push(at.elapsed().as_secs_f64() * 1e3);
                    pending.pop_front();
                } else {
                    break;
                }
            }
        }
        frames.wait_idle().unwrap();
        let mut gpu = Vec::new();
        if latency.backend() == Backend::NvidiaReflex && mode != LowLatency::Off {
            latency.nv_reports(&swapchain, &mut reports).unwrap();
            gpu.extend(
                reports
                    .iter()
                    .filter(|r| r.present_id >= first_id)
                    .filter_map(NvFrameReport::input_to_gpu_end_us)
                    .map(|us| us as f64 / 1e3),
            );
        }
        let ms = |v: f64| {
            if v.is_nan() {
                String::from("—")
            } else {
                format!("{v:.2}")
            }
        };
        let row = format!(
            "| {name} | {:?} | {} | {} | {} | {} |",
            latency.backend(),
            ms(median(&mut interval)),
            ms(median(&mut input_to_present_call)),
            ms(median(&mut input_to_display)),
            if gpu.is_empty() {
                String::from("—")
            } else {
                format!("{:.2}", median(&mut gpu))
            },
        );
        println!("{row}");
        rows.push(row);
    }

    println!(
        "\n### Latency (ref: {}, {:?}, {fills} × 32 MiB fills per frame)\n",
        adapter.name(),
        present_mode
    );
    println!(
        "| configuration | backend | frame ms | input → present call ms | input → displayed ms | input → GPU end ms (Reflex) |"
    );
    println!("|---|---|---:|---:|---:|---:|");
    for row in &rows {
        println!("{row}");
    }
    frames.wait_idle().unwrap();
    // SAFETY: the GPU is idle; the buffer is no longer used.
    unsafe { device.raw().destroy_buffer(load, None) };
    allocator.free(load_memory);
    drop(frames);
    drop(swapchain);
    drop(surface);
    windows.destroy_window(window.id).unwrap();
    let report = instance.validation_report();
    println!(
        "\nvalidation: enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    if report.errors > 0 {
        std::process::exit(1);
    }
}
