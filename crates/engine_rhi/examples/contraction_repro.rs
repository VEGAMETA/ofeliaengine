//! Does the Vulkan driver honour NoContraction (GLSL `precise`)? Runs `contraction.comp`
//! (next to this file; its variants say what each tests) over 65,536 items and counts the
//! items whose difference of two equal values is not exactly zero. It is the driver bug
//! behind the vendored Brixelizer's holes on AMD (ADR 0090, `third_party/README.md`):
//! AMD's driver 26.8.1 fuses the difference into an FMA (allowed) and keeps doing so under
//! NoContraction when an undecorated twin of the subtraction is used too (variant 3: not
//! allowed). Re-run it after a driver update; reported to AMD on 2026-10-11.
//!
//! ```text
//! for v in 0 1 2 3 4; do glslangValidator -V --target-env vulkan1.1 -DVARIANT=$v //!     -o variant$v.spv crates/engine_rhi/examples/contraction.comp; done
//! cargo run -p engine_rhi --example contraction_repro -- variant0.spv variant1.spv ...
//! ```
//!
//! (`ENGINE_ADAPTER=radeon` for the iGPU.) On 2026-10-11, items with `e.y != 0` in
//! variants 0–4: NVIDIA 616.92 none; AMD 26.8.1 6,921 / 0 / 0 / 6,921 / 0.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc, MemoryLocation};

const ITEMS: usize = 65_536;

/// A small deterministic generator (no dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    assert!(!paths.is_empty(), "give the SPIR-V files of the variants");
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("contraction repro"),
        validation: true,
        debug_utils: true,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .unwrap();
    let adapter = instance
        .select_adapter(
            &AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance),
        )
        .unwrap();
    let device = Device::new(&instance, &adapter).unwrap();
    println!("{}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let d = device.raw().clone();

    // Inputs: per item wp0, edge (y exactly 0), brick_min, size (a voxel / 7, as
    // Brixelizer's bricks of 8 distances over a voxel).
    let mut rng = Lcg(0x5eed);
    let mut inputs = Vec::with_capacity(ITEMS * 16);
    let voxels = [0.1f32, 0.2, 0.4, 0.8];
    for i in 0..ITEMS {
        let voxel = voxels[i % voxels.len()];
        let size = voxel / 7.0;
        let wp0 = [
            rng.next() * 60.0 - 30.0,
            rng.next() * 60.0 - 30.0,
            rng.next() * 60.0 - 30.0,
        ];
        let edge = [rng.next() * 80.0 - 40.0, 0.0, rng.next() * 80.0 - 40.0];
        let k = [
            (wp0[0] / voxel).floor(),
            (wp0[1] / voxel).floor(),
            (wp0[2] / voxel).floor(),
        ];
        let brick_min = [
            k[0] * voxel - size / 2.0,
            k[1] * voxel - size / 2.0,
            k[2] * voxel - size / 2.0,
        ];
        inputs.extend_from_slice(&[wp0[0], wp0[1], wp0[2], 0.0]);
        inputs.extend_from_slice(&[edge[0], edge[1], edge[2], 0.0]);
        inputs.extend_from_slice(&[brick_min[0], brick_min[1], brick_min[2], 0.0]);
        inputs.extend_from_slice(&[size, 0.0, 0.0, 0.0]);
    }
    let input_bytes = (inputs.len() * 4) as u64;
    let output_bytes = (ITEMS * 16) as u64;
    let usage = vk::BufferUsageFlags::STORAGE_BUFFER;
    let (input, input_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(input_bytes)
                .usage(usage),
            MemoryLocation::Upload,
        )
        .unwrap();
    let (output, output_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(output_bytes)
                .usage(usage),
            MemoryLocation::Readback,
        )
        .unwrap();
    // SAFETY: the mapping covers the buffer; nothing else uses it.
    unsafe {
        core::ptr::copy_nonoverlapping(
            inputs.as_ptr().cast::<u8>(),
            input_memory.mapped_ptr().unwrap().as_ptr(),
            input_bytes as usize,
        );
    }

    // SAFETY: plain object creation and one-shot submissions, each waited for.
    unsafe {
        let bindings = [0, 1].map(|binding| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        });
        let set_layout = d
            .create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
            .unwrap();
        let layouts = [set_layout];
        let layout = d
            .create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts),
                None,
            )
            .unwrap();
        let sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(2)];
        let pool = d
            .create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sizes),
                None,
            )
            .unwrap();
        let set = d
            .allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
            .unwrap()[0];
        let infos = [input, output].map(|buffer| {
            [vk::DescriptorBufferInfo::default()
                .buffer(buffer)
                .range(vk::WHOLE_SIZE)]
        });
        let writes = [0, 1].map(|i| {
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(i as u32)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&infos[i])
        });
        d.update_descriptor_sets(&writes, &[]);
        let command_pool = d
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
                    .command_pool(command_pool)
                    .command_buffer_count(1),
            )
            .unwrap()[0];

        for path in &paths {
            let bytes = std::fs::read(path).unwrap();
            let code: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let module = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
                .unwrap();
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            let pipeline = d
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(stage)
                        .layout(layout)],
                    None,
                )
                .unwrap()[0];
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
            d.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::COMPUTE, layout, 0, &[set], &[]);
            d.cmd_dispatch(cmd, (ITEMS / 64) as u32, 1, 1);
            let barrier = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ)];
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().memory_barriers(&barrier),
            );
            d.end_command_buffer(cmd).unwrap();
            let buffers = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&buffers)],
                vk::Fence::null(),
            )
            .unwrap();
            device.wait_idle().unwrap();
            let results = core::slice::from_raw_parts(
                output_memory
                    .mapped_ptr()
                    .unwrap()
                    .as_ptr()
                    .cast::<[f32; 4]>(),
                ITEMS,
            );
            let nonzero: Vec<&[f32; 4]> = results.iter().filter(|r| r[1] != 0.0).collect();
            let largest = nonzero.iter().map(|r| r[1].abs()).fold(0.0f32, f32::max);
            println!(
                "{path}: e.y != 0 in {} of {ITEMS} items (largest |e.y| {largest:e}); first: {:?}",
                nonzero.len(),
                nonzero.first()
            );
            d.destroy_pipeline(pipeline, None);
            d.destroy_shader_module(module, None);
        }
        d.destroy_command_pool(command_pool, None);
        d.destroy_descriptor_pool(pool, None);
        d.destroy_pipeline_layout(layout, None);
        d.destroy_descriptor_set_layout(set_layout, None);
        allocator.destroy_buffer(input, input_memory);
        allocator.destroy_buffer(output, output_memory);
    }
    drop(allocator);
    drop(device);
    let report = instance.validation_report();
    assert_eq!(
        report.errors, 0,
        "validation errors: {:#?}",
        report.first_errors
    );
}
