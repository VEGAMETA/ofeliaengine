//! The persistent pipeline cache and parallel pipeline creation (ADR 0055).
//!
//! GPU tests (see `gpu.rs`): without a Vulkan 1.3 implementation they print why and pass
//! unless `ENGINE_REQUIRE_GPU=1`. The process-wide creation counters are compared before
//! and after, so this file keeps one test that creates pipelines.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use crate::common;

use common::{assert_no_validation_errors, gpu};
use engine_core::jobs::ThreadPool;
use engine_rhi::ash::vk;
use engine_rhi::{
    Device, GraphicsPipelineDesc, Instance, Pipeline, PipelineCacheLoad, PipelineCacheSave,
    PipelineDesc, RhiError, ShaderStageDesc, creation_stats, spirv_words,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const VERTEX: &[u8] = include_bytes!("../../shaders/triangle.vert.spv");
const FRAGMENT: &[u8] = include_bytes!("../../shaders/triangle.frag.spv");

/// A directory of this test's own, emptied.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("engine_rhi_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Creates `count` distinct graphics pipelines through `Pipeline::create_many`. The
/// triangle shaders declare no specialization constants, so a value of this run's own
/// (constant id 0, unused) gives the pipelines cache keys no earlier run made.
fn create(device: &Device, layout: vk::PipelineLayout, nonce: u32, count: u32) -> Vec<Pipeline> {
    let (vertex, fragment) = (spirv_words(VERTEX).unwrap(), spirv_words(FRAGMENT).unwrap());
    let formats = [vk::Format::R8G8B8A8_UNORM];
    let specializations: Vec<[u32; 2]> = (0..count).map(|i| [nonce, i]).collect();
    let descs: Vec<PipelineDesc<'_>> = specializations
        .iter()
        .map(|specialization| {
            PipelineDesc::Graphics(GraphicsPipelineDesc {
                color_formats: &formats,
                depth_test: false,
                depth_write: false,
                specialization,
                ..GraphicsPipelineDesc::new(
                    ShaderStageDesc {
                        spirv: &vertex,
                        entry: c"vertexMain",
                    },
                    Some(ShaderStageDesc {
                        spirv: &fragment,
                        entry: c"fragmentMain",
                    }),
                )
            })
        })
        .collect();
    let pipelines = Pipeline::create_many(device, layout, &descs).unwrap();
    assert_eq!(pipelines.len(), count as usize);
    pipelines
}

fn empty_layout(device: &Device) -> vk::PipelineLayout {
    // SAFETY: an empty layout on a live device.
    unsafe {
        device
            .raw()
            .create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None)
    }
    .unwrap()
}

fn destroy_layout(device: &Device, layout: vk::PipelineLayout) {
    // SAFETY: no pipeline created with it is in use.
    unsafe { device.raw().destroy_pipeline_layout(layout, None) };
}

/// A second device on the instance's selected adapter.
fn another_device(instance: &Instance, device: &Device) -> Device {
    Device::new(instance, device.adapter()).unwrap()
}

/// Rewrites the file at `path` with `change` applied to its bytes.
fn tamper(path: &Path, change: impl FnOnce(&mut Vec<u8>)) {
    let mut bytes = std::fs::read(path).unwrap();
    change(&mut bytes);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn the_cache_survives_a_restart_and_rejects_foreign_files() {
    let Some((instance, mut device)) = gpu(false) else {
        return;
    };
    let dir = scratch_dir("pipeline_cache");
    let path = dir.join(device.pipeline_cache_file_name());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        | 1;
    const COUNT: u32 = 6;

    // First run: no file; every pipeline is compiled.
    assert_eq!(
        device.load_pipeline_cache(&path),
        PipelineCacheLoad::Missing
    );
    let layout = empty_layout(&device);
    let before = creation_stats();
    let first = create(&device, layout, nonce, COUNT);
    let after = creation_stats();
    println!("first run: {:?} -> {:?}", before, after);
    assert_eq!(after.created - before.created, u64::from(COUNT));
    let written = device.save_pipeline_cache(&path).unwrap();
    let PipelineCacheSave::Written { bytes } = written else {
        panic!("the first save writes: {written:?}");
    };
    assert!(bytes > 52, "{bytes} bytes");
    assert!(
        !dir.join(format!("{}.tmp", device.pipeline_cache_file_name()))
            .exists()
    );
    // Nothing new: the file stays as it is.
    assert_eq!(
        device.save_pipeline_cache(&path).unwrap(),
        PipelineCacheSave::Unchanged
    );
    drop(first);
    destroy_layout(&device, layout);
    drop(device);

    // "Restart": a new device loads the file and takes every pipeline from the cache.
    let mut device = selected_device(&instance);
    let load = device.load_pipeline_cache(&path);
    assert!(
        matches!(load, PipelineCacheLoad::Loaded { bytes } if bytes > 0),
        "{load:?}"
    );
    let layout = empty_layout(&device);
    let before = creation_stats();
    let second = create(&device, layout, nonce, COUNT);
    let after = creation_stats();
    println!("second run: {:?} -> {:?}", before, after);
    assert_eq!(after.cache_hits - before.cache_hits, u64::from(COUNT));
    assert_eq!(after.cache_misses, before.cache_misses);
    drop(second);
    destroy_layout(&device, layout);

    // A damaged file is refused, and the cache keeps working.
    std::fs::copy(&path, dir.join("good.bin")).unwrap();
    tamper(&path, |bytes| {
        let last = bytes.len() - 1;
        bytes[last] ^= 0x5a;
    });
    let load = device.load_pipeline_cache(&path);
    assert!(
        matches!(&load, PipelineCacheLoad::Rejected(why) if why.contains("checksum")),
        "{load:?}"
    );
    // A file from another GPU (its device id changed in the envelope).
    std::fs::copy(dir.join("good.bin"), &path).unwrap();
    tamper(&path, |bytes| bytes[12] ^= 0xff);
    let load = device.load_pipeline_cache(&path);
    assert!(
        matches!(&load, PipelineCacheLoad::Rejected(why) if why.contains("another GPU")),
        "{load:?}"
    );
    // Not a cache file at all.
    std::fs::write(&path, b"hello").unwrap();
    assert!(matches!(
        device.load_pipeline_cache(&path),
        PipelineCacheLoad::Rejected(_)
    ));
    let layout = empty_layout(&device);
    drop(create(&device, layout, nonce ^ 0xffff, 2));
    destroy_layout(&device, layout);
    // Saving replaces the refused file with a good one.
    assert!(matches!(
        device.save_pipeline_cache(&path).unwrap(),
        PipelineCacheSave::Written { .. }
    ));
    let mut fresh = another_device(&instance, &device);
    assert!(matches!(
        fresh.load_pipeline_cache(&path),
        PipelineCacheLoad::Loaded { .. }
    ));
    drop(fresh);
    drop(device);
    let _ = std::fs::remove_dir_all(&dir);
    assert_no_validation_errors(&instance);
}

/// A device on the adapter `gpu` selects, again.
fn selected_device(instance: &Instance) -> Device {
    Device::new(
        instance,
        &instance.select_adapter(&common::preference()).unwrap(),
    )
    .unwrap()
}

#[test]
fn parallel_creation_keeps_order_and_reports_the_first_error() {
    let Some((instance, mut device)) = gpu(false) else {
        return;
    };
    let check = |device: &Device| {
        let values = device.create_parallel(100, |i| Ok(i * 3)).unwrap();
        assert_eq!(values, (0..100).map(|i| i * 3).collect::<Vec<_>>());
        let failed = device.create_parallel(100, |i| {
            if i == 30 || i == 70 {
                Err(RhiError::ShaderInterface(format!("{i}")))
            } else {
                Ok(i)
            }
        });
        assert!(
            matches!(&failed, Err(RhiError::ShaderInterface(i)) if i == "30"),
            "{failed:?}"
        );
        assert!(
            device
                .create_parallel(0, Ok::<usize, RhiError>)
                .unwrap()
                .is_empty()
        );
    };
    // Threads of its own, then the engine's job pool.
    check(&device);
    device.set_job_pool(Arc::new(ThreadPool::builder().workers(3).build().unwrap()));
    check(&device);
    drop(device);
    assert_no_validation_errors(&instance);
}
