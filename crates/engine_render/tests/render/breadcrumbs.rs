//! GPU breadcrumbs through the render graph (ADR 0061): the markers written around every
//! recorded pass tell how far the GPU got in each frame slot.
//!
//! `deliberate_gpu_fault_is_reported` makes the GPU fail on purpose and checks the
//! device-loss report: `ENGINE_TEST_GPU_FAULT=page` writes to an unmapped address (a page
//! fault: NVIDIA loses the device at once; AMD drops the writes and carries on),
//! `ENGINE_TEST_GPU_FAULT=hang` loops until Windows resets the GPU (2 s). Both reset the
//! GPU (the driver recovers in a second or two; other programs using it may lose their
//! devices too), so the test is skipped unless asked for:
//!
//! ```text
//! ENGINE_TEST_GPU_FAULT=hang cargo test -p engine_render --test render -- --nocapture breadcrumbs::deliberate
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use engine_render::compute::{ComputeShader, Dispatch};
use engine_render::graph::RenderGraph;
use engine_rhi::ash::{self as ash_device, vk};
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Frames, Instance, InstanceDesc, RhiError,
};

fn gpu(validation: bool) -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render breadcrumb tests"),
            validation,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: validation,
        })?;
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    let (instance, device) = match attempt() {
        Ok(pair) => pair,
        Err(e) if !required => {
            println!("skipping GPU test (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            return None;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    println!("adapter: {}", device.adapter().summary());
    if device.breadcrumbs().is_none() {
        // NVIDIA and AMD drivers have VK_AMD_buffer_marker; software renderers may not.
        let vendor = device.adapter().vendor_id;
        assert!(
            !(required && (vendor == 0x10DE || vendor == 0x1002)),
            "no breadcrumbs on an NVIDIA or AMD GPU (is ENGINE_GPU_BREADCRUMBS=0 set?)"
        );
        println!("skipping: no VK_AMD_buffer_marker");
        return None;
    }
    Some((instance, device))
}

/// Records three passes (a fourth is culled): markers only, no work.
fn record(graph: &mut RenderGraph, slot: usize, cmd: vk::CommandBuffer, allocator: &mut Allocator) {
    // SAFETY: the test waits for the slot's previous submission before recording again.
    let mut fg = unsafe { graph.begin_slot(slot, cmd) };
    fg.add_pass("first").side_effects();
    fg.add_pass("second").side_effects();
    fg.add_pass("culled");
    fg.add_pass("third").side_effects();
    fg.execute(allocator).unwrap();
}

fn report(device: &Device) -> String {
    let mut text = String::new();
    device
        .breadcrumbs()
        .unwrap()
        .write_report(&mut text)
        .unwrap();
    text
}

#[test]
fn breadcrumbs_follow_the_gpu_through_a_frame() {
    let Some((instance, device)) = gpu(true) else {
        return;
    };
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let mut graph = RenderGraph::new(&device, 2);
    // SAFETY: plain object creation on a live device.
    let (pool, cmds, gate) = unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
            .unwrap();
        let cmds = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(2),
            )
            .unwrap();
        let mut timeline = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let gate = d
            .create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut timeline),
                None,
            )
            .unwrap();
        (pool, cmds, gate)
    };
    // Records a frame into `slot` and submits it; with `wait`, the GPU starts it only
    // once the host sets the gate to that value.
    let mut submit = |graph: &mut RenderGraph, slot: usize, wait: Option<u64>| {
        let cmd = cmds[slot];
        // SAFETY: the command buffer's previous submission has completed (checked by
        // the test's waits); the gate is a live timeline semaphore.
        unsafe {
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
            record(graph, slot, cmd, &mut allocator);
            d.end_command_buffer(cmd).unwrap();
            let waits = [vk::SemaphoreSubmitInfo::default()
                .semaphore(gate)
                .value(wait.unwrap_or(0))
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
            let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            let info = vk::SubmitInfo2::default()
                .command_buffer_infos(&commands)
                .wait_semaphore_infos(if wait.is_some() { &waits } else { &[] });
            d.queue_submit2(device.graphics.raw, &[info], vk::Fence::null())
                .unwrap();
        }
    };

    // Frame 1, slot 0: runs to the end.
    submit(&mut graph, 0, None);
    device.wait_idle().unwrap();
    assert_eq!(graph.gpu_progress(0), Some((Some(2), Some(2))));
    let text = report(&device);
    println!("{text}");
    assert!(
        text.contains("3 passes: the GPU finished every pass"),
        "{text}"
    );

    // Opens the gate even if an assertion below fails, so the device can go idle.
    struct OpenOnDrop<'a>(&'a ash_device::Device, vk::Semaphore);
    impl Drop for OpenOnDrop<'_> {
        fn drop(&mut self) {
            // SAFETY: host signal of a live timeline semaphore; ignored if already higher.
            let _ = unsafe {
                self.0.signal_semaphore(
                    &vk::SemaphoreSignalInfo::default()
                        .semaphore(self.1)
                        .value(1),
                )
            };
        }
    }
    let open = OpenOnDrop(d, gate);

    // Frame 2, slot 1, and frame 3, slot 0 (its markers still hold frame 1's): both wait
    // for the gate, so the GPU has started neither.
    submit(&mut graph, 1, Some(1));
    submit(&mut graph, 0, Some(1));
    std::thread::park_timeout(std::time::Duration::from_millis(50));
    assert_eq!(graph.gpu_progress(1), Some((None, None)));
    assert_eq!(graph.gpu_progress(0), Some((None, None)));
    let text = report(&device);
    println!("{text}");
    assert_eq!(
        text.matches("the GPU had not started it").count(),
        2,
        "{text}"
    );

    drop(open);
    device.wait_idle().unwrap();
    assert_eq!(graph.gpu_progress(0), Some((Some(2), Some(2))));
    assert_eq!(graph.gpu_progress(1), Some((Some(2), Some(2))));

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, None);
        d.destroy_semaphore(gate, None);
        d.destroy_command_pool(pool, None);
    }
    let validation = instance.validation_report();
    assert_eq!(validation.errors, 0, "{:?}", validation.first_errors);
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FaultPush {
    target: u64,
    value: u32,
    stride: u32,
}

#[test]
fn deliberate_gpu_fault_is_reported() {
    let mode = std::env::var("ENGINE_TEST_GPU_FAULT").unwrap_or_default();
    let (spirv, entry): (&[u8], _) = match mode.as_str() {
        "page" => (
            include_bytes!("../shaders/fault.faultWrite.spv"),
            c"faultWrite",
        ),
        "hang" => (
            include_bytes!("../shaders/fault.faultHang.spv"),
            c"faultHang",
        ),
        _ => {
            println!("skipping: resets the GPU; set ENGINE_TEST_GPU_FAULT=page or hang to run it");
            return;
        }
    };
    let Some((_instance, device)) = gpu(false) else {
        return;
    };
    let reports = std::env::temp_dir().join(format!("engine_gpu_fault_{}", std::process::id()));
    engine_log::report::set_dir(&reports);
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let shader = ComputeShader::<FaultPush>::new(&device, &bindless, spirv, entry).unwrap();
    let mut frames = Frames::new(&device, device.graphics.family, 1).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let frame = frames.begin().unwrap();
    {
        let mut fg = graph.begin(&frame);
        fg.add_pass("before the fault").side_effects();
        fg.add_compute("fault", &bindless).side_effects().dispatch(
            &shader,
            Dispatch::Threads([1 << 16, 1, 1]),
            |_| {
                // Far above any allocation: every write is a page fault.
                Some(FaultPush {
                    target: 0x7F00_DEAD_0000,
                    value: 0xBAD,
                    stride: 1024,
                })
            },
        );
        fg.add_pass("after the fault").side_effects();
        fg.execute_with_bindless(&mut allocator, Some(&bindless))
            .unwrap();
    }
    let submitted = frames.submit(device.graphics, frame, false, None);
    let waited = submitted.and_then(|()| frames.wait_idle());
    println!("result: {waited:?}");
    assert!(waited.is_err(), "the GPU survived the {mode} fault");
    let report = engine_rhi::last_gpu_fault().expect("no GPU fault report");
    println!("{report}");
    let device_report = &report.devices[0];
    assert!(
        device_report.breadcrumbs.contains("'fault'"),
        "the breadcrumbs name the faulting pass: {}",
        device_report.breadcrumbs
    );
    if !report.hang && device.device_fault_enabled() {
        assert!(device_report.description.is_some());
    }
    let file = report.report_file.as_ref().expect("no report file");
    assert!(std::fs::read_to_string(file).unwrap().contains("'fault'"));
    let _ = std::fs::remove_dir_all(&reports);
    // The device is lost: skip orderly destruction of what used it, except the device
    // itself (destroying objects of a lost device is allowed; waiting is not needed).
    core::mem::forget(graph);
    core::mem::forget(frames);
    core::mem::forget(shader);
    core::mem::forget(bindless);
    core::mem::forget(allocator);
}
