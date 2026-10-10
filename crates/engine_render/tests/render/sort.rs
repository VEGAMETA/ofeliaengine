//! The GPU radix sort (`engine_render::sort`, ADR 0080) against the standard library's
//! stable sort: keys of every size that ends in a partial block, many equal keys (the
//! payload keeps their order), 16-bit keys, and a number of keys the GPU wrote (the keys
//! after it untouched). Synchronization validation watches the pass's own barriers.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_render::graph::{Access, BufferDesc, BufferState, ImportedBuffer, RenderGraph};
use engine_render::sort::{GpuSort, SortCount, SortJob};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render sort test"),
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

/// A pseudo-random 32-bit value from `i` and `seed`.
fn hash(i: u32, seed: u32) -> u32 {
    let mut h = i.wrapping_mul(0x9E37_79B9) ^ seed.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^ (h >> 16)
}

struct Case {
    name: &'static str,
    keys: Vec<u32>,
    /// Keys sorted: all of them, or the GPU's count.
    count: u32,
    from_gpu: bool,
    key_bits: u32,
}

/// The keys and payload after sorting `case` on the GPU.
#[allow(clippy::too_many_lines)]
fn sort_on_gpu(
    device: &Device,
    allocator: &mut Allocator,
    bindless: &Bindless,
    sort: &GpuSort,
    graph: &mut RenderGraph,
    cmd: vk::CommandBuffer,
    case: &Case,
) -> (Vec<u32>, Vec<u32>) {
    let d = device.raw();
    let capacity = case.keys.len() as u32;
    let bytes = u64::from(capacity.max(1)) * 4;
    // Keys, then payload (their indices), then the count.
    let mut data: Vec<u32> = case.keys.clone();
    data.resize(capacity.max(1) as usize, 0);
    data.extend(0..capacity.max(1));
    data.push(case.count);
    let data_bytes = (data.len() * 4) as u64;
    let (upload, upload_mem) = allocator
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
            upload_mem.mapped_ptr().unwrap().as_ptr(),
            data_bytes as usize,
        );
    }
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(2 * bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();

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
    {
        // SAFETY: nothing is in flight.
        let mut fg = unsafe { graph.begin_slot(0, cmd) };
        let source = fg.import_buffer(
            "sort input",
            ImportedBuffer {
                buffer: upload,
                size: data_bytes,
                address: 0,
                initial: BufferState::idle(),
                final_state: None,
            },
        );
        let keys = fg.create_buffer("keys", BufferDesc { size: bytes });
        let values = fg.create_buffer("values", BufferDesc { size: bytes });
        let count = fg.create_buffer("count", BufferDesc { size: 16 });
        fg.add_pass("upload")
            .read(source, Access::TransferRead)
            .write(keys, Access::TransferWrite)
            .write(values, Access::TransferWrite)
            .write(count, Access::TransferWrite)
            .execute(move |ctx| {
                let copy = |dst, src_offset, size| {
                    // SAFETY: recording; the graph orders the copies.
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(source),
                            ctx.buffer(dst),
                            &[vk::BufferCopy {
                                src_offset,
                                dst_offset: 0,
                                size,
                            }],
                        );
                    }
                };
                copy(keys, 0, bytes);
                copy(values, bytes, bytes);
                copy(count, 2 * bytes, 4);
            });
        sort.add_pass(
            &mut fg,
            bindless,
            "sort",
            SortJob {
                keys,
                values: Some(values),
                capacity,
                count: if case.from_gpu {
                    SortCount::Gpu {
                        buffer: count,
                        offset: 0,
                    }
                } else {
                    SortCount::Keys(case.count)
                },
                key_bits: case.key_bits,
            },
        );
        let out = fg.import_buffer(
            "readback",
            ImportedBuffer {
                buffer: readback,
                size: 2 * bytes,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        fg.add_pass("read back")
            .read(keys, Access::TransferRead)
            .read(values, Access::TransferRead)
            .write(out, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording; the graph orders the copies after the sort.
                unsafe {
                    for (from, offset) in [(keys, 0), (values, bytes)] {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            ctx.buffer(from),
                            ctx.buffer(out),
                            &[vk::BufferCopy {
                                src_offset: 0,
                                dst_offset: offset,
                                size: bytes,
                            }],
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
    // SAFETY: the GPU is idle; the mapping covers the buffer.
    let out: &[u32] = unsafe {
        core::slice::from_raw_parts(
            readback_mem.mapped_ptr().unwrap().as_ptr().cast::<u32>(),
            2 * capacity.max(1) as usize,
        )
    };
    let n = capacity as usize;
    let gpu_keys = out[..n].to_vec();
    let gpu_values = out[capacity.max(1) as usize..][..n].to_vec();
    // SAFETY: the GPU is idle; the graph only borrowed them for this frame.
    unsafe {
        allocator.destroy_buffer(upload, upload_mem);
        allocator.destroy_buffer(readback, readback_mem);
    }
    (gpu_keys, gpu_values)
}

#[test]
fn the_gpu_sort_matches_a_stable_sort() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
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

    let random = |n: u32, seed: u32| (0..n).map(|i| hash(i, seed)).collect::<Vec<u32>>();
    let mut cases = Vec::new();
    // Sizes around a block (512 keys) and a workgroup's share, and one beyond 800
    // workgroups of one block (several blocks each).
    for (k, n) in [1u32, 7, 511, 512, 513, 4097, 100_003, 1 << 20]
        .into_iter()
        .enumerate()
    {
        cases.push(Case {
            name: "random",
            keys: random(n, k as u32),
            count: n,
            from_gpu: false,
            key_bits: 32,
        });
    }
    cases.push(Case {
        name: "no keys",
        keys: random(64, 99),
        count: 0,
        from_gpu: false,
        key_bits: 32,
    });
    // Many equal keys: the payload shows whether their order was kept.
    cases.push(Case {
        name: "equal keys",
        keys: random(70_001, 7).into_iter().map(|k| k % 37).collect(),
        count: 70_001,
        from_gpu: false,
        key_bits: 32,
    });
    // 16-bit keys in half the passes.
    cases.push(Case {
        name: "16-bit",
        keys: random(50_000, 8).into_iter().map(|k| k & 0xFFFF).collect(),
        count: 50_000,
        from_gpu: false,
        key_bits: 16,
    });
    // A count the GPU wrote, smaller than the buffer: the keys after it stay.
    cases.push(Case {
        name: "count from the GPU",
        keys: random(30_000, 9),
        count: 21_789,
        from_gpu: true,
        key_bits: 32,
    });
    cases.push(Case {
        name: "count from the GPU, above the capacity",
        keys: random(3_000, 10),
        count: 5_000,
        from_gpu: true,
        key_bits: 32,
    });

    for case in &cases {
        let (keys, values) = sort_on_gpu(
            &device,
            &mut allocator,
            &bindless,
            &sort,
            &mut graph,
            cmd,
            case,
        );
        let sorted = (case.count as usize).min(case.keys.len());
        let mut expected: Vec<(u32, u32)> = case.keys[..sorted]
            .iter()
            .enumerate()
            .map(|(i, &k)| (k, i as u32))
            .collect();
        // Stable: equal keys keep their order (their indices increase).
        expected.sort_by_key(|&(k, _)| k);
        let mismatch = (0..sorted).find(|&i| (keys[i], values[i]) != expected[i]);
        assert!(
            mismatch.is_none(),
            "{} ({} keys, {sorted} sorted): at {:?} the GPU has {:?}, expected {:?}",
            case.name,
            case.keys.len(),
            mismatch,
            mismatch.map(|i| (keys[i], values[i])),
            mismatch.map(|i| expected[i]),
        );
        // The rest untouched.
        for i in sorted..case.keys.len() {
            assert_eq!(
                (keys[i], values[i]),
                (case.keys[i], i as u32),
                "{}: key {i} after the sorted ones changed",
                case.name
            );
        }
        println!("{}: {} keys, {sorted} sorted", case.name, case.keys.len());
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        d.destroy_command_pool(pool, None);
    }
    drop(sort);
    drop(bindless);
    drop(allocator);
    let report = instance.validation_report();
    assert_eq!(
        report.errors, 0,
        "validation errors: {:#?}",
        report.first_errors
    );
}
