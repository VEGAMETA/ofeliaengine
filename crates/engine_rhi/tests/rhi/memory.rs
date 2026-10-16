//! Device memory allocator on a real device (lavapipe in containers, NVIDIA and AMD on
//! ref-1). Runs under the validation layer; any validation error fails the test.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use crate::common;

use common::{assert_no_validation_errors, gpu};
use engine_rhi::ash::vk;
use engine_rhi::{Allocator, MemoryLocation, ResourceKind};

fn buffer_info(size: u64, usage: vk::BufferUsageFlags) -> vk::BufferCreateInfo<'static> {
    vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
}

#[test]
fn upload_copy_and_read_back() {
    let Some((instance, device)) = gpu(false) else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    const SIZE: u64 = 1 << 20;

    let (upload, mut upload_mem) = allocator
        .create_buffer(
            &buffer_info(SIZE, vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Upload,
        )
        .unwrap();
    let (gpu_buffer, gpu_mem) = allocator
        .create_buffer(
            &buffer_info(
                SIZE,
                vk::BufferUsageFlags::TRANSFER_SRC
                    | vk::BufferUsageFlags::TRANSFER_DST
                    | vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
            ),
            MemoryLocation::Device,
        )
        .unwrap();
    let (readback, readback_mem) = allocator
        .create_buffer(
            &buffer_info(SIZE, vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();

    // Buffer device address works on sub-allocated buffers (blocks carry DEVICE_ADDRESS).
    // SAFETY: the buffer was created with SHADER_DEVICE_ADDRESS usage.
    let address = unsafe {
        d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(gpu_buffer))
    };
    assert_ne!(address, 0);
    println!(
        "upload type {} offset {}, device type {} offset {}, readback type {} offset {}, address {address:#x}",
        upload_mem.memory_type(),
        upload_mem.offset(),
        gpu_mem.memory_type(),
        gpu_mem.offset(),
        readback_mem.memory_type(),
        readback_mem.offset()
    );

    let pattern: Vec<u8> = (0..SIZE as usize).map(|i| (i * 7 + 3) as u8).collect();
    upload_mem
        .mapped_slice_mut()
        .expect("upload memory is mapped")
        .copy_from_slice(&pattern);

    // SAFETY: straightforward one-shot submission; every handle is alive until the
    // device is idle below.
    unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(device.graphics.family),
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
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .unwrap();
        let region = [vk::BufferCopy::default().size(SIZE)];
        d.cmd_copy_buffer(cmd, upload, gpu_buffer, &region);
        let barrier = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)];
        d.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().memory_barriers(&barrier),
        );
        d.cmd_copy_buffer(cmd, gpu_buffer, readback, &region);
        let to_host = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::HOST)
            .dst_access_mask(vk::AccessFlags2::HOST_READ)];
        d.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().memory_barriers(&to_host),
        );
        d.end_command_buffer(cmd).unwrap();
        let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
        d.queue_submit2(
            device.graphics.raw,
            &[vk::SubmitInfo2::default().command_buffer_infos(&cmds)],
            vk::Fence::null(),
        )
        .unwrap();
        device.wait_idle().unwrap();
        d.destroy_command_pool(pool, None);
    }

    let ptr = readback_mem
        .mapped_ptr()
        .expect("readback memory is mapped");
    // SAFETY: the copy has completed (device idle); the mapping covers SIZE bytes.
    let result = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), SIZE as usize) };
    assert!(result == pattern.as_slice(), "read-back data differs");

    let stats = allocator.heap_stats();
    println!("heaps: {stats:#?}");
    assert_eq!(stats.iter().map(|s| s.allocations).sum::<u32>(), 3);

    // SAFETY: the device is idle; each pair comes from one create call.
    unsafe {
        allocator.destroy_buffer(upload, upload_mem);
        allocator.destroy_buffer(gpu_buffer, gpu_mem);
        allocator.destroy_buffer(readback, readback_mem);
    }
    assert!(allocator.heap_stats().iter().all(|s| s.allocations == 0));
    drop(allocator);
    assert_no_validation_errors(&instance);
}

#[test]
fn blocks_are_shared_released_and_large_resources_get_their_own_memory() {
    let Some((instance, device)) = gpu(false) else {
        return;
    };
    let mut allocator = Allocator::new(&device);

    // 1000 small buffers: a handful of blocks, not 1000 device memory objects.
    let mut buffers = Vec::new();
    for i in 0..1000u64 {
        let size = 4096 + (i * 7919) % (60 * 1024);
        buffers.push(
            allocator
                .create_buffer(
                    &buffer_info(size, vk::BufferUsageFlags::STORAGE_BUFFER),
                    MemoryLocation::Device,
                )
                .unwrap(),
        );
    }
    let small_blocks = allocator.device_memory_count();
    println!("1000 buffers in {small_blocks} device memory objects");
    assert!(small_blocks <= 4, "{small_blocks}");

    // Optimal-tiling images live in their own pool.
    let mut images = Vec::new();
    for size in [64u32, 256, 1024] {
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width: size,
                height: size,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST);
        images.push(
            allocator
                .create_image(&info, MemoryLocation::Device)
                .unwrap(),
        );
    }

    // Larger than half a block: a dedicated allocation.
    let (big, big_mem) = allocator
        .create_buffer(
            &buffer_info(200 << 20, vk::BufferUsageFlags::STORAGE_BUFFER),
            MemoryLocation::Device,
        )
        .unwrap();
    assert!(big_mem.is_dedicated());
    assert_eq!(big_mem.offset(), 0);

    // Per-frame data is mapped wherever it lands.
    let (dynamic, dynamic_mem) = allocator
        .create_buffer(
            &buffer_info(64 * 1024, vk::BufferUsageFlags::UNIFORM_BUFFER),
            MemoryLocation::Dynamic,
        )
        .unwrap();
    assert!(dynamic_mem.mapped_ptr().is_some());

    let peak = allocator.device_memory_count();
    // SAFETY: nothing was submitted; each pair comes from one create call.
    unsafe {
        allocator.destroy_buffer(big, big_mem);
        allocator.destroy_buffer(dynamic, dynamic_mem);
        for (image, memory) in images {
            allocator.destroy_image(image, memory);
        }
        for (buffer, memory) in buffers {
            allocator.destroy_buffer(buffer, memory);
        }
    }
    let left = allocator.device_memory_count();
    println!("device memory objects: peak {peak}, after freeing everything {left}");
    // At most one empty block kept per pool used (buffers, images, dynamic).
    assert!(left <= 3, "{left}");
    let stats = allocator.heap_stats();
    assert!(
        stats
            .iter()
            .all(|s| s.allocations == 0 && s.dedicated_allocations == 0)
    );
    drop(allocator);
    assert_no_validation_errors(&instance);
}

/// A few KiB of staging or readback take a small block of mapped system memory, not an
/// eighth of the preferred block: every byte of it counts as the process's private bytes.
#[test]
fn small_staging_takes_a_small_block_of_system_memory() {
    let Some((instance, device)) = gpu(false) else {
        return;
    };
    let mut allocator = Allocator::new(&device);
    let mut made = Vec::new();
    for location in [MemoryLocation::Upload, MemoryLocation::Readback] {
        made.push(
            allocator
                .create_buffer(
                    &buffer_info(4096, vk::BufferUsageFlags::TRANSFER_DST),
                    location,
                )
                .unwrap(),
        );
    }
    let local = |heap: usize| {
        device.adapter().memory.memory_heaps[heap]
            .flags
            .contains(vk::MemoryHeapFlags::DEVICE_LOCAL)
    };
    for (heap, stats) in allocator.heap_stats().iter().enumerate() {
        println!(
            "heap {heap} (device-local {}): {} bytes in {} blocks, {} mapped",
            local(heap),
            stats.block_bytes,
            stats.blocks,
            stats.mapped_bytes
        );
        if !local(heap) && stats.blocks > 0 {
            assert!(
                stats.block_bytes <= 2 * (4 << 20),
                "{} bytes for two 4 KiB buffers",
                stats.block_bytes
            );
        }
    }
    // SAFETY: nothing was submitted; each pair comes from one create call.
    unsafe {
        for (buffer, memory) in made {
            allocator.destroy_buffer(buffer, memory);
        }
    }
    drop(allocator);
    assert_no_validation_errors(&instance);
}

/// The allocator's own count of device-local bytes follows the OS's usage
/// (`VK_EXT_memory_budget`): what streaming budgets are computed from (ADR 0076).
#[test]
fn budget_accounting_follows_the_os_usage() {
    let Some((instance, device)) = gpu(false) else {
        return;
    };
    let mut allocator = Allocator::new(&device);
    let before = allocator.budget();
    if !before.from_os {
        println!("no VK_EXT_memory_budget: nothing to compare");
        return;
    }
    let local = before.device_local();
    println!(
        "device-local: size {} MiB, budget {} MiB, usage {} MiB, ours {} MiB",
        local.size >> 20,
        local.budget >> 20,
        local.usage >> 20,
        local.allocated >> 20
    );
    assert!(local.budget > 0 && local.budget <= local.size);
    // 48 textures of 2048² RGBA8 (16 MiB each, 768 MiB): blocks and dedicated memory.
    let mut images = Vec::new();
    for _ in 0..48 {
        images.push(
            allocator
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .extent(vk::Extent3D {
                            width: 2048,
                            height: 2048,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::SAMPLED)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    MemoryLocation::Device,
                )
                .unwrap(),
        );
    }
    let after = allocator.budget().device_local();
    let ours = after.allocated - local.allocated;
    let os = after.usage.saturating_sub(local.usage);
    println!(
        "after 768 MiB of images: ours +{} MiB, OS usage +{} MiB, budget {} MiB",
        ours >> 20,
        os >> 20,
        after.budget >> 20
    );
    assert!(ours >= 768 << 20, "{ours}");
    // Within 5% (plus 16 MiB for the driver's own): what the allocator holds is what the
    // OS counts.
    let tolerance = ours / 20 + (16 << 20);
    assert!(
        os.abs_diff(ours) <= tolerance,
        "OS usage +{os} against ours +{ours}"
    );
    for (image, memory) in images {
        // SAFETY: never used by the GPU; each pair comes from one create call.
        unsafe { allocator.destroy_image(image, memory) };
    }
    let freed = allocator.budget().device_local();
    // Freed memory leaves the usage (the allocator may keep one empty block).
    assert!(
        freed.usage <= local.usage + (256 << 20),
        "{} after {}",
        freed.usage,
        local.usage
    );
    drop(allocator);
    assert_no_validation_errors(&instance);
}

/// A pool's blocks double as it grows, so its newest block is often mostly free: the
/// budget reports that room (`HeapBudget::unused`, which streaming takes first, ADR 0076),
/// and of two empty blocks the larger is released, the smaller kept against thrashing.
#[test]
fn free_room_in_blocks_is_reported_and_the_larger_empty_block_goes() {
    let Some((instance, device)) = gpu(false) else {
        return;
    };
    let mut allocator = Allocator::new(&device);
    let buffers_of = |allocator: &Allocator| -> Vec<(u64, u64)> {
        allocator
            .blocks()
            .iter()
            .filter(|b| b.kind == ResourceKind::Buffer)
            .map(|b| (b.capacity, b.used))
            .collect()
    };
    // 1 MiB buffers until the pool holds two blocks (the second twice the first).
    let mut buffers = Vec::new();
    while buffers_of(&allocator).len() < 2 && buffers.len() < 1000 {
        buffers.push(
            allocator
                .create_buffer(
                    &buffer_info(1 << 20, vk::BufferUsageFlags::STORAGE_BUFFER),
                    MemoryLocation::Device,
                )
                .unwrap(),
        );
    }
    let blocks = buffers_of(&allocator);
    println!(
        "{} buffers of 1 MiB in blocks {:?} MiB",
        buffers.len(),
        blocks
            .iter()
            .map(|&(c, u)| (c >> 20, u >> 20))
            .collect::<Vec<_>>()
    );
    assert_eq!(blocks.len(), 2, "two blocks");
    assert!(blocks[1].0 > blocks[0].0, "the second block is larger");
    // The room in them, as the budget reports it.
    let free: u64 = blocks.iter().map(|&(c, u)| c - u).sum();
    let unused = allocator.budget().device_local().unused;
    assert_eq!(
        unused, free,
        "the budget's unused bytes are the blocks' free bytes"
    );
    assert_eq!(allocator.device_local_unused_bytes(), free);
    assert!(
        unused >= blocks[1].0 - (2 << 20),
        "the newest block is mostly free"
    );
    // Everything freed, the newest block's first (it is then the pool's only empty block):
    // the smaller block stays, the larger goes.
    for (buffer, memory) in buffers.into_iter().rev() {
        // SAFETY: never used by the GPU; each pair comes from one create call.
        unsafe { allocator.destroy_buffer(buffer, memory) };
    }
    let left = buffers_of(&allocator);
    println!(
        "after freeing: blocks {:?} MiB",
        left.iter()
            .map(|&(c, u)| (c >> 20, u >> 20))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        left,
        vec![(blocks[0].0, 0)],
        "the smaller empty block is kept"
    );
    drop(allocator);
    assert_no_validation_errors(&instance);
}
