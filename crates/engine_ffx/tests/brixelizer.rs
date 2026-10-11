//! Brixelizer and Brixelizer GI built from the SDK (ADR 0090), through the raw interface,
//! under the validation layer: a distance field of one box is built over every cascade,
//! its statistics read back and its memory counted, and a GI context is created and
//! destroyed over it. `engine_brixelizer`'s tests cover the GI's output.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_ffx::brixelizer::{self as brix, CreateDesc, GiCreateDesc, InternalResolution};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Device, Instance, InstanceDesc, MemoryLocation,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_ffx brixelizer test"),
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

/// A host-visible storage buffer holding `bytes`.
fn buffer(allocator: &mut Allocator, bytes: &[u8]) -> (vk::Buffer, Allocation) {
    let (buffer, mut allocation) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes.len() as u64)
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
            MemoryLocation::Dynamic,
        )
        .unwrap();
    allocation.mapped_slice_mut().unwrap()[..bytes.len()].copy_from_slice(bytes);
    (buffer, allocation)
}

#[test]
fn a_box_builds_a_distance_field_and_a_gi_context_over_it() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    let mut allocator = Allocator::new(&device);
    // A closed box from -1 to 1: 8 corners, 12 triangles.
    let corners: Vec<f32> = (0..8u32)
        .flat_map(|i| {
            [i & 1, (i >> 1) & 1, (i >> 2) & 1].map(|b| if b == 1 { 1.0f32 } else { -1.0 })
        })
        .collect();
    let faces: [u32; 36] = [
        0, 2, 1, 1, 2, 3, 4, 5, 6, 5, 7, 6, 0, 1, 4, 1, 5, 4, 2, 6, 3, 3, 6, 7, 0, 4, 2, 2, 4, 6,
        1, 3, 5, 3, 7, 5,
    ];
    let positions: Vec<u8> = corners.iter().flat_map(|v| v.to_le_bytes()).collect();
    let indices: Vec<u8> = faces.iter().flat_map(|v| v.to_le_bytes()).collect();
    let (position_buffer, position_memory) = buffer(&mut allocator, &positions);
    let (index_buffer, index_memory) = buffer(&mut allocator, &indices);

    engine_ffx::register_modules();
    let desc = CreateDesc {
        device: engine_ffx::Device::new(&device),
        center: [0.0; 3],
        cascade_count: 4,
        voxel_size: 0.25,
        cascade_ratio: 2.0,
        dynamic: 1,
        max_references: 1 << 20,
        triangle_swap_size: 16 << 20,
        max_bricks_per_bake: 1 << 14,
        readback: 1,
        max_gi_contexts: 1,
    };
    let mut context = core::ptr::null_mut();
    // SAFETY: the device is alive; the modules were handed over.
    let created = unsafe { brix::engine_brixelizer_create(&desc, &mut context) };
    assert_eq!(created, 0);
    let (mut vertex_index, mut index_index) = (0, 0);
    // SAFETY: a live context and live buffers.
    unsafe {
        assert_eq!(
            brix::engine_brixelizer_register_buffer(
                context,
                position_buffer,
                positions.len() as u32,
                &mut vertex_index,
            ),
            0
        );
        assert_eq!(
            brix::engine_brixelizer_register_buffer(
                context,
                index_buffer,
                indices.len() as u32,
                &mut index_index,
            ),
            0
        );
    }
    let instance_desc = brix::Instance {
        aabb_min: [-1.0; 3],
        aabb_max: [1.0; 3],
        transform: [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
        index_buffer: index_index,
        index_offset: 0,
        triangle_count: 12,
        vertex_buffer: vertex_index,
        vertex_offset: 0,
        vertex_stride: 12,
        vertex_count: 8,
        dynamic: 0,
    };
    let mut id = 0;
    // SAFETY: a live context; one instance, one id.
    let created =
        unsafe { brix::engine_brixelizer_create_instances(context, &instance_desc, 1, &mut id) };
    assert_eq!(created, 0);

    let d = device.raw();
    // SAFETY: plain object creation.
    let (pool, cmd, fence) = unsafe {
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
        let fence = d
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .unwrap();
        (pool, cmd, fence)
    };
    // Every cascade updated a few times: cascade i every 2^(i+1) frames.
    for frame in 0..32u32 {
        // SAFETY: one recording at a time, waited for below.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
            let update = brix::UpdateDesc {
                command_buffer: cmd,
                center: [0.0; 3],
                frame_index: frame,
            };
            assert_eq!(brix::engine_brixelizer_update(context, &update), 0);
            d.end_command_buffer(cmd).unwrap();
            d.queue_submit(
                device.graphics.raw,
                &[vk::SubmitInfo::default().command_buffers(&[cmd])],
                fence,
            )
            .unwrap();
            d.wait_for_fences(&[fence], true, u64::MAX).unwrap();
            d.reset_fences(&[fence]).unwrap();
        }
    }
    let mut stats = brix::Stats::default();
    // SAFETY: a live context.
    let memory = unsafe {
        brix::engine_brixelizer_stats(context, &mut stats);
        brix::engine_brixelizer_gpu_memory(context)
    };
    println!("{stats:?}, {} MiB", memory >> 20);
    // The box's surface took bricks from the 2^18 of the atlas.
    assert!(
        stats.free_bricks > 0 && stats.free_bricks < 1 << 18,
        "{stats:?}"
    );
    assert!(stats.scratch_needed <= stats.scratch_size, "{stats:?}");
    // The 512³ atlas alone is 128 MiB.
    assert!(memory >= 128 << 20, "{memory}");

    let gi_desc = GiCreateDesc {
        width: 64,
        height: 64,
        internal_resolution: InternalResolution::Percent50,
        specular: 0,
    };
    let mut gi = core::ptr::null_mut();
    // SAFETY: a live context.
    let created = unsafe { brix::engine_brixelizer_gi_create(context, &gi_desc, &mut gi) };
    assert_eq!(created, 0);
    // SAFETY: a live GI context, never used on the GPU.
    let gi_memory = unsafe { brix::engine_brixelizer_gi_gpu_memory(gi) };
    println!("GI context: {} MiB", gi_memory >> 20);
    // The radiance cache alone (256³ R11G11B10) is 64 MiB.
    assert!(gi_memory >= 64 << 20, "{gi_memory}");

    // SAFETY: the GPU is idle (every submission was waited for).
    unsafe {
        brix::engine_brixelizer_gi_destroy(gi);
        brix::engine_brixelizer_destroy(context);
        d.destroy_fence(fence, None);
        d.destroy_command_pool(pool, None);
        d.destroy_buffer(position_buffer, None);
        d.destroy_buffer(index_buffer, None);
    }
    allocator.free(position_memory);
    allocator.free(index_memory);
    drop(allocator);
    drop(device);
    let report = instance.validation_report();
    assert_eq!(
        report.errors, 0,
        "validation errors: {:#?}",
        report.first_errors
    );
}
