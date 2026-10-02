//! GPU hardware self-check (docs/research/radeon-igpu-lds-faults.md): a compute shader
//! exchanges values between the threads of a workgroup through groupshared memory (LDS),
//! with barriers, and the result is compared with the same computation on the CPU. The
//! shader is trivially correct, so any difference is a fault of the GPU or the platform,
//! not of the engine.
//!
//! On ref-1 the Radeon iGPU (Raphael, RDNA2) flips bits of LDS reads while the CPU is
//! busy: lane 24 of a wave, mostly bit 9. Every flaky image comparison seen on that GPU
//! (missing objects, cracks, spikes, wrong colors) went through LDS: the driver's NGG
//! primitive culling and vertex compaction, and attribute interpolation.
//!
//! The check is opt-in (it fails on a faulty machine whatever the engine does):
//!
//! ```text
//! ENGINE_ADAPTER=radeon ENGINE_REQUIRE_GPU=1 cargo test -p engine_render --test gpu_selfcheck -- --ignored --nocapture
//! ```
//!
//! It keeps every CPU thread busy while it runs, as a build next to the tests does
//! (`ENGINE_SELFCHECK_CPU_LOAD=0` turns that off), and runs `ENGINE_SELFCHECK_DISPATCHES`
//! dispatches (default 500, about 6 s on the iGPU).

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Device, Instance as VkInstance, InstanceDesc, MemoryLocation,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const THREADS: u32 = 1 << 18;
const GROUP: u32 = 256;
/// Exchange rounds: one keeps a wrong read in the thread that made it (its lane shows).
const ROUNDS: u32 = 1;

fn gpu() -> Option<(VkInstance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(VkInstance, Device), engine_rhi::RhiError> {
        let instance = VkInstance::new(&InstanceDesc {
            app_name: String::from("engine_render gpu self-check"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: false,
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

/// The shader's result for every thread, computed on the CPU.
fn reference() -> Vec<u32> {
    let mut out = vec![0u32; THREADS as usize];
    for (g, group) in out.chunks_exact_mut(GROUP as usize).enumerate() {
        let base = g as u32 * GROUP;
        let mut x: Vec<u32> = (0..GROUP)
            .map(|l| (base + l).wrapping_mul(2_654_435_761).wrapping_add(1))
            .collect();
        let mut y = vec![0u32; GROUP as usize];
        for i in 0..ROUNDS {
            for (l, y) in y.iter_mut().enumerate() {
                *y = x[(l as u32 + 1 + i * 37) as usize & (GROUP as usize - 1)];
            }
            for (x, &y) in x.iter_mut().zip(&y) {
                *x ^= y.wrapping_add(*x << 6).wrapping_add(*x >> 2);
            }
        }
        group.copy_from_slice(&x);
    }
    out
}

/// Busy threads on every logical CPU until dropped.
struct CpuLoad {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl CpuLoad {
    fn new(count: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..count)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut x = 1u64;
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..10_000 {
                            x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005) + 1);
                        }
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for CpuLoad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[test]
#[ignore = "hardware check, fails on a faulty GPU whatever the engine does; run with --ignored"]
fn lds_exchanges_are_exact() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let dispatches: usize = std::env::var("ENGINE_SELFCHECK_DISPATCHES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let load_threads = std::env::var("ENGINE_SELFCHECK_CPU_LOAD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, usize::from));
    let expected = reference();
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bytes = u64::from(THREADS) * 4;
    let (out, out_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default().size(bytes).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                    | vk::BufferUsageFlags::TRANSFER_SRC
                    | vk::BufferUsageFlags::TRANSFER_DST,
            ),
            MemoryLocation::Device,
        )
        .unwrap();
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
    let spirv = include_bytes!("shaders/selfcheck.ldsExchange.spv");
    let words: Vec<u32> = spirv
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    // SAFETY: plain object creation; the buffer has device address usage.
    let (address, module, layout, pipeline, pool, cmd) = unsafe {
        let address =
            d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(out));
        let module = d
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
            .unwrap();
        let ranges = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::COMPUTE,
            offset: 0,
            size: 16,
        }];
        let layout = d
            .create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&ranges),
                None,
            )
            .unwrap();
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(c"ldsExchange");
        let pipeline = d
            .create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(stage)
                    .layout(layout)],
                None,
            )
            .unwrap()[0];
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
        (address, module, layout, pipeline, pool, cmd)
    };
    let mut push = [0u8; 16];
    push[..8].copy_from_slice(&address.to_le_bytes());
    push[8..12].copy_from_slice(&ROUNDS.to_le_bytes());
    let barrier = |src: vk::PipelineStageFlags2,
                   src_access: vk::AccessFlags2,
                   dst: vk::PipelineStageFlags2,
                   dst_access: vk::AccessFlags2| {
        [vk::MemoryBarrier2::default()
            .src_stage_mask(src)
            .src_access_mask(src_access)
            .dst_stage_mask(dst)
            .dst_access_mask(dst_access)]
    };
    let load = (load_threads > 0).then(|| CpuLoad::new(load_threads));
    let mut wrong_dispatches = 0;
    let mut lanes = [0u32; 32];
    for k in 0..dispatches {
        // SAFETY: one-shot recording and submission, then wait; the barriers order the
        // clear, the dispatch, the copy and the host read.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
            d.cmd_fill_buffer(cmd, out, 0, vk::WHOLE_SIZE, 0);
            let b = barrier(
                vk::PipelineStageFlags2::ALL_TRANSFER,
                vk::AccessFlags2::TRANSFER_WRITE,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_STORAGE_WRITE,
            );
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&b));
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
            d.cmd_push_constants(cmd, layout, vk::ShaderStageFlags::COMPUTE, 0, &push);
            d.cmd_dispatch(cmd, THREADS / GROUP, 1, 1);
            let b = barrier(
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_STORAGE_WRITE,
                vk::PipelineStageFlags2::ALL_TRANSFER,
                vk::AccessFlags2::TRANSFER_READ,
            );
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&b));
            d.cmd_copy_buffer(
                cmd,
                out,
                readback,
                &[vk::BufferCopy {
                    src_offset: 0,
                    dst_offset: 0,
                    size: bytes,
                }],
            );
            let b = barrier(
                vk::PipelineStageFlags2::ALL_TRANSFER,
                vk::AccessFlags2::TRANSFER_WRITE,
                vk::PipelineStageFlags2::HOST,
                vk::AccessFlags2::HOST_READ,
            );
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&b));
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
        let ptr = readback_memory.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let got =
            unsafe { core::slice::from_raw_parts(ptr.as_ptr().cast::<u32>(), THREADS as usize) };
        let mut first = None;
        let mut count = 0;
        for (i, (g, e)) in got.iter().zip(&expected).enumerate() {
            if g != e {
                count += 1;
                lanes[i % 32] += 1;
                first.get_or_insert(i);
            }
        }
        if let Some(i) = first {
            wrong_dispatches += 1;
            if wrong_dispatches <= 5 {
                println!(
                    "dispatch {k}: {count} threads wrong, first {i} (wave lane {}): {:08x} instead of {:08x}",
                    i % 32,
                    got[i],
                    expected[i]
                );
            }
        }
    }
    drop(load);
    println!(
        "{wrong_dispatches} of {dispatches} dispatches wrong, {} wrong threads in all \
         ({load_threads} busy CPU threads); wrong threads per wave32 lane: {lanes:?}",
        lanes.iter().map(|&n| u64::from(n)).sum::<u64>()
    );
    // SAFETY: the device is idle.
    unsafe {
        d.destroy_pipeline(pipeline, None);
        d.destroy_pipeline_layout(layout, None);
        d.destroy_shader_module(module, None);
        d.destroy_command_pool(pool, None);
        allocator.destroy_buffer(out, out_memory);
        allocator.destroy_buffer(readback, readback_memory);
    }
    drop(allocator);
    drop(device);
    let report = instance.validation_report();
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
    assert_eq!(
        wrong_dispatches, 0,
        "the GPU returned wrong groupshared-memory reads: a hardware or platform fault, \
         not an engine bug (docs/research/radeon-igpu-lds-faults.md)"
    );
}
