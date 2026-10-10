//! Pipelines described up front and created on first use, and the device's warm-up
//! thread (ADR 0079).
//!
//! GPU tests (see `gpu.rs`): without a Vulkan 1.3 implementation they print why and pass
//! unless `ENGINE_REQUIRE_GPU=1`. The process-wide creation counters are compared, so
//! this file keeps one test that creates pipelines.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use crate::common;

use common::{assert_no_validation_errors, gpu};
use engine_rhi::ash::vk;
use engine_rhi::{
    Device, GraphicsPipelineDesc, LazyPipeline, PipelineDesc, ShaderStageDesc,
    SharedPipelineLayout, creation_stats, spirv_words,
};
use std::time::{Duration, Instant};

const VERTEX: &[u8] = include_bytes!("../../shaders/triangle.vert.spv");
const FRAGMENT: &[u8] = include_bytes!("../../shaders/triangle.frag.spv");

/// A value no earlier run used: the pipelines' cache keys are new to the drivers' caches.
fn nonce() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        | 1
}

/// `count` distinct graphics pipelines described (the triangle shaders declare no
/// specialization constants, so `[nonce, i]` changes only the keys).
fn describe(
    device: &Device,
    layout: &SharedPipelineLayout,
    nonce: u32,
    count: u32,
) -> Vec<LazyPipeline> {
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
    // The words are dropped here: the descriptions own copies.
    LazyPipeline::describe_many(device, layout, &descs)
}

fn empty_layout(device: &Device) -> SharedPipelineLayout {
    // SAFETY: an empty layout on a live device, owned by the shared layout from then on.
    unsafe {
        let raw = device
            .raw()
            .create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None)
            .unwrap();
        SharedPipelineLayout::new(device, raw)
    }
}

#[test]
fn lazy_pipelines_are_created_once_by_whoever_needs_them_first() {
    let Some((instance, mut device)) = gpu(false) else {
        return;
    };
    let layout = empty_layout(&device);
    const COUNT: u32 = 12;

    // Without the warm-up: only what is asked for is created.
    device.set_pipeline_warm_up(false);
    let before = creation_stats();
    let lazy = describe(&device, &layout, nonce(), COUNT);
    assert_eq!(
        creation_stats().created,
        before.created,
        "describing creates nothing"
    );
    assert!(lazy.iter().all(|p| !p.is_settled() && p.get().is_none()));
    // First use on this thread; again: the same pipeline.
    let first = lazy[0].get_or_create().unwrap().raw();
    assert_eq!(lazy[0].get_or_create().unwrap().raw(), first);
    assert_eq!(lazy[0].clone().get().map(|p| p.raw()), Some(first));
    // A frame's set, with a repeat and one already made: the others are made in parallel.
    LazyPipeline::create_all(&[&lazy[0], &lazy[1], &lazy[2], &lazy[2], &lazy[3]]).unwrap();
    assert!(lazy[..4].iter().all(|p| p.get().is_some()));
    assert!(lazy[4..].iter().all(|p| !p.is_settled()));
    assert_eq!(lazy[0].get().unwrap().raw(), first);
    assert_eq!(creation_stats().created - before.created, 4);
    assert_eq!(device.pipelines_warming_up(), 0);

    // With it: what is described is made in the background, each pipeline once.
    device.set_pipeline_warm_up(true);
    let before = creation_stats();
    let warmed = describe(&device, &layout, nonce(), COUNT);
    device.wait_for_pipeline_warm_up();
    assert_eq!(device.pipelines_warming_up(), 0);
    assert!(warmed.iter().all(|p| p.get().is_some()));
    let after = creation_stats();
    println!("warm-up: {before:?} -> {after:?}");
    assert_eq!(after.created - before.created, u64::from(COUNT));

    // The warm-up and a frame wanting the same new pipelines at once: all made, and the
    // handles a frame got are the ones kept.
    let racing = describe(&device, &layout, nonce(), COUNT);
    let all: Vec<&LazyPipeline> = racing.iter().rev().collect();
    LazyPipeline::create_all(&all).unwrap();
    let handles: Vec<_> = racing.iter().map(|p| p.get().unwrap().raw()).collect();
    device.wait_for_pipeline_warm_up();
    assert_eq!(
        racing
            .iter()
            .map(|p| p.get().unwrap().raw())
            .collect::<Vec<_>>(),
        handles
    );

    // Descriptions dropped while queued are skipped; dropping the device with the warm-up
    // busy stops it after the pipeline it creates. The layout stays alive for that one.
    let dropped = describe(&device, &layout, nonce(), 64);
    drop(dropped);
    let many = describe(&device, &layout, nonce(), 64);
    drop((lazy, warmed, racing, layout));
    let start = Instant::now();
    let made = many.iter().filter(|p| p.is_settled()).count();
    drop(many);
    drop(device);
    let stopped = start.elapsed();
    println!("a busy warm-up stopped with its device in {stopped:?} ({made} of 64 made)");
    assert!(stopped < Duration::from_secs(2), "{stopped:?}");
    assert_no_validation_errors(&instance);
}
