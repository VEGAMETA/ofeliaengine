//! How long the engine's pipelines take to create: every renderer the engine ships, one
//! after another, timed (ADR 0055).
//!
//! `cargo run -p engine_upscale --release --example pipeline_bench -- [--cache <file>]`
//!
//! - `--cache <file>` loads the pipeline cache from the file first and saves it after.
//! - `ENGINE_ADAPTER=radeon` picks the other GPU.
//! - `ENGINE_PIPELINE_SALT=<n>` gives every pipeline a cache key no cache has seen: a cold
//!   compile (a new `n` each run). Drivers also keep caches of their own, per executable:
//!   a copy of this program under another name starts cold for the driver but can still
//!   load `--cache`.
//!
//! Prints a Markdown table: per renderer, its pipelines and milliseconds, then the totals.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_render::environment::{Environment, Sky};
use engine_render::forward::{ForwardRenderer, ShadowSettings};
use engine_render::post::{HDR_FORMAT, Tonemap};
use engine_render::postfx::{AutoExposure, Bloom, BloomSettings, ExposureSettings};
use engine_render::subsurface::SubsurfaceProfile;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, creation_stats,
};
use engine_upscale::Taau;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut cache: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cache" => cache = args.next().map(PathBuf::from),
            other => panic!("unknown argument {other}"),
        }
    }
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("pipeline_bench"),
        validation: false,
        debug_utils: false,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .unwrap();
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    let mut device = Device::new(&instance, &adapter).unwrap();

    println!("### Pipeline creation");
    println!();
    println!("Device: {}", device.adapter().summary());
    println!(
        "Salt: {}",
        std::env::var("ENGINE_PIPELINE_SALT").unwrap_or_else(|_| String::from("none"))
    );
    if let Some(path) = &cache {
        let t = Instant::now();
        let load = device.load_pipeline_cache(path);
        println!(
            "Cache: {} → {load:?} in {:.1} ms",
            path.display(),
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    let device = device;
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    println!();
    println!("| renderer | pipelines | ms |");
    println!("|---|---:|---:|");
    let start = Instant::now();
    let row = |name: &str, create: &mut dyn FnMut()| {
        let before = creation_stats().created;
        let t = Instant::now();
        create();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        println!(
            "| {name} | {} | {ms:.1} |",
            creation_stats().created - before
        );
    };
    let mut keep_forward = None;
    row("forward", &mut || {
        keep_forward = Some(
            ForwardRenderer::new(
                &device,
                &bindless,
                HDR_FORMAT,
                vk::Format::D32_SFLOAT,
                ShadowSettings::default(),
            )
            .unwrap(),
        );
    });
    // Subsurface materials' pipelines come with the first profiles (ADR 0059).
    row("subsurface (first profiles)", &mut || {
        if let Some(forward) = keep_forward.as_mut() {
            forward
                .set_subsurface_profiles(&device, &bindless, &[SubsurfaceProfile::skin()])
                .unwrap();
        }
    });
    let mut keep_tonemap = None;
    row("tone map", &mut || {
        keep_tonemap = Some(Tonemap::new(&device, &bindless, vk::Format::B8G8R8A8_SRGB).unwrap());
    });
    let mut keep_bloom = None;
    row("bloom", &mut || {
        keep_bloom = Some(Bloom::new(&device, &bindless, BloomSettings::default()).unwrap());
    });
    let mut keep_exposure = None;
    row("exposure", &mut || {
        keep_exposure = Some(
            AutoExposure::new(
                &device,
                &mut allocator,
                &bindless,
                ExposureSettings::default(),
            )
            .unwrap(),
        );
    });
    let mut keep_environment = None;
    row("environment", &mut || {
        keep_environment =
            Some(Environment::new(&device, &mut allocator, &bindless, Sky::default()).unwrap());
    });
    let mut keep_taau = None;
    row("taau", &mut || {
        keep_taau = Some(Taau::new(&device, &bindless, 2).unwrap());
    });
    let stats = creation_stats();
    println!(
        "| **total** | **{}** | **{:.1}** |",
        stats.created,
        start.elapsed().as_secs_f64() * 1e3
    );
    println!();
    println!(
        "Cache lookups: {} hits, {} misses; time in creation calls {:.1} ms (summed over threads)",
        stats.cache_hits,
        stats.cache_misses,
        stats.time.as_secs_f64() * 1e3
    );
    if let Some(path) = &cache {
        let t = Instant::now();
        let save = device.save_pipeline_cache(path).unwrap();
        println!(
            "Saved: {save:?} in {:.1} ms",
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    device.wait_idle().unwrap();
}
