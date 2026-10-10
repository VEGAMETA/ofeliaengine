//! Sorting on the GPU (ADR 0080): 32-bit keys, and a 32-bit payload with them, by an LSD
//! radix sort of 4-bit digits, each pass stable, in one compute pass of the frame graph.
//! The number of keys can come from the GPU (an earlier pass's count): particles sorted
//! by depth, for example, without the CPU waiting for anything.
//!
//! ```text
//! let sort = GpuSort::new(&device, &bindless)?;
//! // Each frame: the first `count` keys of `keys` (a GPU buffer of `capacity` u32), with
//! // their payload, ascending; only the low 16 bits of the keys matter here.
//! sort.add_pass(fg, &bindless, SortJob {
//!     keys,
//!     values: Some(indices),
//!     capacity,
//!     count: SortCount::Gpu { buffer: counters, offset: 4 },
//!     key_bits: 16,
//! });
//! ```
//!
//! Per 4-bit digit five dispatches (count, reduce, scan, scan add, scatter: see
//! `sort.slang`); 32-bit keys take eight digits, 16-bit keys four. Keys are
//! sorted as unsigned integers: map signed or floating-point values to them first (a
//! float's bits with the sign bit flipped when it is positive, every bit flipped when it
//! is negative).

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::graph::{Access, Buffer, BufferDesc, FrameGraph, ShaderStages};
use engine_rhi::{Bindless, Device, RhiError};

const SETUP: &[u8] = include_bytes!("../shaders/sort.sortSetup.spv");
const COUNT: &[u8] = include_bytes!("../shaders/sort.sortCount.spv");
const REDUCE: &[u8] = include_bytes!("../shaders/sort.sortReduce.spv");
const SCAN: &[u8] = include_bytes!("../shaders/sort.sortScan.spv");
const SCAN_ADD: &[u8] = include_bytes!("../shaders/sort.sortScanAdd.spv");
const SCATTER: &[u8] = include_bytes!("../shaders/sort.sortScatter.spv");
const SCATTER_PAYLOAD: &[u8] = include_bytes!("../shaders/sort.sortScatterPayload.spv");

/// Bins of a digit.
const BINS: u32 = 16;
/// Keys a workgroup takes at a time (`BLOCK` in `sort.slang`).
const BLOCK: u32 = 512;
/// The most count and scatter workgroups: beyond, each takes more blocks.
const MAX_GROUPS: u32 = 800;
/// Bytes of `SortConstants`; its dispatch arguments start at these offsets.
const CONSTANTS_BYTES: u64 = 64;
const COUNT_ARGS: u64 = 32;
const REDUCE_ARGS: u64 = 48;

/// `SortPush` in `sort.slang` (80 bytes).
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct SortPush {
    keys_in: u64,
    keys_out: u64,
    values_in: u64,
    values_out: u64,
    sums: u64,
    reduced: u64,
    constants: u64,
    count: u64,
    shift: u32,
    capacity: u32,
    count_from_gpu: u32,
    max_groups: u32,
}

/// How many keys a sort sorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortCount {
    /// This many (at most [`SortJob::capacity`]).
    Keys(u32),
    /// The `u32` at `offset` in `buffer`, written by an earlier pass of the frame (at most
    /// [`SortJob::capacity`]; more are not sorted).
    Gpu {
        /// The buffer holding the count.
        buffer: Buffer,
        /// Its byte offset there (a multiple of 4).
        offset: u64,
    },
}

/// One sort (see [`GpuSort::add_pass`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortJob {
    /// The keys (`u32`), sorted in place, ascending.
    pub keys: Buffer,
    /// A `u32` per key, moved with it (indices into what the keys stand for).
    pub values: Option<Buffer>,
    /// Keys `keys` (and `values`) hold.
    pub capacity: u32,
    /// How many of them to sort, from the first.
    pub count: SortCount,
    /// The low bits that differ between keys (the others are taken as equal): 32, or
    /// fewer for fewer passes (16-bit keys take half the time). Rounded up to a multiple
    /// of 8.
    pub key_bits: u32,
}

/// The GPU radix sort's pipelines.
pub struct GpuSort {
    setup: ComputeShader<SortPush>,
    count: ComputeShader<SortPush>,
    reduce: ComputeShader<SortPush>,
    scan: ComputeShader<SortPush>,
    scan_add: ComputeShader<SortPush>,
    scatter: ComputeShader<SortPush>,
    scatter_payload: ComputeShader<SortPush>,
}

impl core::fmt::Debug for GpuSort {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpuSort").finish_non_exhaustive()
    }
}

/// 4-bit passes for keys of `bits` bits: an even number, so the keys end where they began.
fn passes(bits: u32) -> u32 {
    bits.clamp(1, 32).div_ceil(8) * 2
}

impl GpuSort {
    /// Creates the pipelines (in parallel where they compile).
    pub fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        let source = |spirv, entry| ComputeSource::<SortPush>::new(spirv, entry, &[]);
        let [
            setup,
            count,
            reduce,
            scan,
            scan_add,
            scatter,
            scatter_payload,
        ] = ComputeSource::create_all(
            device,
            bindless,
            [
                source(SETUP, c"sortSetup")?,
                source(COUNT, c"sortCount")?,
                source(REDUCE, c"sortReduce")?,
                source(SCAN, c"sortScan")?,
                source(SCAN_ADD, c"sortScanAdd")?,
                source(SCATTER, c"sortScatter")?,
                source(SCATTER_PAYLOAD, c"sortScatterPayload")?,
            ],
        )?;
        Ok(Self {
            setup,
            count,
            reduce,
            scan,
            scan_add,
            scatter,
            scatter_payload,
        })
    }

    /// Declares a compute pass `name` that sorts `job`'s keys (and payload). The pass
    /// reads what earlier passes wrote into them, and later ones read them sorted. Its
    /// scratch memory is the frame's (the graph's transient buffers): `capacity` × 4 bytes
    /// for keys, as much again for a payload, and about 52 KB.
    pub fn add_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        name: &'static str,
        job: SortJob,
    ) {
        let capacity = job.capacity.max(1);
        let bytes = u64::from(capacity) * 4;
        let keys_tmp = fg.create_buffer("sort keys", BufferDesc { size: bytes });
        let values_tmp = job
            .values
            .map(|_| fg.create_buffer("sort payload", BufferDesc { size: bytes }));
        let sums = fg.create_buffer(
            "sort sums",
            BufferDesc {
                size: u64::from(BINS * MAX_GROUPS) * 4,
            },
        );
        let reduced = fg.create_buffer(
            "sort reduced",
            BufferDesc {
                size: u64::from(BINS * MAX_GROUPS.div_ceil(BLOCK)) * 4,
            },
        );
        let constants = fg.create_buffer(
            "sort constants",
            BufferDesc {
                size: CONSTANTS_BYTES,
            },
        );
        let (count_buffer, count_offset, sorted) = match job.count {
            SortCount::Keys(n) => (None, 0, n.min(job.capacity)),
            SortCount::Gpu { buffer, offset } => (Some(buffer), offset, job.capacity),
        };
        let rw = Access::StorageReadWrite(ShaderStages::COMPUTE);
        let passes = passes(job.key_bits);
        let scatter = if job.values.is_some() {
            &self.scatter_payload
        } else {
            &self.scatter
        };
        fg.add_compute(name, bindless)
            .write(job.keys, rw)
            .maybe_read(job.values, rw)
            .write(keys_tmp, rw)
            .maybe_read(values_tmp, rw)
            .write(sums, rw)
            .write(reduced, rw)
            .write(constants, rw)
            .read(constants, Access::Indirect)
            .maybe_read(count_buffer, Access::StorageRead(ShaderStages::COMPUTE))
            .record(move |rec| {
                let ctx = rec.context();
                let keys = [ctx.address(job.keys), ctx.address(keys_tmp)];
                let values = [
                    job.values.map_or(0, |v| ctx.address(v)),
                    values_tmp.map_or(0, |v| ctx.address(v)),
                ];
                let args = ctx.buffer(constants);
                let mut push = SortPush {
                    keys_in: keys[0],
                    keys_out: keys[1],
                    values_in: values[0],
                    values_out: values[1],
                    sums: ctx.address(sums),
                    reduced: ctx.address(reduced),
                    constants: ctx.address(constants),
                    count: count_buffer.map_or(0, |b| ctx.address(b) + count_offset),
                    shift: 0,
                    capacity: sorted,
                    count_from_gpu: u32::from(count_buffer.is_some()),
                    max_groups: MAX_GROUPS,
                };
                rec.dispatch(&self.setup, Dispatch::Groups([1, 1, 1]), &push);
                rec.barrier_for_indirect();
                for pass in 0..passes {
                    let from = (pass % 2) as usize;
                    push.shift = pass * 4;
                    push.keys_in = keys[from];
                    push.keys_out = keys[1 - from];
                    push.values_in = values[from];
                    push.values_out = values[1 - from];
                    rec.dispatch_indirect(&self.count, args, COUNT_ARGS, &push);
                    rec.barrier();
                    rec.dispatch_indirect(&self.reduce, args, REDUCE_ARGS, &push);
                    rec.barrier();
                    rec.dispatch(&self.scan, Dispatch::Groups([1, 1, 1]), &push);
                    rec.barrier();
                    rec.dispatch_indirect(&self.scan_add, args, REDUCE_ARGS, &push);
                    rec.barrier();
                    rec.dispatch_indirect(scatter, args, COUNT_ARGS, &push);
                    rec.barrier();
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_are_even_so_keys_end_in_place() {
        assert_eq!(passes(32), 8);
        assert_eq!(passes(24), 6);
        assert_eq!(passes(17), 6);
        assert_eq!(passes(16), 4);
        assert_eq!(passes(1), 2);
        assert_eq!(passes(0), 2);
        assert_eq!(core::mem::size_of::<SortPush>(), 80);
    }
}
