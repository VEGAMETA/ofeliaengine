// GPU radix sort (ADR 0080): 32-bit keys, and a 32-bit payload with them, sorted 4 bits
// at a time from the least significant, each pass stable, by reduce-then-scan:
//
// 1. `sortSetup` (one thread): from the number of keys, how the keys are cut into
//    workgroups, and the indirect dispatch sizes of the passes below (the number of keys
//    may be one a GPU pass wrote).
// 2. Per digit:
//    - `sortCount`: each workgroup counts the digits of its blocks of keys (a histogram
//      of 16 bins per workgroup);
//    - `sortReduce`: the histograms summed in chunks, per bin;
//    - `sortScan` (one workgroup): the chunks' sums scanned, bin after bin: where each
//      bin's keys begin;
//    - `sortScanAdd`: each workgroup's histogram scanned within its chunk, plus the
//      chunk's start: where each workgroup's keys of each digit go;
//    - `sortScatter` (`sortScatterPayload` with a payload): each workgroup sorts 128 keys
//      at a time by the digit in shared memory (two stable 2-bit splits) and writes them,
//      in order, after the keys of that digit written before.
//
// The order of the passes and the shared-memory layouts follow AMD's FidelityFX Parallel
// Sort (MIT, FFX_ParallelSort.h v1.x, THIRD_PARTY_NOTICES.md); the code is the engine's.
// No single-pass scan with decoupled look-back: NVIDIA patents it (US 9,928,033 B2), and
// Vulkan does not promise the forward progress it needs.

static const uint BITS = 4;
static const uint BINS = 16;
static const uint GROUP = 128;
static const uint PER_THREAD = 4;
static const uint BLOCK = GROUP * PER_THREAD;

// `SortConstants` in sort.rs (64 bytes), written by `sortSetup`.
struct SortConstants
{
    uint keys;
    // Blocks of BLOCK keys per count / scatter workgroup.
    uint blocks_per_group;
    // Count / scatter workgroups.
    uint groups;
    // The last `groups_with_extra` of them take one more block.
    uint groups_with_extra;
    // Reduce workgroups per bin.
    uint reduce_per_bin;
    // Values `sortScan` scans: BINS × reduce_per_bin.
    uint scan_values;
    uint _pad0;
    uint _pad1;
    // `vkCmdDispatchIndirect` arguments: count and scatter, then reduce and scan add
    // (the fourth word unused).
    uint4 count_args;
    uint4 reduce_args;
};

// `SortPush` in sort.rs (80 bytes).
struct SortPush
{
    uint* keys_in;
    uint* keys_out;
    uint* values_in;
    uint* values_out;
    // Per bin, per count workgroup: the counts, then where the keys go.
    uint* sums;
    // Per bin, per reduce workgroup: the chunks' sums, then where they begin.
    uint* reduced;
    SortConstants* constants;
    // The number of keys a GPU pass wrote (`sortSetup` with `count_from_gpu`).
    uint* count;
    // The digit of this pass: the lowest bit.
    uint shift;
    // `sortSetup`: the keys when not from the GPU, and at most that many from it.
    uint capacity;
    uint count_from_gpu;
    // `sortSetup`: the most count / scatter workgroups.
    uint max_groups;
};

[[vk::push_constant]]
ConstantBuffer<SortPush> push;

[shader("compute")]
[numthreads(1, 1, 1)]
void sortSetup()
{
    uint keys = push.capacity;
    if (push.count_from_gpu != 0)
        keys = min(push.count[0], push.capacity);
    uint blocks = (keys + BLOCK - 1) / BLOCK;
    uint groups = push.max_groups;
    uint blocks_per_group = blocks / groups;
    uint groups_with_extra = blocks % groups;
    if (blocks < groups)
    {
        blocks_per_group = 1;
        groups = blocks;
        groups_with_extra = 0;
    }
    uint reduce_per_bin = groups > BLOCK ? (groups + BLOCK - 1) / BLOCK : 1;
    SortConstants c;
    c.keys = keys;
    c.blocks_per_group = blocks_per_group;
    c.groups = groups;
    c.groups_with_extra = groups_with_extra;
    c.reduce_per_bin = reduce_per_bin;
    c.scan_values = BINS * reduce_per_bin;
    c._pad0 = 0;
    c._pad1 = 0;
    // No keys: no workgroups (a dispatch of zero workgroups does nothing).
    c.count_args = uint4(groups, 1, 1, 0);
    c.reduce_args = uint4(groups == 0 ? 0 : BINS * reduce_per_bin, 1, 1, 0);
    push.constants[0] = c;
}

// The keys a count or scatter workgroup takes: its first key and how many blocks.
void group_blocks(SortConstants c, uint group, out uint start, out uint blocks)
{
    start = BLOCK * c.blocks_per_group * group;
    blocks = c.blocks_per_group;
    uint plain = c.groups - c.groups_with_extra;
    if (group >= plain)
    {
        start += (group - plain) * BLOCK;
        blocks++;
    }
}

groupshared uint g_histogram[BINS * GROUP];

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void sortCount(uint3 thread : SV_GroupThreadID, uint3 workgroup : SV_GroupID)
{
    SortConstants c = push.constants[0];
    uint lane = thread.x;
    uint group = workgroup.x;
    for (uint b = 0; b < BINS; b++)
        g_histogram[b * GROUP + lane] = 0;
    GroupMemoryBarrierWithGroupSync();

    uint start, blocks;
    group_blocks(c, group, start, blocks);
    // Each thread counts into its own column: no two threads add to one counter.
    uint index = start + lane;
    for (uint block = 0; block < blocks; block++, index += BLOCK)
    {
        [unroll]
        for (uint k = 0; k < PER_THREAD; k++)
        {
            uint i = index + k * GROUP;
            if (i < c.keys)
            {
                uint digit = (push.keys_in[i] >> push.shift) & (BINS - 1);
                g_histogram[digit * GROUP + lane]++;
            }
        }
    }
    GroupMemoryBarrierWithGroupSync();

    if (lane < BINS)
    {
        uint sum = 0;
        for (uint t = 0; t < GROUP; t++)
            sum += g_histogram[lane * GROUP + t];
        push.sums[lane * c.groups + group] = sum;
    }
}

groupshared uint g_waves[GROUP];

// The sum over the workgroup of every thread's `value` (thread 0 holds it).
uint group_sum(uint value, uint lane)
{
    uint wave = lane / WaveGetLaneCount();
    uint sum = WaveActiveSum(value);
    if (WaveIsFirstLane())
        g_waves[wave] = sum;
    GroupMemoryBarrierWithGroupSync();
    if (wave == 0)
        sum = WaveActiveSum(lane < GROUP / WaveGetLaneCount() ? g_waves[lane] : 0);
    return sum;
}

// The sum of `value` over the threads before this one in the workgroup.
uint group_prefix(uint value, uint lane)
{
    uint wave = lane / WaveGetLaneCount();
    uint prefix = WavePrefixSum(value);
    if (WaveGetLaneIndex() == WaveGetLaneCount() - 1)
        g_waves[wave] = prefix + value;
    GroupMemoryBarrierWithGroupSync();
    if (wave == 0)
        g_waves[lane] = WavePrefixSum(lane < GROUP / WaveGetLaneCount() ? g_waves[lane] : 0);
    GroupMemoryBarrierWithGroupSync();
    prefix += g_waves[wave];
    // Before `g_waves` is written again.
    GroupMemoryBarrierWithGroupSync();
    return prefix;
}

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void sortReduce(uint3 thread : SV_GroupThreadID, uint3 workgroup : SV_GroupID)
{
    SortConstants c = push.constants[0];
    uint lane = thread.x;
    uint group = workgroup.x;
    uint bin = group / c.reduce_per_bin;
    uint base = (group % c.reduce_per_bin) * BLOCK;
    uint sum = 0;
    [unroll]
    for (uint k = 0; k < PER_THREAD; k++)
    {
        uint i = base + k * GROUP + lane;
        if (i < c.groups)
            sum += push.sums[bin * c.groups + i];
    }
    sum = group_sum(sum, lane);
    if (lane == 0)
        push.reduced[group] = sum;
}

groupshared uint g_scan[PER_THREAD][GROUP];

// Exclusive scan, in place, of `count` values from `values[offset + base]` on, BLOCK at
// most, plus `add` to each.
void scan_block(uint* values, uint offset, uint base, uint count, uint add, uint lane)
{
    // Loads of neighbouring values by neighbouring threads, kept so that each thread
    // then holds PER_THREAD consecutive values.
    [unroll]
    for (uint k = 0; k < PER_THREAD; k++)
    {
        uint e = k * GROUP + lane;
        uint i = base + e;
        g_scan[e % PER_THREAD][e / PER_THREAD] = i < count ? values[offset + i] : 0;
    }
    GroupMemoryBarrierWithGroupSync();

    uint sum = 0;
    [unroll]
    for (uint k = 0; k < PER_THREAD; k++)
    {
        uint value = g_scan[k][lane];
        g_scan[k][lane] = sum;
        sum += value;
    }
    uint before = group_prefix(sum, lane) + add;
    [unroll]
    for (uint k = 0; k < PER_THREAD; k++)
        g_scan[k][lane] += before;
    GroupMemoryBarrierWithGroupSync();

    [unroll]
    for (uint k = 0; k < PER_THREAD; k++)
    {
        uint e = k * GROUP + lane;
        uint i = base + e;
        if (i < count)
            values[offset + i] = g_scan[e % PER_THREAD][e / PER_THREAD];
    }
}

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void sortScan(uint3 thread : SV_GroupThreadID)
{
    SortConstants c = push.constants[0];
    scan_block(push.reduced, 0, 0, c.scan_values, 0, thread.x);
}

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void sortScanAdd(uint3 thread : SV_GroupThreadID, uint3 workgroup : SV_GroupID)
{
    SortConstants c = push.constants[0];
    uint group = workgroup.x;
    uint bin = group / c.reduce_per_bin;
    uint base = (group % c.reduce_per_bin) * BLOCK;
    scan_block(push.sums, bin * c.groups, base, c.groups, push.reduced[group], thread.x);
}

groupshared uint g_offsets[BINS];
groupshared uint g_local[BINS];
groupshared uint g_starts[BINS];
groupshared uint g_total;
groupshared uint g_moved[GROUP];

// The scatter of one digit pass: VALUES moves a payload along with the keys.
void scatter<let VALUES : bool>(uint lane, uint group)
{
    SortConstants c = push.constants[0];
    // Where this workgroup's keys of each digit go.
    if (lane < BINS)
        g_offsets[lane] = push.sums[lane * c.groups + group];
    GroupMemoryBarrierWithGroupSync();

    uint start, blocks;
    group_blocks(c, group, start, blocks);
    uint index = start + lane;
    for (uint block = 0; block < blocks; block++, index += BLOCK)
    {
        for (uint k = 0; k < PER_THREAD; k++)
        {
            uint i = index + k * GROUP;
            if (lane < BINS)
                g_local[lane] = 0;
            // Past the end: a key that sorts last and is not written.
            uint key = i < c.keys ? push.keys_in[i] : 0xFFFFFFFFu;
            uint value = 0;
            if (VALUES && i < c.keys)
                value = push.values_in[i];

            // The workgroup's 128 keys sorted by the digit, stable: two 2-bit splits. Each
            // thread's place comes from a scan of four 8-bit counters packed in a word
            // (at most 128 keys per counter).
            for (uint bit = 0; bit < BITS; bit += 2)
            {
                uint two = (((key >> push.shift) & (BINS - 1)) >> bit) & 3;
                uint packed = 1u << (two * 8);
                uint before = group_prefix(packed, lane);
                if (lane == GROUP - 1)
                    g_total = before + packed;
                GroupMemoryBarrierWithGroupSync();
                // Keys of the smaller 2-bit values come first.
                uint total = g_total;
                before += (total << 8) + (total << 16) + (total << 24);
                uint place = (before >> (two * 8)) & 0xFF;
                g_moved[place] = key;
                GroupMemoryBarrierWithGroupSync();
                key = g_moved[lane];
                GroupMemoryBarrierWithGroupSync();
                if (VALUES)
                {
                    g_moved[place] = value;
                    GroupMemoryBarrierWithGroupSync();
                    value = g_moved[lane];
                    GroupMemoryBarrierWithGroupSync();
                }
            }

            // The keys are in digit order across the workgroup: each key's place among
            // those of its digit is its thread less the first thread with that digit.
            uint digit = (key >> push.shift) & (BINS - 1);
            InterlockedAdd(g_local[digit], 1);
            GroupMemoryBarrierWithGroupSync();
            uint first = WavePrefixSum(lane < BINS ? g_local[lane] : 0);
            if (lane < BINS)
                g_starts[lane] = first;
            GroupMemoryBarrierWithGroupSync();
            uint target = g_offsets[digit] + lane - g_starts[digit];
            if (target < c.keys)
            {
                push.keys_out[target] = key;
                if (VALUES)
                    push.values_out[target] = value;
            }
            GroupMemoryBarrierWithGroupSync();
            // The next 128 keys of each digit go after these.
            if (lane < BINS)
                g_offsets[lane] += g_local[lane];
            GroupMemoryBarrierWithGroupSync();
        }
    }
}

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void sortScatter(uint3 thread : SV_GroupThreadID, uint3 workgroup : SV_GroupID)
{
    scatter<false>(thread.x, workgroup.x);
}

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void sortScatterPayload(uint3 thread : SV_GroupThreadID, uint3 workgroup : SV_GroupID)
{
    scatter<true>(thread.x, workgroup.x);
}
