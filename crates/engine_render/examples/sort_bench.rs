//! The GPU radix sort's time (ADR 0080): random 32-bit keys, with and without a payload,
//! and 16-bit keys, from 16 K to 4 M keys. Each frame copies fresh random keys in (not
//! timed) and sorts them; the GPU time of the sort pass, median of the timed frames.
//!
//! `cargo run -p engine_render --release --example sort_bench` (`ENGINE_ADAPTER=radeon`
//! for the integrated GPU). Prints a Markdown table.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_render::graph::{Access, BufferDesc, BufferState, ImportedBuffer, RenderGraph};
use engine_render::sort::{GpuSort, SortCount, SortJob};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const WARMUP: usize = 5;
const FRAMES: usize = 25;

fn hash(i: u32) -> u32 {
    let mut h = i.wrapping_mul(0x9E37_79B9);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^ (h >> 16)
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

#[allow(clippy::too_many_lines)]
fn main() {
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("sort_bench"),
        validation: false,
        debug_utils: false,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .unwrap();
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    let device = Device::new(&instance, &adapter).unwrap();
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let sort = GpuSort::new(&device, &bindless).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
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

    const MAX: u32 = 4 << 20;
    // Random keys, then the indices 0.. (the payload).
    let data: Vec<u32> = (0..MAX).map(hash).chain(0..MAX).collect();
    let data_bytes = (data.len() * 4) as u64;
    let (source, source_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(data_bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Upload,
        )
        .unwrap();
    // SAFETY: the mapping covers the buffer; nothing else uses it yet.
    unsafe {
        core::ptr::copy_nonoverlapping(
            data.as_ptr().cast::<u8>(),
            source_mem.mapped_ptr().unwrap().as_ptr(),
            data_bytes as usize,
        );
    }

    println!("### GPU radix sort");
    println!();
    println!("Device: {}", device.adapter().summary());
    println!();
    println!(
        "| keys | 32-bit keys, µs | with payload, µs | 16-bit keys + payload, µs | Mkeys/s (32-bit + payload) |"
    );
    println!("|---:|---:|---:|---:|---:|");
    for n in [16u32 << 10, 64 << 10, 256 << 10, 1 << 20, 4 << 20] {
        let mut columns = Vec::new();
        for (payload, key_bits) in [(false, 32), (true, 32), (true, 16)] {
            let mut times = Vec::new();
            for frame in 0..=WARMUP + FRAMES {
                // SAFETY: one frame at a time: recorded, submitted, waited for.
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
                    // SAFETY: the previous frame finished.
                    let mut fg = unsafe { graph.begin_slot(0, cmd) };
                    let input = fg.import_buffer(
                        "keys source",
                        ImportedBuffer {
                            buffer: source,
                            size: data_bytes,
                            address: 0,
                            initial: BufferState::idle(),
                            final_state: None,
                        },
                    );
                    let bytes = u64::from(n) * 4;
                    let keys = fg.create_buffer("keys", BufferDesc { size: bytes });
                    let values =
                        payload.then(|| fg.create_buffer("values", BufferDesc { size: bytes }));
                    fg.add_pass("fresh keys")
                        .read(input, Access::TransferRead)
                        .write(keys, Access::TransferWrite)
                        .maybe_read(values, Access::TransferWrite)
                        .execute(move |ctx| {
                            // SAFETY: recording; the graph orders the copies.
                            unsafe {
                                ctx.device.cmd_copy_buffer(
                                    ctx.cmd,
                                    ctx.buffer(input),
                                    ctx.buffer(keys),
                                    &[vk::BufferCopy {
                                        src_offset: 0,
                                        dst_offset: 0,
                                        size: bytes,
                                    }],
                                );
                                if let Some(values) = values {
                                    ctx.device.cmd_copy_buffer(
                                        ctx.cmd,
                                        ctx.buffer(input),
                                        ctx.buffer(values),
                                        &[vk::BufferCopy {
                                            src_offset: u64::from(MAX) * 4,
                                            dst_offset: 0,
                                            size: bytes,
                                        }],
                                    );
                                }
                            }
                        });
                    sort.add_pass(
                        &mut fg,
                        &bindless,
                        "sort",
                        SortJob {
                            keys,
                            values,
                            capacity: n,
                            count: SortCount::Keys(n),
                            key_bits,
                        },
                    );
                    // Something reads the sorted keys: the passes are kept.
                    let (sink, sink_mem) = allocator
                        .create_buffer(
                            &vk::BufferCreateInfo::default()
                                .size(4)
                                .usage(vk::BufferUsageFlags::TRANSFER_DST),
                            MemoryLocation::Readback,
                        )
                        .unwrap();
                    let out = fg.import_buffer(
                        "sink",
                        ImportedBuffer {
                            buffer: sink,
                            size: 4,
                            address: 0,
                            initial: BufferState::idle(),
                            final_state: Some(BufferState::host_read()),
                        },
                    );
                    fg.add_pass("read one")
                        .read(keys, Access::TransferRead)
                        .write(out, Access::TransferWrite)
                        .execute(move |ctx| {
                            // SAFETY: recording; after the sort.
                            unsafe {
                                ctx.device.cmd_copy_buffer(
                                    ctx.cmd,
                                    ctx.buffer(keys),
                                    ctx.buffer(out),
                                    &[vk::BufferCopy {
                                        src_offset: 0,
                                        dst_offset: 0,
                                        size: 4,
                                    }],
                                );
                            }
                        });
                    fg.execute_with_bindless(&mut allocator, Some(&bindless))
                        .unwrap();
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
                        allocator.destroy_buffer(sink, sink_mem);
                    }
                }
                // The frame before this one's times, read when this one began.
                if frame > WARMUP {
                    times.push(sort_time(&graph));
                }
            }
            columns.push(median(times));
        }
        println!(
            "| {} | {:.1} | {:.1} | {:.1} | {:.0} |",
            n,
            columns[0],
            columns[1],
            columns[2],
            f64::from(n) / columns[1]
        );
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        allocator.destroy_buffer(source, source_mem);
        d.destroy_command_pool(pool, None);
    }
}

/// The sort pass's GPU time in the frame the graph last read times of.
fn sort_time(graph: &RenderGraph) -> f64 {
    graph
        .gpu_pass_times()
        .iter()
        .filter(|t| t.name == "sort")
        .map(|t| f64::from(t.gpu_us))
        .sum()
}
