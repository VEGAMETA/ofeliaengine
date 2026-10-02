//! Streaming within the GPU memory budget (ADR 0076), measured: a cooked scene's textures
//! streamed from its pack while a camera flies through it, with texture feedback, at a
//! steady 60 frames per second (so the drive and decompression get real time).
//!
//! Reports, for the flight: memory resident against the budget, request-to-arrival
//! latency, the feedback pass's GPU cost, the texture pool's upload pass, GPU and CPU frame
//! times (median, 99th percentile, worst) and the frame's streaming work on the CPU.
//!
//! ```text
//! cargo run -p engine_streaming --release --example stream_bench -- <scene.pack>
//!     [--budget <MiB>]       the streaming pool (default: from the OS budget)
//!     [--frames <n>]         frames of flight (default 900: 15 s)
//!     [--fps <n>]            pacing (default 60; 0: as fast as possible)
//!     [--divisor <n>] [--interval <n>] [--slices <n>]   feedback target divisor, period
//!                            and slices of the draw commands per pass
//!     [--no-feedback]        no feedback pass (tails only): its cost by difference
//!     [--still]              no flight: the first view until everything settles
//!     [--synthetic <n>,<size>]   instead of a pack: n quads with their own size² BC7
//!                            texture each (a stress scene), cooked into a pack in the
//!                            temp directory
//! ```

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_assets::{
    CookedInstance, CookedMaterial, CookedMesh, CookedScene, EncodedTexture, TextureFormat,
    TextureSlot, TextureWrap, write_pack,
};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};
use engine_render::budget::{BudgetSettings, GpuBudget};
use engine_render::feedback::{FeedbackSettings, TextureFeedback};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{Access, RenderGraph, TextureDesc};
use engine_render::material::{MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId};
use engine_render::post::HDR_FORMAT;
use engine_render::streaming::LodView;
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};
use engine_streaming::{SceneStreamSettings, SceneStreamer, material_desc};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const MIB: f64 = (1u64 << 20) as f64;
/// A frame: its number, GPU and CPU ms, texture traffic (MiB), its longest pass and µs.
type FrameLog = (usize, f64, f64, f64, (&'static str, f64));

/// Frames left out of the statistics at the start.
const WARMUP: usize = 10;

fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// A stress scene: `count` quads of 8 × 8 units in a row along x, 2 units apart, each with
/// its own `size`² BC7 texture of hashed blocks.
fn synthetic(count: usize, size: u32) -> CookedScene {
    let levels = 32 - size.leading_zeros();
    let mut scene = CookedScene::default();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for t in 0..count {
        let levels: Vec<Vec<u8>> = (0..levels)
            .map(|k| {
                let s = (size >> k).max(1);
                let bytes = TextureFormat::Bc7Srgb.level_bytes(s, s);
                (0..bytes)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        (seed >> 24) as u8
                    })
                    .collect()
            })
            .collect();
        scene.textures.push(EncodedTexture {
            format: TextureFormat::Bc7Srgb,
            width: size,
            height: size,
            levels,
        });
        scene.materials.push(CookedMaterial {
            base_color: [1.0; 4],
            base_color_texture: Some(TextureSlot {
                texture: t as u32,
                wrap: [TextureWrap::Repeat; 2],
            }),
            metallic: 0.0,
            roughness: 0.8,
            metallic_roughness_texture: None,
            normal_texture: None,
            normal_scale: 1.0,
            occlusion_texture: None,
            occlusion_strength: 1.0,
            emissive: [0.0; 3],
            emissive_texture: None,
            alpha: engine_assets::AlphaMode::Opaque,
            double_sided: true,
        });
        scene.instances.push(CookedInstance {
            mesh: 0,
            material: Some(t as u32),
            world_from_object: Mat4::from_translation(Vec3::new(t as f32 * 10.0, 0.0, 0.0))
                .to_cols_array(),
            skin: None,
        });
    }
    // A floor quad (xz plane, facing up), 8 × 8 units.
    scene.meshes.push(CookedMesh {
        positions: vec![
            [-4.0, 0.0, -4.0],
            [4.0, 0.0, -4.0],
            [4.0, 0.0, 4.0],
            [-4.0, 0.0, 4.0],
        ],
        normals: vec![[0.0, 1.0, 0.0]; 4],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        tangents: vec![[1.0, 0.0, 0.0, 1.0]; 4],
        indices: vec![0, 2, 1, 0, 3, 2],
        min: [-4.0, 0.0, -4.0],
        max: [4.0, 0.0, 4.0],
        ..CookedMesh::default()
    });
    scene
}

fn main() {
    let mut path: Option<PathBuf> = None;
    let mut budget_mib: Option<u64> = None;
    let mut frames = 900usize;
    let mut fps = 60.0f64;
    let mut feedback_settings = FeedbackSettings::default();
    let mut use_feedback = true;
    let mut still = false;
    let mut verbose = false;
    let mut synthetic_scene: Option<(usize, u32)> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--budget" => budget_mib = args.next().map(|v| v.parse().unwrap()),
            "--frames" => frames = args.next().unwrap().parse().unwrap(),
            "--fps" => fps = args.next().unwrap().parse().unwrap(),
            "--divisor" => feedback_settings.divisor = args.next().unwrap().parse().unwrap(),
            "--interval" => feedback_settings.interval = args.next().unwrap().parse().unwrap(),
            "--slices" => feedback_settings.slices = args.next().unwrap().parse().unwrap(),
            "--no-feedback" => use_feedback = false,
            "--still" => still = true,
            "--verbose" => verbose = true,
            "--synthetic" => {
                let v = args.next().unwrap();
                let (n, s) = v.split_once(',').unwrap();
                synthetic_scene = Some((n.parse().unwrap(), s.parse().unwrap()));
            }
            _ => path = Some(PathBuf::from(arg)),
        }
    }
    let jobs = ThreadPool::builder().build().unwrap();
    if let Some((count, size)) = synthetic_scene {
        let file = std::env::temp_dir().join(format!("stream_bench_{count}_{size}.pack"));
        if !file.exists() {
            let started = Instant::now();
            let scene = synthetic(count, size);
            write_pack(&file, &scene, &jobs, 1).unwrap();
            println!(
                "cooked the stress scene into {} in {:.1} s",
                file.display(),
                started.elapsed().as_secs_f64()
            );
        }
        path = Some(file);
    }
    let Some(path) = path else {
        println!("give a pack (ofeliaengine cook <scene.gltf> -o <out.pack>) or --synthetic");
        return;
    };

    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("stream_bench"),
        validation: false,
        debug_utils: true,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .unwrap();
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    let device = Device::new(&instance, &adapter).unwrap();
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let depth_format = vk::Format::D32_SFLOAT;
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 64 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut budget = GpuBudget::new(&device);
    if let Some(mib) = budget_mib {
        budget.set_settings(BudgetSettings {
            streaming_limit: Some(mib << 20),
            ..budget.settings()
        });
    }
    // 32 MiB of texture traffic per frame on the GPU.
    let mut textures = TexturePool::new(&device, 32 << 20, 1);
    let opened = Instant::now();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mut streamer = SceneStreamer::open(
        &path,
        &jobs,
        &mut textures,
        &mut geometry,
        &mut allocator,
        &mut budget,
        SceneStreamSettings::default(),
    )
    .unwrap();
    let open_ms = opened.elapsed().as_secs_f64() * 1e3;
    let mut feedback = use_feedback
        .then(|| TextureFeedback::new(&device, &bindless, feedback_settings, 1).unwrap());

    // Meshes, materials, draws.
    let scene = streamer.layout().scene.clone();
    let meshes: Vec<MeshId> = streamer.meshes().to_vec();
    let mut materials = MaterialTable::new(1);
    let ids = streamer.textures().to_vec();
    let material_ids: Vec<MaterialId> = scene
        .materials
        .iter()
        .map(|m| materials.add(material_desc(m, &ids)))
        .collect();
    let fallback = materials.default_material();
    let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let mut draws: Vec<DrawItem> = Vec::new();
    for instance in &scene.instances {
        let world = Mat4::from_cols_array(&instance.world_from_object);
        let mesh = &scene.meshes[instance.mesh as usize];
        draws.push(DrawItem {
            mesh: meshes[instance.mesh as usize],
            material: instance
                .material
                .map_or(fallback, |i| material_ids[i as usize]),
            world_from_object: world,
        });
        for corner in 0..8 {
            let pick = |bit: usize, axis: usize| {
                if corner & bit != 0 {
                    mesh.max[axis]
                } else {
                    mesh.min[axis]
                }
            };
            let c = world.transform_point3(Vec3::new(pick(1, 0), pick(2, 1), pick(4, 2)));
            min = min.min(c);
            max = max.max(c);
        }
    }
    draws.sort_by_key(|d| format!("{:?}", d.mesh));
    let size = max - min;
    let full_bytes: u64 = streamer
        .layout()
        .textures
        .iter()
        .map(|r| {
            let format =
                engine_streaming::texture_format(TextureFormat::from_code(r.format).unwrap());
            engine_render::texture_pool::levels_bytes(format, r.width, r.height, r.levels, 0)
        })
        .sum();

    // The flight: along the longest horizontal axis and back, a fifth of the height up,
    // the view swinging ±60° around the direction of travel.
    let along_x = size.x >= size.z;
    let camera_at = |frame: usize| -> (Vec3, Vec3) {
        let t = if still {
            0.0
        } else {
            frame as f32 / frames.max(1) as f32
        };
        let leg = if t < 0.5 { t * 2.0 } else { 2.0 - t * 2.0 };
        let s = 0.1 + 0.8 * leg;
        let forward_sign = if t < 0.5 { 1.0 } else { -1.0 };
        let height = min.y + size.y.max(1.0) * 0.2 + if size.y < 1.0 { 1.5 } else { 0.0 };
        let center = (min + max) * 0.5;
        let eye = if along_x {
            Vec3::new(min.x + size.x * s, height, center.z)
        } else {
            Vec3::new(center.x, height, min.z + size.z * s)
        };
        let swing = (t * 6.0 * core::f32::consts::TAU).sin() * 60f32.to_radians();
        let ahead = if along_x {
            Vec3::new(forward_sign, 0.0, 0.0)
        } else {
            Vec3::new(0.0, 0.0, forward_sign)
        };
        let dir = Vec3::new(
            ahead.x * swing.cos() - ahead.z * swing.sin(),
            -0.15,
            ahead.x * swing.sin() + ahead.z * swing.cos(),
        );
        (eye, eye + dir)
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.2),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.08),
    };
    // SAFETY: plain object creation.
    let (command_pool, cmd) = unsafe {
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
        (command_pool, cmd)
    };

    println!("### Streaming within the GPU budget ({WIDTH}×{HEIGHT}, ADR 0076)");
    println!();
    println!("Device: {}", device.adapter().summary());
    println!(
        "Scene: {}: {} textures ({:.0} MiB with every level), {} draws; opened in {open_ms:.0} ms",
        path.display(),
        ids.len(),
        full_bytes as f64 / MIB,
        draws.len()
    );
    println!(
        "Budget: {}; feedback: {}",
        budget_mib.map_or(String::from("from the OS"), |m| format!(
            "{m} MiB streaming pool"
        )),
        if use_feedback {
            format!(
                "1/{} per side, every {} frame(s), {} slice(s)",
                feedback_settings.divisor, feedback_settings.interval, feedback_settings.slices
            )
        } else {
            String::from("off")
        }
    );
    println!();
    let period = if fps > 0.0 {
        Duration::from_secs_f64(1.0 / fps)
    } else {
        Duration::ZERO
    };
    let mut gpu_frame = Vec::new();
    let mut frame_log: Vec<FrameLog> = Vec::new();
    let mut cpu_frame = Vec::new();
    let mut stream_cpu = Vec::new();
    let mut feedback_gpu = Vec::new();
    let mut upload_gpu = Vec::new();
    let mut frame_bytes = Vec::new();
    let mut passes_us: std::collections::BTreeMap<&'static str, Vec<f64>> = Default::default();
    let mut over_budget_frames = 0usize;
    let mut peak_resident = 0u64;
    let mut settled: Option<usize> = None;
    let mut sleeper = engine_platform::time::PreciseSleeper::new().unwrap();
    let started = Instant::now();
    let total = if still { frames.max(1) } else { frames };
    for frame in 0..total {
        let frame_number = frame as u64 + 1;
        let deadline = started + period * frame as u32;
        sleeper.sleep_until(deadline).unwrap();
        let frame_start = Instant::now();
        let (eye, target) = camera_at(frame);
        let camera = Camera {
            clip_from_world: vulkan_perspective(
                60f32.to_radians(),
                WIDTH as f32 / HEIGHT as f32,
                size.length().max(1.0) * 0.0005,
                size.length().max(1.0) * 2.0,
            ) * look_at(eye, target, Vec3::Y),
            position: eye,
        };
        let t = Instant::now();
        let lod_view = LodView::new(&camera.clip_from_world, eye, HEIGHT, 1.0);
        streamer.update(
            &mut textures,
            &mut geometry,
            &mut allocator,
            &mut budget,
            feedback.as_mut(),
            Some(&lod_view),
            &materials,
            frame_number,
        );
        budget.update(&allocator);
        if frame >= WARMUP {
            stream_cpu.push(t.elapsed().as_secs_f64() * 1e3);
        }
        // SAFETY: one-shot recording and submission, then wait.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
        }
        ring.begin_slot(0);
        {
            // SAFETY: nothing is in flight (the previous frame was waited for).
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                .unwrap();
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                .unwrap();
            let color = fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
            let depth =
                fg.create_texture("depth", TextureDesc::new_2d(depth_format, WIDTH, HEIGHT));
            let passes = renderer
                .add_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    &draws,
                    &ForwardView {
                        camera,
                        light,
                        color,
                        depth,
                        clear: Some([0.3, 0.4, 0.5, 1.0]),
                        occlusion: None,
                        temporal: None,
                        lights: &[],
                        environment: None,
                        reflections: Default::default(),
                    },
                )
                .unwrap();
            if let Some(feedback) = feedback.as_mut() {
                feedback
                    .add_pass(
                        &mut fg,
                        &device,
                        &mut allocator,
                        &bindless,
                        &geo,
                        &mats,
                        &passes,
                        (WIDTH, HEIGHT),
                        0.0,
                        frame_number,
                    )
                    .unwrap();
            }
            fg.add_pass("consume")
                .read(color, Access::TransferRead)
                .side_effects()
                .execute(|_| {});
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        // SAFETY: as above.
        unsafe {
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                vk::Fence::null(),
            )
            .unwrap();
            device.wait_idle().unwrap();
        }
        // The first frames (pipelines' first use, the scene's meshes uploaded) are not
        // the flight's: left out of the statistics.
        let warm = frame >= WARMUP;
        let cpu_ms = frame_start.elapsed().as_secs_f64() * 1e3;
        let gpu_ms = graph.gpu_frame_time().map_or(0.0, |us| f64::from(us) / 1e3);
        let top = graph
            .gpu_pass_times()
            .iter()
            .max_by(|a, b| a.gpu_us.total_cmp(&b.gpu_us))
            .map_or(("", 0.0), |t| (t.name, f64::from(t.gpu_us)));
        frame_log.push((
            frame,
            gpu_ms,
            cpu_ms,
            textures.last_frame_bytes() as f64 / MIB,
            top,
        ));
        let mut fb = 0.0;
        let mut up = 0.0;
        for time in graph.gpu_pass_times().iter().filter(|_| warm) {
            passes_us
                .entry(time.name)
                .or_default()
                .push(f64::from(time.gpu_us));
            match time.name {
                "texture feedback" | "texture feedback clear" | "texture feedback readback" => {
                    fb += f64::from(time.gpu_us);
                }
                "texture pool upload" => up += f64::from(time.gpu_us),
                _ => {}
            }
        }
        if warm {
            cpu_frame.push(cpu_ms);
            gpu_frame.push(gpu_ms);
            if use_feedback {
                feedback_gpu.push(fb);
            }
            upload_gpu.push(up);
            frame_bytes.push(textures.last_frame_bytes() as f64 / MIB);
        }
        let (live, retiring) = textures.memory();
        let resident = live + retiring;
        peak_resident = peak_resident.max(resident);
        let b = budget.stats();
        let grant = budget.grant(streamer.texture_streamer().client());
        if resident > grant {
            over_budget_frames += 1;
        }
        if settled.is_none() && frame > 10 && streamer.idle() && textures.pending().0 == 0 {
            settled = Some(frame);
            if still {
                break;
            }
        }
        if frame % 120 == 0 || frame + 1 == total || verbose {
            let s = streamer.stats();
            println!(
                "frame {frame:4}: meshes {:5.2} MiB (grant {:7.1}, bias {}), textures {:6.1} MiB (grant {:6.1}, pool {:7.1}, demand {:6.1}), visible {:3}, bias {}, planned {:.2} of {:.2}, evictions {}, arrived {:5}, waiting {:3}, latency {:5.1} ms",
                s.meshes.resident as f64 / MIB,
                s.meshes.grant as f64 / MIB,
                s.meshes.pressure_bias,
                resident as f64 / MIB,
                grant as f64 / MIB,
                b.pool as f64 / MIB,
                b.demand as f64 / MIB,
                s.textures.visible,
                s.textures.pressure_bias,
                s.textures.planned as f64 / MIB,
                s.textures.grant as f64 / MIB,
                s.textures.evictions,
                s.arrived,
                s.waiting,
                s.latency_ms,
            );
        }
    }
    let s = streamer.stats();
    println!();
    println!("| measure | median | p99 | worst |");
    println!("|---|---:|---:|---:|");
    let row = |name: &str, v: &[f64], unit: &str| {
        println!(
            "| {name} | {:.3} {unit} | {:.3} {unit} | {:.3} {unit} |",
            percentile(v, 0.5),
            percentile(v, 0.99),
            percentile(v, 1.0)
        );
    };
    row("GPU frame", &gpu_frame, "ms");
    row("CPU frame (record, submit, wait)", &cpu_frame, "ms");
    row("streaming on the frame's thread", &stream_cpu, "ms");
    if use_feedback {
        row("feedback pass (GPU)", &feedback_gpu, "µs");
    }
    row("texture pool upload pass (GPU)", &upload_gpu, "µs");
    row("texture traffic per frame", &frame_bytes, "MiB");
    for (name, v) in &passes_us {
        if percentile(v, 0.5) >= 5.0 {
            row(&format!("pass `{name}`"), v, "µs");
        }
    }
    println!();
    println!(
        "Arrived {} levels ({} of meshes; {:.1} MiB read), failed {}; latency request→uploaded: mean {:.1} ms, worst {:.1} ms ({} frames)",
        s.arrived,
        s.mesh_levels,
        s.bytes as f64 / MIB,
        s.failed,
        s.latency_ms_sum / s.arrived.max(1) as f64,
        s.worst_latency_ms,
        s.worst_latency_frames,
    );
    println!(
        "Memory: peak {:.1} MiB of textures resident (all levels: {:.1} MiB); frames over the grant {over_budget_frames}; settled {}",
        peak_resident as f64 / MIB,
        full_bytes as f64 / MIB,
        settled.map_or(String::from("never"), |f| format!("at frame {f}")),
    );
    if s.mesh_vertices > 0 {
        println!(
            "Mesh levels: {} vertices decoded and staged in {:.2} ms ({:.0} ns per vertex), at most {:.2} ms in a frame",
            s.mesh_vertices,
            s.mesh_upload_ms_sum,
            s.mesh_upload_ms_sum * 1e6 / s.mesh_vertices as f64,
            s.mesh_upload_ms_max,
        );
    }
    println!(
        "Worst GPU frame / median: {:.2}×",
        percentile(&gpu_frame, 1.0) / percentile(&gpu_frame, 0.5).max(1e-9)
    );
    let median = percentile(&gpu_frame, 0.5);
    let slow: Vec<_> = frame_log
        .iter()
        .filter(|f| f.0 >= WARMUP && f.1 > 2.0 * median)
        .collect();
    println!(
        "Frames over twice the median GPU time (after the first {WARMUP}): {}",
        slow.len()
    );
    for f in slow.iter().take(8) {
        println!(
            "  frame {}: GPU {:.2} ms, CPU {:.2} ms, texture traffic {:.2} MiB, longest pass `{}` {:.0} µs",
            f.0, f.1, f.2, f.3, f.4.0, f.4.1
        );
    }
    if let Some(f) = feedback.as_ref()
        && device.pipeline_statistics_enabled()
    {
        for stat in f.statistics(&device) {
            println!("feedback pipeline: {stat:?}");
        }
    }

    // SAFETY: the device is idle.
    unsafe {
        device.wait_idle().unwrap();
        streamer.finish(&mut textures);
        if let Some(f) = feedback.as_mut() {
            f.destroy(&mut allocator);
        }
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(command_pool, None);
    }
    drop(feedback);
    drop(renderer);
}
