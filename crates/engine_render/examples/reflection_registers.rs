//! Prints the driver's register counts of the forward shading and reflection pipelines
//! (ADR 0073): `ENGINE_PIPELINE_STATS=1 ENGINE_ADAPTER=radeon cargo run --release -p
//! engine_render --example reflection_registers`. Faster than `scene_bench
//! --pipeline-stats` for iterating on shader code: no scene is loaded.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_render::forward::{ForwardRenderer, ShadowSettings};
use engine_render::post::HDR_FORMAT;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Bindless, Device, Instance, InstanceDesc};

fn main() {
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("reflection_registers"),
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
    println!("{}", device.adapter().summary());
    let bindless = Bindless::new(&device).unwrap();
    let mut renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        vk::Format::D32_SFLOAT,
        ShadowSettings::default(),
    )
    .unwrap();
    renderer.set_reflections(&device, &bindless, true).unwrap();
    let vgprs = |stats: &[engine_rhi::PipelineStatistic]| {
        stats
            .iter()
            .filter(|s| s.name.contains("numUsedVgprs") || s.name.contains("scratch"))
            .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let names = [
        "plain",
        "local lights",
        "environment",
        "lights + environment",
    ];
    for (name, stats) in names.iter().zip(renderer.shading_statistics(&device)) {
        println!("{name}: {}", vgprs(&stats));
    }
    let names = [
        "reflections shading",
        "reflections shading + lights",
        "composite, probes",
        "composite, SSR",
        "composite, probes + SSR",
    ];
    for (name, stats) in names.iter().zip(renderer.reflection_statistics(&device)) {
        println!("{name}: {}", vgprs(&stats));
    }
}
