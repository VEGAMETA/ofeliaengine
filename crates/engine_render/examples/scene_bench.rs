//! Real content through the forward renderer, headless (ADR 0039, ADR 0040): a glTF scene
//! with its materials and textures, drawn into a 1920×1080 HDR target with a shadow map
//! from a fixed camera.
//!
//! Reports the import (glTF, then every texture decoded, given its mip chain and
//! block-compressed in parallel, or loaded from the derived data cache, then the meshes
//! into the geometry pool), the memory of geometry and
//! textures, and the median GPU time of the shadow and forward passes, with the materials
//! as imported and with their factors only (no textures). `--screenshot out.png` also
//! writes the textured frame, tonemapped.
//!
//! `cargo run -p engine_render --release --example scene_bench -- test-assets/Sponza/Sponza.gltf`
//! (scenes: `tools/fetch-test-assets.ps1`; options: `--screenshot <png>`,
//! `--view <eye x,y,z,target x,y,z>`, `--uncompressed` for RGBA8 textures, `--prepass` for the depth prepass,
//! `--ao low|medium|high|ultra|<slices>x<steps>` for ambient occlusion (ADR 0058, implies the prepass),
//! `--motion` for a jittered projection and motion vectors (ADR 0043), `--cascades <1-4>`
//! `--shadow-res <texels per cascade>` and `--shadow-blend <band>` for the shadows
//! (ADR 0044), `--lights <count>` for point and spot lights spread over the scene and
//! `--no-light-culling` to evaluate every light for every pixel (ADR 0045),
//! `--shadowed-lights <count>` for shadows of the first lights (ADR 0071; at least that many
//! lights), `--local-shadow-budget <views>`, `--local-shadow-atlas <texels>` and
//! `--local-shadow-res <min>,<max>` for their settings, `--local-shadows-every-frame` to draw
//! every shadow again every frame (the cache dropped), `--contact bend|march` for the sun's
//! contact shadows (ADR 0071; implies the prepass), `--sky` for
//! image-based light and the sky from an environment and `--sky-every-frame` to rebuild it
//! every frame (ADR 0046), `--atmosphere` for the physically based sky and atmosphere
//! instead of the gradient sky (ADR 0069; implies `--sky`), `--sun <elevation°>` for the sun's
//! height (default: the scene's light), `--atmosphere-every-frame` to rebuild its tables and
//! the image-based light every frame, `--fog` for exponential height fog (implies `--sky`),
//! `--post` for bloom, automatic exposure and tone mapping every
//! frame (ADR 0047), `--curve gt|pbr-neutral|agx|aces` for the tone curve (ADR 0048), `--ddc <dir>`
//! for the derived data cache, `test-assets/.ddc` by default), `--lods` for mesh detail
//! levels made at load (ADR 0053), `--pack <file>` to load a cooked pack instead of the glTF
//! scene (ADR 0056; `ofeliaengine cook` makes one), `--stream` with it to stream the
//! textures while drawing (ADR 0057: tails first, then larger levels, through the texture
//! pool's staging ring), `--async-compute on|off` to run the async passes (ambient occlusion)
//! on the async compute queue or not (ADR 0065; default: the vendor's measured default or
//! `ENGINE_ASYNC_COMPUTE`). The last column is the GPU frame time end to end (the graph's
//! first pass to its last on the graphics queue), which async compute shortens while the
//! overlapped passes themselves may read longer.

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_assets::cook::TextureRecord;
use engine_assets::pack::{PackStreamer, StreamThread, Streamed};
use engine_assets::{
    AlphaMode as GltfAlpha, CookSettings, CookedScene, DerivedDataCache, TextureEntry,
    TextureFormat as Stored, TextureSlot, TextureWrap, cook_scene, import_gltf, load_pack,
    open_scene,
};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};
use engine_render::ao::{AoQuality, AoSettings};
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, LocalLight, ShadowSettings,
    Spot, Temporal,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialId, MaterialTable, TextureRef};
use engine_render::mesh::{GeometryCapacity, GeometryPool, LodIndices, MeshData, MeshId};
use engine_render::png::{PngFormat, encode_png};
use engine_render::post::{Composite, HDR_FORMAT, ToneCurve, Tonemap};
use engine_render::postfx::{AutoExposure, Bloom, BloomSettings, ExposureSettings};
use engine_render::texture_pool::StagingSpan;
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation, Wrap,
};
use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WARMUP: usize = 30;
const FRAMES: usize = 200;
/// Texture bytes uploaded per frame: the whole scene within a few frames.
const UPLOAD_BUDGET: u64 = 256 << 20;

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn stored_format(format: Stored) -> TextureFormat {
    match format {
        Stored::Rgba8Srgb => TextureFormat::Rgba8Srgb,
        Stored::Rgba8Unorm => TextureFormat::Rgba8Unorm,
        Stored::Bc7Srgb => TextureFormat::Bc7Srgb,
        Stored::Bc7Unorm => TextureFormat::Bc7Unorm,
        Stored::Bc5Unorm => TextureFormat::Bc5Unorm,
    }
}

/// Textures streamed from a pack into the pool while frames are drawn (ADR 0057).
struct Streaming {
    streamer: StreamThread,
    records: Vec<TextureRecord>,
    queue: VecDeque<TextureEntry>,
    /// Requests by tag: the span being filled and what goes into it.
    requests: Vec<Option<(StagingSpan, TextureEntry)>>,
    finished: Vec<Streamed>,
    tails_left: usize,
    bytes: u64,
    /// When streaming began (the first frame).
    began: Option<(Instant, u64)>,
    /// Time each pump took on the frame's thread (while streaming), ms.
    pump_ms: Vec<f64>,
    /// When the scene was whole at low resolution, and at full: (ms and frames since
    /// streaming began).
    tails_done: Option<(f64, u64)>,
    all_done: Option<(f64, u64)>,
}

impl Streaming {
    /// Requests what fits (streaming thread, staging ring), then uploads what arrived.
    fn pump(
        &mut self,
        pool: &mut TexturePool,
        allocator: &mut Allocator,
        ids: &[Option<TextureId>],
        frame_number: u64,
    ) {
        self.began.get_or_insert((Instant::now(), frame_number));
        if !self.busy() {
            return;
        }
        let pump_start = Instant::now();
        while self.streamer.has_room() {
            let Some(&te) = self.queue.front() else {
                break;
            };
            let entry = self.streamer.entries()[te.entry as usize];
            let bytes = PackStreamer::target_bytes(&entry) as u64;
            let Some(mut span) = pool.staging(allocator, bytes).unwrap() else {
                break;
            };
            let tag = self.requests.len() as u64;
            // SAFETY: the span is lent to this request alone and stays alive until it is
            // handed to the pool when the entry has arrived.
            unsafe {
                self.streamer
                    .request(te.entry, span.as_mut_ptr(), span.len() as usize, tag)
                    .unwrap();
            }
            self.bytes += bytes;
            self.requests.push(Some((span, te)));
            self.queue.pop_front();
        }
        self.streamer.poll(&mut self.finished);
        for done in self.finished.drain(..) {
            let Some((span, te)) = self.requests[done.tag as usize].take() else {
                continue;
            };
            done.result.unwrap();
            if te.first_level + te.level_count == self.records[te.texture as usize].levels {
                self.tails_left -= 1;
            }
            let Some(id) = ids[te.texture as usize] else {
                pool.cancel_staging(span);
                continue;
            };
            pool.upload_levels(id, te.first_level, te.level_count, span)
                .unwrap();
        }
        self.pump_ms.push(pump_start.elapsed().as_secs_f64() * 1e3);
    }

    fn busy(&self) -> bool {
        !self.queue.is_empty() || self.streamer.pending() > 0
    }
}

fn wrap(w: TextureWrap) -> Wrap {
    match w {
        TextureWrap::Repeat => Wrap::Repeat,
        TextureWrap::MirroredRepeat => Wrap::MirroredRepeat,
        TextureWrap::ClampToEdge => Wrap::ClampToEdge,
    }
}

/// The scene's materials; `textured: false` keeps the factors only.
fn material_descs(
    scene: &CookedScene,
    ids: &[Option<TextureId>],
    textured: bool,
) -> Vec<MaterialDesc> {
    let find = |slot: Option<TextureSlot>| -> Option<TextureRef> {
        let slot = slot.filter(|_| textured)?;
        Some(TextureRef {
            texture: ids[slot.texture as usize]?,
            wrap: slot.wrap.map(wrap),
        })
    };
    scene
        .materials
        .iter()
        .map(|m| MaterialDesc {
            base_color: m.base_color,
            base_color_texture: find(m.base_color_texture),
            metallic: m.metallic,
            roughness: m.roughness,
            metallic_roughness_texture: find(m.metallic_roughness_texture),
            normal_texture: find(m.normal_texture),
            normal_scale: m.normal_scale,
            occlusion_texture: find(m.occlusion_texture),
            occlusion_strength: m.occlusion_strength,
            emissive: m.emissive,
            emissive_texture: find(m.emissive_texture),
            alpha: match m.alpha {
                GltfAlpha::Mask(bits) => AlphaMode::Mask {
                    cutoff: f32::from_bits(bits),
                },
                GltfAlpha::Opaque => AlphaMode::Opaque,
                GltfAlpha::Blend => AlphaMode::Blend,
            },
            double_sided: m.double_sided,
            subsurface: None,
        })
        .collect()
}

/// A camera from `--view ex,ey,ez,tx,ty,tz`, or an interior view down the longest
/// horizontal axis from a quarter of its length, a little above the floor, or an object
/// view from the front.
fn view(min: Vec3, max: Vec3, requested: Option<[f32; 6]>) -> (Vec3, Vec3) {
    if let Some(v) = requested {
        return (Vec3::new(v[0], v[1], v[2]), Vec3::new(v[3], v[4], v[5]));
    }
    let size = max - min;
    let center = (min + max) * 0.5;
    if size.x.max(size.z) > 1.5 * size.y {
        let (eye_y, target_y) = (min.y + size.y * 0.15, min.y + size.y * 0.25);
        if size.x >= size.z {
            (
                Vec3::new(min.x + size.x * 0.25, eye_y, center.z),
                Vec3::new(max.x, target_y, center.z),
            )
        } else {
            (
                Vec3::new(center.x, eye_y, min.z + size.z * 0.25),
                Vec3::new(center.x, target_y, max.z),
            )
        }
    } else {
        (center + Vec3::new(0.0, 0.0, size.length() * 0.9), center)
    }
}

fn main() {
    let mut path = String::from("test-assets/Sponza/Sponza.gltf");
    let (mut screenshot, mut requested_view) = (None, None);
    let mut uncompressed = false;
    let mut prepass = false;
    let mut motion = false;
    let mut shadows = ShadowSettings::default();
    let mut light_count = 0usize;
    let mut shadowed_lights = 0usize;
    let mut local_shadows = engine_render::forward::LocalShadowSettings::default();
    let mut local_shadows_every_frame = false;
    let mut contact: Option<engine_render::forward::ContactShadowSettings> = None;
    let mut light_culling = true;
    let (mut with_sky, mut sky_every_frame) = (false, false);
    let (mut atmosphere, mut atmosphere_every_frame, mut fog) = (false, false, false);
    let mut sun_elevation: Option<f32> = None;
    let mut lods = false;
    let mut pipeline_stats = false;
    let mut post = false;
    let mut curve = ToneCurve::Gt;
    let mut ddc = String::from("test-assets/.ddc");
    let mut pack: Option<String> = None;
    let mut ao: Option<AoSettings> = None;
    let mut stream = false;
    let mut async_compute: Option<bool> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--screenshot" => screenshot = args.next(),
            "--uncompressed" => uncompressed = true,
            "--prepass" => prepass = true,
            "--ao" => {
                let quality = match args.next().as_deref() {
                    Some("low") => AoQuality::Low,
                    Some("medium") => AoQuality::Medium,
                    Some("ultra") => AoQuality::Ultra,
                    Some(custom) if custom.contains('x') => {
                        let (slices, steps) = custom.split_once('x').unwrap();
                        AoQuality::Custom {
                            slices: slices.parse().unwrap(),
                            steps: steps.parse().unwrap(),
                        }
                    }
                    _ => AoQuality::High,
                };
                ao = Some(AoSettings {
                    quality,
                    ..AoSettings::default()
                });
            }
            "--motion" => motion = true,
            "--lights" => light_count = args.next().unwrap().parse().unwrap(),
            "--shadowed-lights" => shadowed_lights = args.next().unwrap().parse().unwrap(),
            "--local-shadow-budget" => {
                local_shadows.updates_per_frame = args.next().unwrap().parse().unwrap();
            }
            "--local-shadow-atlas" => {
                local_shadows.atlas_size = args.next().unwrap().parse().unwrap();
            }
            "--local-shadow-res" => {
                let value = args.next().unwrap();
                let (min, max) = value.split_once(',').unwrap();
                local_shadows.min_resolution = min.parse().unwrap();
                local_shadows.max_resolution = max.parse().unwrap();
            }
            "--local-shadows-every-frame" => local_shadows_every_frame = true,
            "--contact" => {
                let method = match args.next().as_deref() {
                    Some("march") => engine_render::forward::ContactMethod::March,
                    _ => engine_render::forward::ContactMethod::Bend,
                };
                contact = Some(engine_render::forward::ContactShadowSettings {
                    method,
                    ..Default::default()
                });
            }
            "--no-light-culling" => light_culling = false,
            "--sky" => with_sky = true,
            "--lods" => lods = true,
            "--pipeline-stats" => pipeline_stats = true,
            "--post" => post = true,
            "--curve" => {
                curve = match args.next().as_deref() {
                    Some("pbr-neutral") => ToneCurve::PbrNeutral,
                    Some("agx") => ToneCurve::Agx,
                    Some("aces") => ToneCurve::Aces,
                    _ => ToneCurve::Gt,
                }
            }
            "--sky-every-frame" => (with_sky, sky_every_frame) = (true, true),
            "--atmosphere" => (with_sky, atmosphere) = (true, true),
            "--atmosphere-every-frame" => {
                (with_sky, atmosphere, atmosphere_every_frame) = (true, true, true);
            }
            "--fog" => (with_sky, fog) = (true, true),
            "--sun" => sun_elevation = args.next().map(|v| v.parse().unwrap()),
            "--cascades" => shadows.cascades = args.next().unwrap().parse().unwrap(),
            "--shadow-res" => shadows.resolution = args.next().unwrap().parse().unwrap(),
            "--shadow-blend" => shadows.blend = args.next().unwrap().parse().unwrap(),
            "--ddc" => ddc = args.next().unwrap(),
            "--pack" => pack = args.next(),
            "--stream" => stream = true,
            "--async-compute" => async_compute = args.next().map(|v| v == "on"),
            "--view" => {
                let v: Vec<f32> = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .map(|x| x.trim().parse().unwrap())
                    .collect();
                requested_view = Some(<[f32; 6]>::try_from(v).unwrap());
            }
            _ => path = arg,
        }
    }
    // The scene, cooked: from a pack, or from the glTF file (textures through the derived
    // data cache, meshes given detail levels here).
    let jobs = ThreadPool::builder().build().unwrap();
    let start = Instant::now();
    let mut streaming_setup = None;
    let (mut cooked, source) = match &pack {
        Some(file) if stream => {
            let (pack, layout, load) = open_scene(Path::new(file), &jobs, 32).unwrap();
            let source = format!(
                "pack: scene and meshes {:.1} MB read through {} in {:.0} ms; {} texture entries streamed while drawing",
                load.read_bytes as f64 / 1e6,
                load.backend,
                load.seconds * 1e3,
                layout.texture_entries.len(),
            );
            streaming_setup = Some((pack, layout.textures, layout.texture_entries));
            (layout.scene, source)
        }
        Some(file) => {
            let (scene, load) = load_pack(Path::new(file), &jobs, 32).unwrap();
            let source = format!(
                "pack {:.1} MB read ({:.1} MB unpacked; zstd and checks {:.0} ms of CPU on {} threads) through {} in {:.0} ms ({:.0} MB/s from the drive)",
                load.read_bytes as f64 / 1e6,
                load.bytes as f64 / 1e6,
                load.decode_seconds * 1e3,
                jobs.num_workers() + 1,
                load.backend,
                load.seconds * 1e3,
                load.read_bytes as f64 / 1e6 / load.seconds,
            );
            (scene, source)
        }
        None => {
            let bytes = std::fs::read(&path).unwrap_or_else(|e| {
                panic!("{path}: {e} (download test scenes with tools/fetch-test-assets.ps1)")
            });
            let dir = std::path::Path::new(&path).parent();
            let scene = import_gltf(&bytes, dir).unwrap();
            let imported = start.elapsed();
            let cache = DerivedDataCache::new(&ddc).unwrap();
            let settings = CookSettings {
                lods,
                compression_level: 0,
                block_compression: !uncompressed,
                ..CookSettings::default()
            };
            let (cooked, stats) = cook_scene(&scene, dir, Some(&cache), &jobs, &settings).unwrap();
            let source = format!(
                "glTF {:.0} ms; {} textures {} in {:.0} ms on {} threads ({} from the cache); meshes{} {:.0} ms",
                imported.as_secs_f64() * 1e3,
                cooked.textures.len(),
                if uncompressed {
                    "read, decoded and mipmapped"
                } else {
                    "read, decoded, mipmapped and compressed"
                },
                stats.texture_seconds * 1e3,
                jobs.num_workers() + 1,
                stats.cache_hits,
                if lods { " with detail levels" } else { "" },
                stats.mesh_seconds * 1e3,
            );
            (cooked, source)
        }
    };
    let loaded = start.elapsed();
    let texels: u64 = match &streaming_setup {
        Some((_, records, _)) => records
            .iter()
            .map(|t| u64::from(t.width) * u64::from(t.height))
            .sum(),
        None => cooked
            .textures
            .iter()
            .map(|t| u64::from(t.width) * u64::from(t.height))
            .sum(),
    };

    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("scene_bench"),
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
    let mut renderer =
        ForwardRenderer::new(&device, &bindless, HDR_FORMAT, depth_format, shadows).unwrap();
    let screenshot_format = vk::Format::R8G8B8A8_SRGB;
    let tonemap = Tonemap::new(&device, &bindless, screenshot_format).unwrap();
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 64 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    if let Some(on) = async_compute {
        graph.set_async_compute(on);
    }
    let readback_size = u64::from(WIDTH * HEIGHT * 4);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(readback_size)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
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

    // Meshes, one per primitive.
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let start = Instant::now();
    let meshes: Vec<MeshId> = cooked
        .meshes
        .iter()
        .map(|m| {
            let levels: Vec<LodIndices<'_>> = m
                .lods
                .iter()
                .map(|l| LodIndices {
                    indices: &l.indices,
                    error: l.error,
                })
                .collect();
            geometry
                .add_mesh_with_lods(
                    &MeshData {
                        positions: &m.positions,
                        normals: &m.normals,
                        tangents: &m.tangents,
                        uvs: &m.uvs,
                        indices: &m.indices,
                    },
                    &levels,
                )
                .unwrap()
        })
        .collect();
    let meshes_added = start.elapsed();
    let lod_triangles: usize = cooked
        .meshes
        .iter()
        .flat_map(|m| &m.lods)
        .map(|l| l.indices.len() / 3)
        .sum();
    if lod_triangles > 0 {
        println!("Detail levels: {lod_triangles} triangles in coarser levels");
    }
    let (vertices, _, indices, _) = geometry.usage();

    // Textures and materials.
    let mut textures = TexturePool::new(&device, UPLOAD_BUDGET, 1);
    let mut streaming = None;
    let (texture_ids, texture_bytes): (Vec<Option<TextureId>>, u64) = match streaming_setup {
        Some((pack, records, entries)) => {
            let ids = records
                .iter()
                .map(|r| {
                    let format = Stored::from_code(r.format).map(stored_format)?;
                    textures
                        .add_empty(&mut allocator, r.width, r.height, format, r.levels)
                        .ok()
                })
                .collect();
            let bytes = records
                .iter()
                .filter_map(|r| {
                    let format = stored_format(Stored::from_code(r.format)?);
                    Some(
                        (0..r.levels)
                            .map(|k| {
                                format.level_bytes((r.width >> k).max(1), (r.height >> k).max(1))
                            })
                            .sum::<u64>(),
                    )
                })
                .sum();
            let tails_left = records.len();
            streaming = Some(Streaming {
                // Reads, decompression and checks on a thread of their own (below-normal
                // priority), with half the cores for decompression.
                streamer: StreamThread::spawn(
                    pack,
                    32,
                    64,
                    std::thread::available_parallelism().map_or(4, |n| n.get() / 2),
                )
                .unwrap(),
                records,
                queue: entries.into(),
                requests: Vec::new(),
                finished: Vec::new(),
                tails_left,
                bytes: 0,
                began: None,
                pump_ms: Vec::new(),
                tails_done: None,
                all_done: None,
            });
            (ids, bytes)
        }
        None => {
            let ids = std::mem::take(&mut cooked.textures)
                .into_iter()
                .map(|t| {
                    let levels = TextureLevels {
                        width: t.width,
                        height: t.height,
                        format: stored_format(t.format),
                        levels: t.levels,
                    };
                    textures.add(&device, &mut allocator, levels).ok()
                })
                .collect();
            (ids, textures.pending().1)
        }
    };
    let mut materials = MaterialTable::new(1);
    let textured = material_descs(&cooked, &texture_ids, true);
    let untextured = material_descs(&cooked, &texture_ids, false);
    let material_ids: Vec<MaterialId> = textured.iter().map(|&m| materials.add(m)).collect();
    let fallback = materials.default_material();

    // Draws sorted by mesh, so instances of a primitive share a batch.
    let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let mut draws: Vec<DrawItem> = Vec::new();
    for instance in &cooked.instances {
        let world = Mat4::from_cols_array(&instance.world_from_object);
        let mesh = &cooked.meshes[instance.mesh as usize];
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
    let (eye, target) = view(min, max, requested_view);
    // Lights spread over the scene's box (a fixed sequence): every fourth a spot pointing
    // down, ranges a few percent of the scene's size.
    let size_of_scene = (max - min).length();
    let mut seed = 0x9E37_79B9u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed >> 8) as f32 / (1 << 24) as f32
    };
    let light_count = light_count.max(shadowed_lights);
    let lights: Vec<LocalLight> = (0..light_count)
        .map(|k| {
            let t = Vec3::new(next(), next(), next());
            let range = size_of_scene * (0.03 + 0.05 * next());
            LocalLight {
                position: min + (max - min) * t,
                range,
                color: Vec3::new(0.3 + next(), 0.3 + next(), 0.3 + next()) * range * range * 0.5,
                spot: (k % 4 == 0).then_some(Spot {
                    direction: Vec3::NEG_Y,
                    inner: 0.4,
                    outer: 0.7,
                }),
                shadow: k < shadowed_lights,
            }
        })
        .collect();
    let size = max - min;
    let camera = Camera {
        clip_from_world: vulkan_perspective(
            60f32.to_radians(),
            WIDTH as f32 / HEIGHT as f32,
            size.length() * 0.001,
            size.length() * 2.0,
        ) * look_at(eye, target, Vec3::Y),
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.2),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.08),
    };

    let masked = cooked
        .materials
        .iter()
        .filter(|m| matches!(m.alpha, GltfAlpha::Mask(_)))
        .count();
    let blended = cooked
        .materials
        .iter()
        .filter(|m| m.alpha == GltfAlpha::Blend)
        .count();
    let double_sided = cooked.materials.iter().filter(|m| m.double_sided).count();
    println!(
        "### Scene through the forward renderer ({WIDTH}×{HEIGHT} HDR, {} shadow cascades of {}²)",
        shadows.cascade_count(),
        shadows.resolution
    );
    println!();
    println!("Device: {}", device.adapter().summary());
    println!(
        "Async compute: {} (queue {:?})",
        if graph.async_compute() { "on" } else { "off" },
        device.async_compute.map(|q| q.family)
    );
    println!(
        "Scene: {}: {} triangles in {} primitives, {} draws; {} materials ({masked} masked, \
         {blended} blended, {double_sided} double-sided)",
        pack.as_deref().unwrap_or(&path),
        cooked
            .meshes
            .iter()
            .map(|m| m.indices.len() / 3)
            .sum::<usize>(),
        cooked.meshes.len(),
        draws.len(),
        cooked.materials.len(),
    );
    println!(
        "Load: {source}; {:.1} Mtexels; {:.0} ms in all; meshes into the pool {:.0} ms",
        texels as f64 / 1e6,
        loaded.as_secs_f64() * 1e3,
        meshes_added.as_secs_f64() * 1e3,
    );
    println!(
        "Memory: geometry {:.1} MB ({vertices} vertices), textures {:.1} MB ({} with mips)",
        (vertices * 28 + indices * 4) as f64 / (1 << 20) as f64,
        texture_bytes as f64 / (1 << 20) as f64,
        if uncompressed { "RGBA8" } else { "BC7 and BC5" },
    );
    println!();
    renderer.set_depth_prepass(prepass);
    renderer
        .set_ambient_occlusion(&device, &bindless, ao)
        .unwrap();
    renderer.set_light_culling(light_culling);
    if shadowed_lights > 0 {
        renderer
            .set_local_shadows(&device, &mut allocator, &bindless, Some(local_shadows))
            .unwrap();
    }
    renderer
        .set_contact_shadows(&device, &bindless, contact)
        .unwrap();
    if pipeline_stats {
        // Needs ENGINE_PIPELINE_STATS=1 (the device captures statistics at creation).
        let names = [
            "plain",
            "local lights",
            "environment",
            "lights + environment",
        ];
        for (name, stats) in names.iter().zip(renderer.shading_statistics(&device)) {
            let line: Vec<String> = stats
                .iter()
                .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
                .collect();
            println!("Pipeline statistics, {name}: {}", line.join("; "));
        }
        let names = [
            "atmosphere pass",
            "transparent under atmosphere",
            "transparent under atmosphere + lights",
        ];
        for (name, stats) in names.iter().zip(renderer.atmosphere_statistics(&device)) {
            let line: Vec<String> = stats
                .iter()
                .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
                .collect();
            println!("Pipeline statistics, {name}: {}", line.join("; "));
        }
        let names = [
            "contact shadows",
            "contact shadows + lights",
            "contact shadows + environment",
            "contact shadows + lights + environment",
        ];
        for (name, stats) in names.iter().zip(renderer.contact_statistics(&device)) {
            if stats.is_empty() {
                continue;
            }
            let line: Vec<String> = stats
                .iter()
                .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
                .collect();
            println!("Pipeline statistics, {name}: {}", line.join("; "));
        }
        let names = ["lights with shadows", "lights with shadows + environment"];
        for (name, stats) in names.iter().zip(renderer.shadowed_statistics(&device)) {
            if stats.is_empty() {
                continue;
            }
            let line: Vec<String> = stats
                .iter()
                .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
                .collect();
            println!("Pipeline statistics, {name}: {}", line.join("; "));
        }
        println!();
    }
    let post_effects = post.then(|| {
        (
            Bloom::new(&device, &bindless, BloomSettings::default()).unwrap(),
            AutoExposure::new(
                &device,
                &mut allocator,
                &bindless,
                ExposureSettings::default(),
            )
            .unwrap(),
        )
    });
    let mut environment = with_sky.then(|| {
        let mut environment =
            Environment::new(&device, &mut allocator, &bindless, Sky::default()).unwrap();
        if atmosphere {
            environment
                .set_atmosphere(
                    &device,
                    &mut allocator,
                    &bindless,
                    Some(Atmosphere::earth()),
                )
                .unwrap();
        }
        if fog {
            environment.set_fog(Some(HeightFog::default()));
        }
        environment
    });
    let light = match sun_elevation {
        Some(degrees) => {
            let e = degrees.to_radians();
            let horizontal =
                Vec3::new(light.direction.x, 0.0, light.direction.z).normalize_or(Vec3::X);
            DirectionalLight {
                direction: -(-horizontal * e.cos() + Vec3::Y * e.sin()),
                ..light
            }
        }
        None => light,
    };
    // Motion vectors: the forward pass writes them, a pass fills the background with the
    // camera's motion, and the instances are copied for the next frame.
    let temporal = motion.then(|| {
        let temporal = Temporal::new(&device, &mut allocator, draws.len() as u32).unwrap();
        temporal.set_jitter_phases(8);
        temporal
    });
    if light_count > 0 {
        println!(
            "Lights: {light_count} ({} culling), {shadowed_lights} with shadows{}",
            if light_culling { "with" } else { "without" },
            if local_shadows_every_frame {
                " (drawn again every frame)"
            } else {
                ""
            }
        );
        println!();
    }
    println!(
        "| materials | upload frames | GPU shadow µs | GPU depth prepass µs | GPU forward µs | \
         GPU motion background + remember instances µs | GPU light tiles µs | CPU add_passes µs | \
         GPU sky µs | GPU environment rebuild µs | GPU bloom µs | GPU exposure µs | GPU tonemap µs | GPU AO µs | \
         GPU frame µs | CPU execute µs | GPU atmosphere tables µs | GPU aerial perspective µs | \
         GPU atmosphere pass µs | GPU local shadows µs | local shadow views | GPU contact shadows µs |"
    );
    println!(
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    );

    let mut frame_number = 0u64;
    for (mode, descs) in [("textured", &textured), ("factors only", &untextured)] {
        for (&id, &desc) in material_ids.iter().zip(descs.iter()) {
            materials.set(id, desc);
        }
        let (mut shadow, mut depth_prepass, mut forward) = (Vec::new(), Vec::new(), Vec::new());
        let mut ao_time = Vec::new();
        let mut frame_time = Vec::new();
        let mut motion_extra = Vec::new();
        let (mut light_tiles, mut cpu_add) = (Vec::new(), Vec::new());
        let mut cpu_execute = Vec::new();
        let (mut local_time, mut local_views) = (Vec::new(), Vec::new());
        let mut contact_time = Vec::new();
        let (mut sky_pass, mut rebuild) = (Vec::new(), Vec::new());
        let (mut tables, mut aerial, mut composite) = (Vec::new(), Vec::new(), Vec::new());
        let (mut bloom_time, mut exposure_time, mut tonemap_time) =
            (Vec::new(), Vec::new(), Vec::new());
        let mut upload_frames = 0;
        let last = mode == "textured" && screenshot.is_some();
        let mut frame = 0;
        while frame < WARMUP + FRAMES + usize::from(last) {
            if let Some(s) = streaming.as_mut() {
                s.pump(
                    &mut textures,
                    &mut allocator,
                    &texture_ids,
                    frame_number + 1,
                );
            }
            let uploading =
                textures.pending().0 > 0 || streaming.as_ref().is_some_and(Streaming::busy);
            upload_frames += usize::from(uploading);
            let capture = last && frame == WARMUP + FRAMES;
            // SAFETY: the previous frame finished (waited below).
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
            let added;
            let executed;
            {
                // SAFETY: nothing is in flight.
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                frame_number += 1;
                textures
                    .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                    .unwrap();
                let mats = materials
                    .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                    .unwrap();
                let geo = geometry
                    .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                    .unwrap();
                let hdr = fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                let depth =
                    fg.create_texture("depth", TextureDesc::new_2d(depth_format, WIDTH, HEIGHT));
                if let Some(environment) = &environment
                    && sky_every_frame
                {
                    // A barely different sky every frame: rebuilt every frame.
                    let mut sky = Sky::default();
                    sky.zenith.x += (frame_number % 2) as f32 * 1e-3;
                    environment.set_sky(sky);
                }
                if let Some(environment) = &mut environment
                    && atmosphere_every_frame
                {
                    // A barely different atmosphere every frame: every table rebuilt.
                    let mut a = Atmosphere::earth();
                    a.mie_anisotropy += (frame_number % 2) as f32 * 1e-4;
                    environment
                        .set_atmosphere(&device, &mut allocator, &bindless, Some(a))
                        .unwrap();
                }
                if local_shadows_every_frame {
                    renderer.invalidate_local_shadows();
                }
                let started = Instant::now();
                renderer
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
                            color: hdr,
                            depth,
                            clear: Some([0.02, 0.02, 0.05, 1.0]),
                            occlusion: None,
                            temporal: temporal.as_ref(),
                            lights: &lights,
                            environment: environment.as_ref(),
                        },
                    )
                    .unwrap();
                added = started.elapsed();
                if capture {
                    let ldr = fg.create_texture(
                        "screenshot",
                        TextureDesc::new_2d(screenshot_format, WIDTH, HEIGHT),
                    );
                    let (bloomed, measured) = match &post_effects {
                        Some((bloom, exposure)) => (
                            Some(bloom.add_passes(&mut fg, &bindless, hdr)),
                            Some(exposure.add_passes(&mut fg, &bindless, hdr, 1.0 / 60.0)),
                        ),
                        None => (None, None),
                    };
                    tonemap.add_composite_pass(
                        &mut fg,
                        &bindless,
                        hdr,
                        ldr,
                        Composite {
                            curve,
                            bloom: bloomed,
                            auto_exposure: measured,
                            ..Composite::default()
                        },
                    );
                    let out = fg.import_buffer(
                        "readback",
                        ImportedBuffer {
                            buffer: readback,
                            size: readback_size,
                            address: 0,
                            initial: BufferState::idle(),
                            final_state: Some(BufferState::host_read()),
                        },
                    );
                    fg.add_pass("read back")
                        .read(ldr, Access::TransferRead)
                        .write(out, Access::TransferWrite)
                        .execute(move |ctx| {
                            let region = vk::BufferImageCopy {
                                image_subresource: vk::ImageSubresourceLayers {
                                    aspect_mask: vk::ImageAspectFlags::COLOR,
                                    mip_level: 0,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                },
                                image_extent: vk::Extent3D {
                                    width: WIDTH,
                                    height: HEIGHT,
                                    depth: 1,
                                },
                                ..Default::default()
                            };
                            // SAFETY: recording; layouts arranged by the graph.
                            unsafe {
                                ctx.device.cmd_copy_image_to_buffer(
                                    ctx.cmd,
                                    ctx.image(ldr),
                                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                    ctx.buffer(out),
                                    &[region],
                                );
                            }
                        });
                } else if let Some((bloom, exposure)) = &post_effects {
                    let ldr = fg.create_texture(
                        "tonemapped",
                        TextureDesc::new_2d(screenshot_format, WIDTH, HEIGHT),
                    );
                    let bloomed = bloom.add_passes(&mut fg, &bindless, hdr);
                    let measured = exposure.add_passes(&mut fg, &bindless, hdr, 1.0 / 60.0);
                    tonemap.add_composite_pass(
                        &mut fg,
                        &bindless,
                        hdr,
                        ldr,
                        Composite {
                            bloom: Some(bloomed),
                            auto_exposure: Some(measured),
                            curve,
                            ..Composite::default()
                        },
                    );
                    fg.add_pass("consume")
                        .read(ldr, Access::TransferRead)
                        .side_effects()
                        .execute(|_| {});
                } else {
                    fg.add_pass("consume")
                        .read(hdr, Access::TransferRead)
                        .side_effects()
                        .execute(|_| {});
                }
                let started = Instant::now();
                fg.execute_with_bindless(&mut allocator, Some(&bindless))
                    .unwrap();
                executed = started.elapsed();
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
            if let Some(s) = streaming.as_mut() {
                let at = s.began.map_or((0.0, 0), |(t, f)| {
                    (t.elapsed().as_secs_f64() * 1e3, frame_number + 1 - f)
                });
                let uploaded = textures.pending().0 == 0;
                if s.tails_left == 0 && uploaded && s.tails_done.is_none() {
                    s.tails_done = Some(at);
                }
                if !s.busy() && uploaded && s.all_done.is_none() {
                    s.all_done = Some(at);
                }
            }
            // Measure once every texture is resident.
            if uploading {
                continue;
            }
            if frame > WARMUP && !capture {
                let mut extra = 0.0;
                let mut shadow_frame = 0.0;
                let mut tiles_frame = 0.0;
                let mut ao_frame = 0.0;
                let (mut sky_frame, mut rebuild_frame) = (0.0, 0.0);
                let (mut tables_frame, mut aerial_frame, mut composite_frame) = (0.0, 0.0, 0.0);
                let (mut bloom_frame, mut exposure_frame, mut tonemap_frame) = (0.0, 0.0, 0.0);
                let mut local_frame = 0.0;
                let mut contact_frame = 0.0;
                for time in graph.gpu_pass_times() {
                    match time.name {
                        "contact shadows" | "contact shadows clear" => {
                            contact_frame += f64::from(time.gpu_us);
                        }
                        "local shadow cull" | "local shadows" => {
                            local_frame += f64::from(time.gpu_us);
                        }
                        "shadow cull" | "shadow" => shadow_frame += f64::from(time.gpu_us),
                        "forward" => forward.push(f64::from(time.gpu_us)),
                        "depth prepass" => depth_prepass.push(f64::from(time.gpu_us)),
                        "ao depths" | "ao" | "ao denoise" => ao_frame += f64::from(time.gpu_us),
                        "motion background" | "remember instances" => {
                            extra += f64::from(time.gpu_us);
                        }
                        "light tiles clear" | "light tiles" => {
                            tiles_frame += f64::from(time.gpu_us);
                        }
                        "sky" => sky_frame += f64::from(time.gpu_us),
                        "bloom downsample" | "bloom upsample" => {
                            bloom_frame += f64::from(time.gpu_us);
                        }
                        "exposure histogram clear" | "exposure histogram" | "exposure average" => {
                            exposure_frame += f64::from(time.gpu_us);
                        }
                        "tonemap" => tonemap_frame += f64::from(time.gpu_us),
                        "environment sky" | "environment prefilter" | "environment irradiance" => {
                            rebuild_frame += f64::from(time.gpu_us);
                        }
                        "atmosphere transmittance"
                        | "atmosphere multiple scattering"
                        | "atmosphere sky view"
                        | "atmosphere sky cube" => tables_frame += f64::from(time.gpu_us),
                        "atmosphere aerial perspective" => aerial_frame += f64::from(time.gpu_us),
                        "atmosphere" => composite_frame += f64::from(time.gpu_us),
                        _ => {}
                    }
                }
                motion_extra.push(extra);
                shadow.push(shadow_frame);
                light_tiles.push(tiles_frame);
                cpu_add.push(added.as_secs_f64() * 1e6);
                cpu_execute.push(executed.as_secs_f64() * 1e6);
                sky_pass.push(sky_frame);
                rebuild.push(rebuild_frame);
                tables.push(tables_frame);
                aerial.push(aerial_frame);
                composite.push(composite_frame);
                bloom_time.push(bloom_frame);
                exposure_time.push(exposure_frame);
                tonemap_time.push(tonemap_frame);
                ao_time.push(ao_frame);
                local_time.push(local_frame);
                contact_time.push(contact_frame);
                local_views.push(f64::from(renderer.local_shadow_stats().views_drawn));
                if let Some(us) = graph.gpu_frame_time() {
                    frame_time.push(f64::from(us));
                }
            }
            if capture && let Some(file) = &screenshot {
                let ptr = readback_mem.mapped_ptr().unwrap();
                // SAFETY: the GPU is idle; the mapping covers the image.
                let pixels =
                    unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
                let png = encode_png(WIDTH, HEIGHT, PngFormat::Rgba, pixels).unwrap();
                std::fs::write(file, png).unwrap();
            }
            frame += 1;
        }
        println!(
            "| {mode} | {upload_frames} | {:.0} | {:.0} | {:.0} | {:.0} | {:.1} | {:.0} | {:.1} | {:.0} | {:.1} | {:.1} | {:.1} | {:.0} | {:.0} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} | {:.1} |",
            median(&mut shadow),
            median(&mut depth_prepass),
            median(&mut forward),
            median(&mut motion_extra),
            median(&mut light_tiles),
            median(&mut cpu_add),
            median(&mut sky_pass),
            median(&mut rebuild),
            median(&mut bloom_time),
            median(&mut exposure_time),
            median(&mut tonemap_time),
            median(&mut ao_time),
            median(&mut frame_time),
            median(&mut cpu_execute),
            median(&mut tables),
            median(&mut aerial),
            median(&mut composite),
            median(&mut local_time),
            median(&mut local_views),
            median(&mut contact_time),
        );
    }
    println!();
    if let Some(s) = &streaming {
        let show = |at: Option<(f64, u64)>| {
            at.map_or(String::from("never"), |(ms, frame)| {
                format!("{ms:.0} ms ({frame} frames)")
            })
        };
        let (sum, max) = s
            .pump_ms
            .iter()
            .fold((0.0, 0.0f64), |(sum, max), &ms| (sum + ms, max.max(ms)));
        println!(
            "Streaming, from the first frame: every texture at low resolution after {}, every level after {}; {:.1} MB through the staging ring; on the frame's thread {:.2} ms per frame on average, {:.2} at most ({} frames)",
            show(s.tails_done),
            show(s.all_done),
            s.bytes as f64 / 1e6,
            sum / s.pump_ms.len().max(1) as f64,
            max,
            s.pump_ms.len(),
        );
    }
    if let Some(file) = &screenshot {
        println!("Screenshot: {file} (eye {eye}, target {target})");
    }

    if shadowed_lights > 0 {
        println!(
            "Local shadows, last frame: {:?}",
            renderer.local_shadow_stats()
        );
    }
    // SAFETY: the device is idle.
    unsafe {
        renderer.destroy(&device, &mut allocator, &bindless);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        if let Some((_, mut exposure)) = post_effects {
            exposure.destroy(&mut allocator);
        }
        if let Some(mut environment) = environment {
            environment.destroy(&mut allocator, &bindless);
        }
        if let Some(mut temporal) = temporal {
            temporal.destroy(&mut allocator);
        }
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(command_pool, None);
    }
}
