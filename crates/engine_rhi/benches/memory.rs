//! GPU memory sub-allocation: the engine's allocator (TLSF, ADR 0010) against
//! gpu-allocator 0.28 (free list, best fit). Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_rhi --bench memory
//! ```
//!
//! Both allocators create real `VkDeviceMemory` blocks on the selected device and serve
//! the same deterministic sequence of requests. Timed phases are pure sub-allocation: a
//! live population keeps the blocks in use. Heap allocations are counted by a global
//! allocator wrapper.

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, AllocationRequest, Allocator, Device, Instance, InstanceDesc,
    MemoryLocation, ResourceKind,
};
use gpu_allocator::vulkan as ga;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct CountingAllocator;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// The two allocators behind one interface.
trait Subject {
    type Handle;
    fn allocate(&mut self, size: u64, align: u64) -> Self::Handle;
    fn free(&mut self, handle: Self::Handle);
    fn device_memory_objects(&self) -> u64;
    fn block_bytes(&self) -> u64;
}

struct Engine {
    allocator: Allocator,
    memory_type_bits: u32,
}

impl Subject for Engine {
    type Handle = engine_rhi::Allocation;
    fn allocate(&mut self, size: u64, align: u64) -> Self::Handle {
        self.allocator
            .allocate(&AllocationRequest {
                requirements: vk::MemoryRequirements {
                    size,
                    alignment: align,
                    memory_type_bits: self.memory_type_bits,
                },
                location: MemoryLocation::Device,
                kind: ResourceKind::Buffer,
                prefers_dedicated: false,
                requires_dedicated: false,
                dedicated_to: None,
            })
            .expect("engine allocation")
    }
    fn free(&mut self, handle: Self::Handle) {
        self.allocator.free(handle);
    }
    fn device_memory_objects(&self) -> u64 {
        u64::from(self.allocator.device_memory_count())
    }
    fn block_bytes(&self) -> u64 {
        self.allocator
            .heap_stats()
            .iter()
            .map(|s| s.block_bytes + s.dedicated_bytes)
            .sum()
    }
}

struct GpuAllocator {
    allocator: ga::Allocator,
    memory_type_bits: u32,
}

impl Subject for GpuAllocator {
    type Handle = ga::Allocation;
    fn allocate(&mut self, size: u64, align: u64) -> Self::Handle {
        self.allocator
            .allocate(&ga::AllocationCreateDesc {
                name: "bench",
                requirements: vk::MemoryRequirements {
                    size,
                    alignment: align,
                    memory_type_bits: self.memory_type_bits,
                },
                location: gpu_allocator::MemoryLocation::GpuOnly,
                linear: true,
                allocation_scheme: ga::AllocationScheme::GpuAllocatorManaged,
            })
            .expect("gpu-allocator allocation")
    }
    fn free(&mut self, handle: Self::Handle) {
        self.allocator.free(handle).expect("gpu-allocator free");
    }
    fn device_memory_objects(&self) -> u64 {
        self.allocator.generate_report().blocks.len() as u64
    }
    fn block_bytes(&self) -> u64 {
        self.allocator.generate_report().total_capacity_bytes
    }
}

/// xorshift64: the same sequence for both allocators.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// 256 B … 1 MiB, log-uniform; alignment 256 B … 64 KiB.
    fn request(&mut self) -> (u64, u64) {
        let size = 256u64 << (self.next() % 13);
        let size = size + (self.next() % size) / 2;
        let align = [256u64, 1024, 4096, 65536][(self.next() % 4) as usize];
        (size, align)
    }
}

struct Result {
    ns_per_pair: f64,
    heap_allocs_per_pair: f64,
    objects: u64,
    overhead: f64,
}

/// `live` allocations, then `pairs` × (free a random one, allocate a new one), timed.
fn churn<S: Subject>(subject: &mut S, live_count: usize, pairs: usize, seed: u64) -> Result {
    let mut rng = Rng(seed);
    let mut live: Vec<(S::Handle, u64)> = Vec::with_capacity(live_count);
    let mut live_bytes = 0;
    for _ in 0..live_count {
        let (size, align) = rng.request();
        live.push((subject.allocate(size, align), size));
        live_bytes += size;
    }
    // Warm-up with the same pattern, untimed.
    let mut step = |subject: &mut S, live: &mut Vec<(S::Handle, u64)>, live_bytes: &mut u64| {
        let victim = (rng.next() % live.len() as u64) as usize;
        let (handle, size) = live.swap_remove(victim);
        subject.free(handle);
        *live_bytes -= size;
        let (size, align) = rng.request();
        live.push((subject.allocate(size, align), size));
        *live_bytes += size;
    };
    for _ in 0..pairs {
        step(subject, &mut live, &mut live_bytes);
    }
    let allocs_before = ALLOCATIONS.load(Ordering::Relaxed);
    let start = Instant::now();
    for _ in 0..pairs {
        step(subject, &mut live, &mut live_bytes);
    }
    let elapsed = start.elapsed();
    let heap = ALLOCATIONS.load(Ordering::Relaxed) - allocs_before;
    let result = Result {
        ns_per_pair: elapsed.as_nanos() as f64 / pairs as f64,
        heap_allocs_per_pair: heap as f64 / pairs as f64,
        objects: subject.device_memory_objects(),
        overhead: subject.block_bytes() as f64 / live_bytes as f64,
    };
    for (handle, _) in live {
        subject.free(handle);
    }
    result
}

fn median(mut runs: Vec<Result>) -> Result {
    runs.sort_by(|a, b| a.ns_per_pair.total_cmp(&b.ns_per_pair));
    runs.swap_remove(runs.len() / 2)
}

fn main() {
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("engine_rhi memory bench"),
        validation: false,
        debug_utils: false,
        surface: false,
        headless_surface: false,
    })
    .expect("Vulkan instance");
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).expect("adapter");
    let device = Device::new(&instance, &adapter).expect("device");

    // The memory types a storage buffer may use.
    let probe = vk::BufferCreateInfo::default()
        .size(1 << 20)
        .usage(vk::BufferUsageFlags::STORAGE_BUFFER);
    // SAFETY: valid create info; destroyed right after the query.
    let memory_type_bits = unsafe {
        let buffer = device.raw().create_buffer(&probe, None).unwrap();
        let bits = device
            .raw()
            .get_buffer_memory_requirements(buffer)
            .memory_type_bits;
        device.raw().destroy_buffer(buffer, None);
        bits
    };

    println!("### GPU memory sub-allocation\n");
    println!("Device: {}\n", device.adapter().summary());
    println!(
        "Each run: N live allocations (256 B – 1.5 MiB, log-uniform; alignment 256 B – 64 KiB), \
         then 50 000 × (free a random one + allocate a new one), after an identical untimed warm-up. \
         Median of 5 runs. Overhead = bytes in VkDeviceMemory / live bytes.\n"
    );
    println!(
        "| Allocator | live | ns per free+allocate | heap allocations per pair | VkDeviceMemory objects | overhead |"
    );
    println!("|---|---|---|---|---|---|");
    for live in [1_000usize, 4_000, 16_000] {
        let pairs = 50_000;
        let engine = median(
            (0..5)
                .map(|run| {
                    let mut subject = Engine {
                        allocator: Allocator::new(&device),
                        memory_type_bits,
                    };
                    churn(&mut subject, live, pairs, 0x9E37_79B9 + run)
                })
                .collect(),
        );
        let reference = median(
            (0..5)
                .map(|run| {
                    let mut subject = GpuAllocator {
                        allocator: ga::Allocator::new(&ga::AllocatorCreateDesc {
                            instance: device.instance_raw().clone(),
                            device: device.raw().clone(),
                            physical_device: device.physical(),
                            debug_settings: gpu_allocator::AllocatorDebugSettings::default(),
                            buffer_device_address: true,
                            allocation_sizes: gpu_allocator::AllocationSizes::default(),
                        })
                        .expect("gpu-allocator"),
                        memory_type_bits,
                    };
                    churn(&mut subject, live, pairs, 0x9E37_79B9 + run)
                })
                .collect(),
        );
        for (name, r) in [
            ("engine (TLSF)", &engine),
            ("gpu-allocator 0.28", &reference),
        ] {
            println!(
                "| {name} | {live} | {:.0} | {:.2} | {} | {:.2} |",
                r.ns_per_pair, r.heap_allocs_per_pair, r.objects, r.overhead
            );
        }
    }
}
