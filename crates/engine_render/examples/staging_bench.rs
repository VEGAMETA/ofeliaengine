//! Which memory streaming uploads should pass through (ADR 0057): upload memory
//! (write-combined host memory, what `MemoryLocation::Upload` picks) or host-cached memory
//! (`MemoryLocation::Readback`). Measures, for each:
//!
//! - unbuffered reads from a file straight into the mapped memory (`ReadQueue::push_into`);
//! - zstd decompression of 1 MiB chunks into it, on one thread and on the job pool;
//! - the GPU copying 128 MiB from it into device memory.
//!
//! `cargo run -p engine_render --release --example staging_bench -- <file of at least 128 MiB>`
//! (`ENGINE_ADAPTER=radeon` for the other GPU).

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used
)]

use engine_core::jobs::ThreadPool;
use engine_platform::io::{AlignedBuffer, READ_ALIGN, ReadFile, ReadQueue};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Device, Instance, InstanceDesc, MemoryLocation,
};
use std::sync::Arc;
use std::time::Instant;

const BYTES: usize = 128 << 20;
const CHUNK: usize = 1 << 20;

fn best(mut runs: impl FnMut() -> f64) -> f64 {
    (0..3).map(|_| runs()).fold(f64::MAX, f64::min)
}

/// Reads the first `BYTES` of `file` into `target`, 1 MiB per read, 32 in flight.
fn read_into(file: &Arc<ReadFile>, target: *mut u8) -> f64 {
    let mut queue = ReadQueue::new(32).unwrap();
    let start = Instant::now();
    let mut next = 0;
    while next < BYTES || queue.pending() > 0 {
        while next < BYTES && queue.has_room() {
            // SAFETY: `target` holds `BYTES` bytes, unused until the reads are popped below.
            unsafe {
                queue
                    .push_into(file, next as u64, target.add(next), CHUNK, 0)
                    .unwrap();
            }
            next += CHUNK;
        }
        queue.submit(true).unwrap();
        while let Some(done) = queue.pop() {
            assert_eq!(done.result.unwrap(), CHUNK);
        }
    }
    start.elapsed().as_secs_f64()
}

/// A mapped address that jobs may share (they write disjoint ranges).
#[derive(Clone, Copy)]
struct Target(*mut u8);
// SAFETY: jobs write disjoint chunks of memory that outlives them.
unsafe impl Send for Target {}
// SAFETY: as above.
unsafe impl Sync for Target {}

impl Target {
    /// The address `offset` bytes in (a method, so closures capture the whole `Target`).
    fn at(self, offset: usize) -> *mut u8 {
        // SAFETY: callers stay within the mapped `BYTES`.
        unsafe { self.0.add(offset) }
    }
}

/// Decompresses `chunks` (each 1 MiB raw) into `target`; on `jobs` or on this thread.
fn decompress_into(chunks: &[Vec<u8>], target: Target, jobs: Option<&ThreadPool>) -> f64 {
    let one = |k: usize| {
        // SAFETY: chunk `k` writes its own 1 MiB of the `BYTES` target.
        let out = unsafe { core::slice::from_raw_parts_mut(target.at(k * CHUNK), CHUNK) };
        let n = zstd::bulk::decompress_to_buffer(&chunks[k], out).unwrap();
        assert_eq!(n, CHUNK);
    };
    let start = Instant::now();
    match jobs {
        Some(jobs) => jobs.parallel_for(chunks.len(), 1, |_, range| range.for_each(one)),
        None => (0..chunks.len()).for_each(one),
    }
    start.elapsed().as_secs_f64()
}

fn main() {
    let path = std::env::args().nth(1).expect("a file of at least 128 MiB");
    let file = Arc::new(ReadFile::open(path.as_ref()).unwrap());
    assert!(
        file.len() >= BYTES as u64,
        "the file is smaller than 128 MiB"
    );
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("staging_bench"),
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
    let jobs = ThreadPool::builder().build().unwrap();

    // Compressible data: the file's first 128 MiB, compressed per 1 MiB chunk.
    let mut source = AlignedBuffer::new(BYTES);
    read_into(&file, source.as_mut_slice().as_mut_ptr());
    let chunks: Vec<Vec<u8>> = source
        .as_slice()
        .chunks(CHUNK)
        .map(|c| zstd::bulk::compress(c, 19).unwrap())
        .collect();
    let packed: usize = chunks.iter().map(Vec::len).sum();

    println!("### Streaming staging memory");
    println!();
    println!("Device: {}", device.adapter().summary());
    println!(
        "File: {path} (first 128 MiB; zstd 19 in 1 MiB chunks: {:.1}%), {} threads",
        100.0 * packed as f64 / BYTES as f64,
        jobs.num_workers() + 1
    );
    println!();
    println!(
        "| memory | read from the drive | zstd, 1 thread | zstd, job pool | GPU copy to device memory |"
    );
    println!("|---|---:|---:|---:|---:|");

    let (device_buffer, device_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(BYTES as u64)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Device,
        )
        .unwrap();
    // SAFETY: plain object creation.
    let (pool, cmd, queries) = unsafe {
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
        let queries = d
            .create_query_pool(
                &vk::QueryPoolCreateInfo::default()
                    .query_type(vk::QueryType::TIMESTAMP)
                    .query_count(2),
                None,
            )
            .unwrap();
        (pool, cmd, queries)
    };
    let period = f64::from(device.adapter().limits.timestamp_period);
    let row = |name: &str, allocator: &mut Allocator, location: MemoryLocation| {
        let (staging, mut memory): (vk::Buffer, Allocation) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(BYTES as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                location,
            )
            .unwrap();
        let mapped = memory.mapped_slice_mut().unwrap();
        assert!(mapped.len() >= BYTES && (mapped.as_ptr() as usize).is_multiple_of(READ_ALIGN));
        let base = mapped.as_mut_ptr();
        let read = best(|| read_into(&file, base));
        // SAFETY: `base` holds `BYTES` bytes; no read is in flight.
        let read_back = unsafe { core::slice::from_raw_parts(base, BYTES) };
        assert!(read_back == source.as_slice(), "the read data differs");
        let single = best(|| decompress_into(&chunks, Target(base), None));
        let parallel = best(|| decompress_into(&chunks, Target(base), Some(&jobs)));
        let copy = best(|| {
            // SAFETY: recording and submitting on an idle queue; both buffers are alive.
            unsafe {
                d.begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .unwrap();
                d.cmd_reset_query_pool(cmd, queries, 0, 2);
                d.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::NONE, queries, 0);
                d.cmd_copy_buffer(
                    cmd,
                    staging,
                    device_buffer,
                    &[vk::BufferCopy::default().size(BYTES as u64)],
                );
                d.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::ALL_COMMANDS, queries, 1);
                d.end_command_buffer(cmd).unwrap();
                let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
                d.queue_submit2(
                    device.graphics.raw,
                    &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                    vk::Fence::null(),
                )
                .unwrap();
                device.wait_idle().unwrap();
                let mut stamps = [0u64; 2];
                d.get_query_pool_results(
                    queries,
                    0,
                    &mut stamps,
                    vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT,
                )
                .unwrap();
                (stamps[1] - stamps[0]) as f64 * period * 1e-9
            }
        });
        let gbps = |s: f64| BYTES as f64 / s / 1e9;
        println!(
            "| {name} | {:.2} GB/s | {:.2} GB/s | {:.1} GB/s | {:.1} GB/s |",
            gbps(read),
            gbps(single),
            gbps(parallel),
            gbps(copy)
        );
        // SAFETY: the GPU is idle.
        unsafe { allocator.destroy_buffer(staging, memory) };
    };
    row(
        "upload (write-combined)",
        &mut allocator,
        MemoryLocation::Upload,
    );
    row("host-cached", &mut allocator, MemoryLocation::Readback);
    // Plain process memory, for reference (no GPU copy from it).
    let mut plain = AlignedBuffer::new(BYTES);
    let base = plain.as_mut_slice().as_mut_ptr();
    let read = best(|| read_into(&file, base));
    let single = best(|| decompress_into(&chunks, Target(base), None));
    let parallel = best(|| decompress_into(&chunks, Target(base), Some(&jobs)));
    let gbps = |s: f64| BYTES as f64 / s / 1e9;
    println!(
        "| process memory | {:.2} GB/s | {:.2} GB/s | {:.1} GB/s | — |",
        gbps(read),
        gbps(single),
        gbps(parallel)
    );
    // SAFETY: the GPU is idle.
    unsafe {
        d.destroy_query_pool(queries, None);
        d.destroy_command_pool(pool, None);
        allocator.destroy_buffer(device_buffer, device_memory);
    }
}
