//! Prints the driver's register counts of the shading models' pipelines (ADR 0092) beside
//! the engine's own shading: the material types of `tests/golden/materials/models.slang`
//! and the engine's built-in glTF types (`shaders/materials/gltf.slang`: glTF's unlit, clear
//! coat and sheen), compiled and made as a frame would ask for them. `ENGINE_PIPELINE_STATS=1
//! ENGINE_ADAPTER=radeon cargo run --release -p engine_render --example material_registers`.
//!
//! `-- --module <file.slang> --types A,B` measures those types instead (ADR 0123's
//! measurement: `bench/graph-numbers/graph_numbers.slang`).

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_render::forward::{ForwardRenderer, MaterialTypeDesc, ShadowSettings};
use engine_render::post::HDR_FORMAT;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Bindless, Device, Instance, InstanceDesc};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The types in `models.slang`, glTF's lighting first.
const TYPES: [&str; 6] = [
    "LitMaterial",
    "UnlitMaterial",
    "ClearcoatMaterial",
    "ClothMaterial",
    "HairMaterial",
    "ToonMaterial",
];

/// The built-in types of glTF's extensions (`engine_assets::gltf_models`).
const GLTF_TYPES: [&str; 4] = [
    "GltfUnlit",
    "GltfClearcoat",
    "GltfSheen",
    "GltfSheenClearcoat",
];

fn main() {
    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("material_registers"),
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
    // `--module <file> --types A,B`: those types instead of the shading models'.
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|k| args.get(k + 1))
            .cloned()
    };
    let (module, names, gltf_names): (PathBuf, Vec<String>, Vec<String>) =
        match (arg("--module"), arg("--types")) {
            (Some(module), Some(types)) => (
                std::fs::canonicalize(&module).unwrap(),
                types.split(',').map(str::to_owned).collect(),
                Vec::new(),
            ),
            _ => (
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tests/golden/materials/models.slang"),
                TYPES.map(String::from).to_vec(),
                GLTF_TYPES.map(String::from).to_vec(),
            ),
        };
    let types: Vec<_> = names
        .iter()
        .map(|name| {
            renderer.register_material_type(MaterialTypeDesc {
                name: name.clone(),
                module: module.clone(),
                type_name: name.clone(),
            })
        })
        .collect();
    let gltf_types: Vec<_> = gltf_names
        .iter()
        .map(|name| {
            renderer.register_material_type(MaterialTypeDesc {
                name: name.clone(),
                module: PathBuf::from("materials/gltf.slang"),
                type_name: name.clone(),
            })
        })
        .collect();
    // Ask for every type's shading, then make it.
    for &ty in types.iter().chain(&gltf_types) {
        let _ = renderer.material_type_statistics(&device, ty);
    }
    let start = Instant::now();
    assert!(
        renderer.wait_for_material_types(&device, Duration::from_secs(120)),
        "the types' pipelines took over 2 minutes"
    );
    let made = engine_rhi::creation_stats();
    println!(
        "types' pipelines made in {:.2} s; the driver's creation calls {:.1} ms over {} pipelines ({} from the cache)",
        start.elapsed().as_secs_f64(),
        made.time.as_secs_f64() * 1e3,
        made.created,
        made.cache_hits
    );
    let vgprs = |stats: &[engine_rhi::PipelineStatistic]| {
        if stats.is_empty() {
            return String::from("(none: ENGINE_PIPELINE_STATS=1, or the type failed)");
        }
        stats
            .iter()
            // AMD's vector registers and scratch; NVIDIA's registers.
            .filter(|s| {
                s.name.contains("numUsedVgprs")
                    || s.name.contains("scratch")
                    || s.name == "Register Count"
            })
            .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let variants = [
        "plain",
        "local lights",
        "environment",
        "lights + environment",
    ];
    for (variant, stats) in variants.iter().zip(renderer.shading_statistics(&device)) {
        println!("engine, {variant}: {}", vgprs(&stats));
    }
    for (name, &ty) in names
        .iter()
        .chain(&gltf_names)
        .zip(types.iter().chain(&gltf_types))
    {
        for (variant, stats) in variants
            .iter()
            .zip(renderer.material_type_statistics(&device, ty))
        {
            println!("{name}, {variant}: {}", vgprs(&stats));
        }
    }
}
