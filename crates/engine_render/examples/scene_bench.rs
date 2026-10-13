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
//! every shadow again every frame (the cache dropped), `--contact` for the sun's contact
//! shadows (ADR 0071; implies the prepass), `--decals <count>` for decals spread over the
//! scene's floor (ADR 0082; implies the prepass: boxes projecting down, a blot texture,
//! every other one with a normal map) and `--decal-size <fraction>` for their side as a
//! fraction of the scene's size (default 3-8 %), `--rays [finest|coarsest]` for the scene's
//! acceleration structures (ADR 0087: the draws in a GPU scene, bottom levels at that
//! detail, the top level refitted or rebuilt every frame: an instance is nudged) and a ray
//! through every pixel from the camera (their own table; with `--screenshot`,
//! `<name>-rays.png` of their distances), `--ray-budget <triangles|device>` for the
//! bottom levels' build budget per frame (`device`: `RaySettings::for_device`), `--gi
//! [spacing]` for global illumination from a volume of probes about `spacing` apart (ADR
//! 0088, default 1; implies `--rays coarsest`: the draws then render from the GPU scene;
//! the volume converges in the first frame, then the GI table; `--gi-scale <1|2|4>` for the
//! pixels per resolved sample, default `GiSettings::for_device`; `GI_DEBUG=1` shows its
//! irradiance; `--scene-draws` with `--rays` draws from the GPU scene as `--gi` does, the
//! comparison for its cost), `--sky` for
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
//! `ENGINE_ASYNC_COMPUTE`); `--probes <x,y,z>` for reflection probes over the scene's
//! bounding box cut into that many boxes (ADR 0073; implies `--sky`), `--probe-faces <1-6>`
//! for their capture budget per frame, `--probes-every-frame` to capture them again
//! continuously (the cost of a capture), `--ssr full|half` for screen-space reflections
//! (implies `--sky`); reflections' passes get a table of their own. The last column is the GPU frame time end to end (the graph's
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
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::ao::{AoQuality, AoSettings};
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, Decal, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, LocalLight,
    ShadowSettings, Spot, Temporal, ViewGi,
};
use engine_render::gi::{GiSettings, ProbeGi, ProbeGrid};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialId, MaterialTable, TextureRef};
use engine_render::mesh::{GeometryCapacity, GeometryPool, LodIndices, MeshData, MeshId};
use engine_render::png::{PngFormat, encode_png};
use engine_render::post::{Composite, HDR_FORMAT, ToneCurve, Tonemap};
use engine_render::postfx::{AutoExposure, Bloom, BloomSettings, ExposureSettings};
use engine_render::rays::{BlasDetail, RAY_MASK_ALL, RayScene, RaySettings};
use engine_render::reflections::{
    ProbeSettings, ReflectionProbe, ReflectionProbes, Reflections, ScreenSpaceReflections,
    SsrSettings,
};
use engine_render::texture::GpuTexture;
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
    let mut decal_count = 0usize;
    let mut decal_size: Option<f32> = None;
    let mut ray_detail: Option<BlasDetail> = None;
    let mut ray_budget: Option<String> = None;
    let mut gi_spacing: Option<f32> = None;
    let mut gi_scale: Option<u32> = None;
    let mut scene_draws = false;
    let mut light_culling = true;
    let (mut with_sky, mut sky_every_frame) = (false, false);
    let (mut atmosphere, mut atmosphere_every_frame, mut fog) = (false, false, false);
    let mut sun_elevation: Option<f32> = None;
    let mut lods = false;
    let mut pipeline_stats = false;
    let mut msaa_stats: u32 = 0;
    let mut post = false;
    let mut curve = ToneCurve::Gt;
    let mut ddc = String::from("test-assets/.ddc");
    let mut pack: Option<String> = None;
    let mut ao: Option<AoSettings> = None;
    let mut stream = false;
    let mut async_compute: Option<bool> = None;
    let mut probe_grid: Option<[u32; 3]> = None;
    let mut probe_faces = 6u32;
    let mut probes_every_frame = false;
    let mut ssr: Option<bool> = None;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--screenshot" => screenshot = args.next(),
            "--uncompressed" => uncompressed = true,
            "--prepass" => prepass = true,
            "--ao" => {
                // An optional quality: the next argument unless it is another flag.
                let quality = match args.next_if(|a| !a.starts_with("--")).as_deref() {
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
                    bent_normals: ao.is_none_or(|a| a.bent_normals),
                    ..AoSettings::default()
                });
            }
            // Ambient occlusion without its bent normals (with or before `--ao`).
            "--no-bent" => {
                ao = Some(AoSettings {
                    bent_normals: false,
                    ..ao.unwrap_or_default()
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
            "--contact" => contact = Some(Default::default()),
            "--decals" => decal_count = args.next().unwrap().parse().unwrap(),
            "--decal-size" => decal_size = Some(args.next().unwrap().parse().unwrap()),
            "--rays" => {
                ray_detail = Some(match args.next_if(|a| !a.starts_with("--")).as_deref() {
                    Some("coarsest") => BlasDetail::Coarsest,
                    _ => BlasDetail::Finest,
                });
            }
            "--ray-budget" => ray_budget = args.next(),
            "--gi-scale" => gi_scale = Some(args.next().unwrap().parse().unwrap()),
            "--scene-draws" => scene_draws = true,
            "--gi" => {
                gi_spacing = Some(
                    args.next_if(|a| !a.starts_with("--"))
                        .map_or(1.0, |v| v.parse().unwrap()),
                );
                ray_detail.get_or_insert(BlasDetail::Coarsest);
            }
            "--no-light-culling" => light_culling = false,
            "--sky" => with_sky = true,
            "--lods" => lods = true,
            "--pipeline-stats" => pipeline_stats = true,
            // With --pipeline-stats: also the multisampled shading's (ADR 0075).
            "--msaa" => msaa_stats = args.next().unwrap().parse().unwrap(),
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
            "--probes" => {
                let v: Vec<u32> = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .map(|x| x.trim().parse().unwrap())
                    .collect();
                probe_grid = Some(<[u32; 3]>::try_from(v).unwrap());
                with_sky = true;
            }
            "--probe-faces" => probe_faces = args.next().unwrap().parse().unwrap(),
            "--probes-every-frame" => probes_every_frame = true,
            "--ssr" => {
                ssr = Some(args.next().as_deref() == Some("half"));
                with_sky = true;
            }
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
    // Decals spread over the scene's floor (the same sequence, ADR 0082): boxes projecting
    // down, a few percent of the scene's size, every other one with a normal map.
    let mut decal_textures: Vec<(GpuTexture, Vec<u8>)> = if decal_count > 0 {
        [
            (vk::Format::R8G8B8A8_SRGB, decal_blot()),
            (vk::Format::R8G8B8A8_UNORM, decal_bumps()),
        ]
        .into_iter()
        .map(|(format, pixels)| {
            let size = DECAL_TEXTURE;
            let texture =
                GpuTexture::new(&device, &mut allocator, &bindless, format, size, size).unwrap();
            (texture, pixels)
        })
        .collect()
    } else {
        Vec::new()
    };
    let decals: Vec<Decal> = (0..decal_count)
        .map(|k| {
            let side = match decal_size {
                Some(fraction) => size_of_scene * fraction,
                None => size_of_scene * (0.03 + 0.05 * next()),
            };
            // Floors are near the bottom of a scene's box (Sponza's 8 % up).
            let at = Vec3::new(
                min.x + (max.x - min.x) * next(),
                min.y + (max.y - min.y) * 0.08,
                min.z + (max.z - min.z) * next(),
            );
            let turn = Quat::from_rotation_y(next() * core::f32::consts::TAU);
            Decal {
                world_from_decal: Mat4::from_scale_rotation_translation(
                    Vec3::splat(side),
                    turn * Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2),
                    at,
                ),
                albedo: Some(decal_textures[0].0.sampled_index()),
                normal: (k % 2 == 0).then(|| decal_textures[1].0.sampled_index()),
                tint: [
                    0.3 + 0.7 * next(),
                    0.3 + 0.7 * next(),
                    0.3 + 0.7 * next(),
                    0.9,
                ],
                roughness: next(),
                surface: 0.5,
                ..Decal::default()
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
    // The scene for ray queries (ADR 0087): the draws as a GPU scene's instances, and a
    // buffer of camera-ray distances (persistent, so the pass that writes it is kept).
    let mut ray_bed = ray_detail
        .filter(|_| device.ray_query().is_some())
        .map(|detail| {
            let mut scene = GpuScene::new(
                &device,
                &bindless,
                draws.len().next_power_of_two() as u32,
                1,
            )
            .unwrap();
            let mut first = None;
            for d in &draws {
                let id = scene.insert(SceneInstance {
                    mesh: d.mesh,
                    material: d.material,
                    world_from_object: d.world_from_object,
                });
                first.get_or_insert((id, d.world_from_object));
            }
            let build_budget = match ray_budget.as_deref() {
                None => u32::MAX,
                Some("device") => RaySettings::for_device(&device).build_budget,
                Some(n) => n.parse().unwrap(),
            };
            let rays = RayScene::new(
                &device,
                &bindless,
                RaySettings {
                    detail,
                    build_budget,
                    ..RaySettings::default()
                },
                1,
            )
            .unwrap();
            let bytes = u64::from(WIDTH * HEIGHT) * 4;
            let (buffer, memory) = allocator
                .create_buffer(
                    &vk::BufferCreateInfo::default().size(bytes).usage(
                        vk::BufferUsageFlags::STORAGE_BUFFER
                            | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                            | vk::BufferUsageFlags::TRANSFER_SRC,
                    ),
                    MemoryLocation::Device,
                )
                .unwrap();
            // SAFETY: created with the device address usage.
            let address = unsafe {
                d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
            };
            let (readback, readback_memory) = allocator
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST),
                    MemoryLocation::Readback,
                )
                .unwrap();
            (
                scene,
                rays,
                buffer,
                Some(memory),
                address,
                readback,
                Some(readback_memory),
                first,
            )
        });
    if ray_detail.is_some() && ray_bed.is_none() {
        println!("Rays: this GPU has no ray queries");
    }
    // The volume of probes (ADR 0088): over the scene, converged in the first frame.
    let gi = gi_spacing.filter(|_| ray_bed.is_some()).map(|spacing| {
        let grid = ProbeGrid::covering(min, max, spacing, 16_384);
        let defaults = GiSettings::for_device(&device);
        let settings = GiSettings {
            updates_per_frame: 11,
            resolve_scale: gi_scale.unwrap_or(defaults.resolve_scale),
            ..defaults
        };
        ProbeGi::new(&device, &mut allocator, &bindless, grid, settings).unwrap()
    });
    renderer
        .set_global_illumination(&device, &bindless, gi.is_some())
        .unwrap();
    let mut gi_rows: Vec<String> = Vec::new();
    // GPU time of the volume's updates while it converges, per pass name; then of each
    // frame's window (ADR 0089), per pass name, every frame.
    let mut gi_update_us: Vec<(&'static str, f64)> = Vec::new();
    let mut gi_window_us: Vec<(&'static str, Vec<f64>)> = Vec::new();
    let mut ray_rows: Vec<String> = Vec::new();
    // The bottom levels' builds: GPU time in all, frames with builds, the most in one.
    let (mut blas_build_us, mut blas_build_frames, mut blas_build_max) = (0.0f64, 0, 0.0f64);
    renderer
        .set_reflections(&device, &bindless, probe_grid.is_some() || ssr.is_some())
        .unwrap();
    let mut probes = probe_grid.map(|grid| {
        let mut probes = ReflectionProbes::new(
            &device,
            &mut allocator,
            &bindless,
            ProbeSettings {
                capacity: grid.iter().product::<u32>().max(1),
                faces_per_frame: probe_faces,
                ..ProbeSettings::default()
            },
        )
        .unwrap();
        let g = Vec3::new(grid[0] as f32, grid[1] as f32, grid[2] as f32).max(Vec3::ONE);
        let cell = (max - min) / g;
        for z in 0..grid[2].max(1) {
            for y in 0..grid[1].max(1) {
                for x in 0..grid[0].max(1) {
                    let center = min + cell * (Vec3::new(x as f32, y as f32, z as f32) + 0.5);
                    probes
                        .add(ReflectionProbe::room(center, cell * 0.5))
                        .unwrap();
                }
            }
        }
        probes
    });
    let mut screen_space = ssr.map(|half_resolution| {
        ScreenSpaceReflections::new(
            &device,
            &mut allocator,
            &bindless,
            WIDTH,
            HEIGHT,
            SsrSettings {
                half_resolution,
                ..SsrSettings::default()
            },
        )
        .unwrap()
    });
    if shadowed_lights > 0 {
        renderer
            .set_local_shadows(&device, &mut allocator, &bindless, Some(local_shadows))
            .unwrap();
    }
    renderer
        .set_contact_shadows(&device, &bindless, contact)
        .unwrap();
    renderer
        .set_decals(&device, &bindless, decal_count > 0 || pipeline_stats)
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
        // With motion vectors (temporal antialiasing): the shading that writes them (no
        // prepass), and the shading after the prepass with ambient occlusion (with `--ao`:
        // the default settings; the prepass writes the motion then).
        let kinds = ["motion, no prepass", "after the prepass with AO"];
        for (name, per_kind) in names.iter().zip(renderer.motion_statistics(&device)) {
            for (kind, stats) in kinds.iter().zip(per_kind) {
                if stats.is_empty() {
                    continue;
                }
                let line: Vec<String> = stats
                    .iter()
                    .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
                    .collect();
                println!("Pipeline statistics, {name}, {kind}: {}", line.join("; "));
            }
        }
        // With the bent normals too (ADR 0058), with `--ao`.
        for (name, stats) in names.iter().zip(renderer.bent_statistics(&device)) {
            if stats.is_empty() {
                continue;
            }
            let line: Vec<String> = stats
                .iter()
                .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
                .collect();
            println!(
                "Pipeline statistics, {name}, after the prepass with AO and bent normals: {}",
                line.join("; ")
            );
        }
        // The probe-GI shading after the prepass (ADR 0088), with `--gi`.
        for (name, stats) in names.iter().zip(renderer.gi_statistics(&device)) {
            if stats.is_empty() {
                continue;
            }
            let line: Vec<String> = stats
                .iter()
                .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
                .collect();
            println!("Pipeline statistics, {name}, GI: {}", line.join("; "));
        }
        let prepass = ["depth prepass", "depth prepass with motion"];
        for (name, stats) in prepass.iter().zip(renderer.prepass_statistics(&device)) {
            let line: Vec<String> = stats
                .iter()
                .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
                .collect();
            println!("Pipeline statistics, {name}: {}", line.join("; "));
        }
        if msaa_stats > 1 {
            let samples = vk::SampleCountFlags::from_raw(msaa_stats);
            renderer.set_samples(&device, &bindless, samples).unwrap();
            let names = [
                "plain",
                "local lights",
                "environment",
                "lights + environment",
                "atmosphere pass per sample",
            ];
            for (name, stats) in names.iter().zip(renderer.msaa_statistics(&device, samples)) {
                let line: Vec<String> = stats
                    .iter()
                    .map(|s| format!("{} {}: {}", s.stage, s.name, s.value))
                    .collect();
                println!(
                    "Pipeline statistics, MSAA {msaa_stats}x, {name}: {}",
                    line.join("; ")
                );
            }
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
        let names = [
            "decals",
            "decals + lights",
            "decals + environment",
            "decals + lights + environment",
        ];
        for (name, stats) in names.iter().zip(renderer.decal_statistics(&device)) {
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
        let names = [
            "reflections shading",
            "reflections shading + lights",
            "reflections composite, probes",
            "reflections composite, SSR",
            "reflections composite, probes + SSR",
        ];
        for (name, stats) in names.iter().zip(renderer.reflection_statistics(&device)) {
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
         GPU atmosphere pass µs | GPU local shadows µs | local shadow views | GPU contact shadows µs | \
         GPU decal tiles µs | GPU decals µs |"
    );
    println!(
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    );

    let mut frame_number = 0u64;
    let mut reflection_rows: Vec<String> = Vec::new();
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
        let (mut decal_tiles_time, mut decal_time) = (Vec::new(), Vec::new());
        let (mut tlas_time, mut camera_rays_time) = (Vec::new(), Vec::new());
        let mut gi_resolve_time = Vec::new();
        let (mut sky_pass, mut rebuild) = (Vec::new(), Vec::new());
        let (mut tables, mut aerial, mut composite) = (Vec::new(), Vec::new(), Vec::new());
        let (mut bloom_time, mut exposure_time, mut tonemap_time) =
            (Vec::new(), Vec::new(), Vec::new());
        let mut upload_frames = 0;
        // Reflections (ADR 0073): probe captures (their views, copy, mips, prefilter),
        // SSR passes, the composite.
        let (mut probe_capture, mut prefilter) = (Vec::new(), Vec::new());
        let (mut ssr_depth, mut ssr_trace, mut ssr_spatial, mut ssr_temporal) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut reflect_composite = Vec::new();
        let mut faces_drawn = Vec::new();
        let last = mode == "textured" && screenshot.is_some();
        let mut frame = 0;
        // Whether the GI updates of the frame before were the convergence's: pass times
        // come back a frame late (when the graph's one slot comes around again).
        let mut gi_converged_before = gi.as_ref().is_none_or(ProbeGi::is_converged);
        while frame < WARMUP + FRAMES + usize::from(last) {
            let gi_converging = !gi_converged_before;
            gi_converged_before = gi.as_ref().is_none_or(ProbeGi::is_converged);
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
            // Pipelines derived from the engine's (ambient occlusion's bent normals) that
            // the last frames asked for.
            renderer.update_material_types(&device);
            if frame == WARMUP {
                // Shading variants no frame drew with are made on the device's warm-up
                // thread (ADR 0079): the timed frames do not share the CPU with it.
                device.wait_for_pipeline_warm_up();
            }
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
                if frame_number == 1 {
                    for (texture, pixels) in &decal_textures {
                        texture.upload(&mut fg, &mut ring, pixels).unwrap();
                    }
                }
                let mut ray_frames = None;
                if let Some((scene, rays, buffer, _, address, readback, _, first)) =
                    ray_bed.as_mut()
                {
                    // One instance nudged every frame: the top level is rebuilt every frame
                    // (a still scene keeps it), as in a game with anything moving.
                    if let Some((id, world)) = *first {
                        let nudge = (frame_number % 2) as f32 * 1e-3;
                        scene.set_transform(
                            id,
                            &(Mat4::from_translation(Vec3::new(nudge, 0.0, 0.0)) * world),
                        );
                    }
                    let scene_frame = scene
                        .add_passes(
                            &mut fg,
                            &device,
                            &mut allocator,
                            &mut ring,
                            &bindless,
                            &geo,
                            &mats,
                            frame_number,
                        )
                        .unwrap()
                        .unwrap();
                    let ray_frame = rays
                        .add_passes(
                            &mut fg,
                            &device,
                            &mut allocator,
                            &mut ring,
                            &bindless,
                            &geo,
                            &scene_frame,
                            frame_number,
                        )
                        .unwrap()
                        .unwrap();
                    let world_from_clip = ring
                        .push(&[camera.clip_from_world.inverse().to_cols_array()])
                        .unwrap();
                    let out = fg.import_buffer(
                        "camera ray distances",
                        ImportedBuffer {
                            buffer: *buffer,
                            size: u64::from(WIDTH * HEIGHT) * 4,
                            address: *address,
                            initial: BufferState::idle(),
                            final_state: Some(BufferState::idle()),
                        },
                    );
                    ray_frame.add_camera_pass(
                        &mut fg,
                        &bindless,
                        world_from_clip,
                        out,
                        [WIDTH, HEIGHT],
                        RAY_MASK_ALL,
                    );
                    ray_frames = Some((scene_frame, ray_frame));
                    if capture {
                        let readback = *readback;
                        let to = fg.import_buffer(
                            "camera ray readback",
                            ImportedBuffer {
                                buffer: readback,
                                size: u64::from(WIDTH * HEIGHT) * 4,
                                address: 0,
                                initial: BufferState::idle(),
                                final_state: Some(BufferState::host_read()),
                            },
                        );
                        fg.add_pass("camera ray readback")
                            .read(out, Access::TransferRead)
                            .write(to, Access::TransferWrite)
                            .execute(move |ctx| {
                                // SAFETY: recording; the graph ordered the copy.
                                unsafe {
                                    ctx.device.cmd_copy_buffer(
                                        ctx.cmd,
                                        ctx.buffer(out),
                                        ctx.buffer(to),
                                        &[vk::BufferCopy {
                                            src_offset: 0,
                                            dst_offset: 0,
                                            size: u64::from(WIDTH * HEIGHT) * 4,
                                        }],
                                    );
                                }
                            });
                    }
                }
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
                if probes_every_frame && let Some(p) = &probes {
                    p.refresh_all();
                }
                if local_shadows_every_frame {
                    renderer.invalidate_local_shadows();
                }
                let started = Instant::now();
                let gi_view = gi.as_ref().map(|g| ViewGi::Probes {
                    volume: g.begin_frame(&mut fg),
                    rays: ray_frames.map(|(_, r)| r),
                });
                let view = ForwardView {
                    camera,
                    light,
                    color: hdr,
                    depth,
                    clear: Some([0.02, 0.02, 0.05, 1.0]),
                    occlusion: None,
                    temporal: temporal.as_ref(),
                    lights: &lights,
                    decals: &decals,
                    environment: environment.as_ref(),
                    reflections: Reflections {
                        probes: probes.as_ref(),
                        ssr: screen_space.as_ref(),
                    },
                    gi: gi_view,
                };
                let passes = match (gi_view, &ray_frames) {
                    // With GI (its updates need them) or `--scene-draws`, the draws as the
                    // GPU scene's instances.
                    (g, Some((scene_frame, _))) if g.is_some() || scene_draws => renderer
                        .add_scene_passes(
                            &mut fg,
                            &mut ring,
                            &bindless,
                            &geo,
                            &mats,
                            scene_frame,
                            &view,
                        ),
                    _ => renderer
                        .add_passes(&mut fg, &mut ring, &bindless, &geo, &mats, &draws, &view),
                }
                .unwrap();
                // `GI_DEBUG=1`: the screenshot shows the GI irradiance per pixel instead.
                let shown = passes
                    .gi
                    .filter(|_| std::env::var("GI_DEBUG").is_ok())
                    .unwrap_or(hdr);
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
                        shown,
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
                let (mut capture_frame, mut prefilter_frame) = (0.0, 0.0);
                let mut ssr_frame = [0.0; 4];
                let mut reflect_frame = 0.0;
                let mut local_frame = 0.0;
                let mut contact_frame = 0.0;
                let (mut decal_tiles_frame, mut decal_frame) = (0.0, 0.0);
                let (mut tlas_frame, mut camera_rays_frame) = (0.0, 0.0);
                let mut gi_resolve_frame = 0.0;
                for time in graph.gpu_pass_times() {
                    let us = f64::from(time.gpu_us);
                    match time.name {
                        "probe mips" | "probe prefilter" => prefilter_frame += us,
                        "ssr depth" => ssr_frame[0] += us,
                        "ssr trace" => ssr_frame[1] += us,
                        "ssr spatial" => ssr_frame[2] += us,
                        "ssr temporal" => ssr_frame[3] += us,
                        "reflections" => reflect_frame += us,
                        name if name.starts_with("probe ") => capture_frame += us,
                        _ => {}
                    }
                    match time.name {
                        "contact shadows" | "contact shadows clear" => {
                            contact_frame += f64::from(time.gpu_us);
                        }
                        "decal tiles clear" | "decal tiles" => decal_tiles_frame += us,
                        "tlas instances" | "tlas build" => tlas_frame += us,
                        "gi resolve" | "gi upsample" => gi_resolve_frame += us,
                        "camera rays" => camera_rays_frame += us,
                        "decals" => decal_frame += us,
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
                probe_capture.push(capture_frame);
                prefilter.push(prefilter_frame);
                ssr_depth.push(ssr_frame[0]);
                ssr_trace.push(ssr_frame[1]);
                ssr_spatial.push(ssr_frame[2]);
                ssr_temporal.push(ssr_frame[3]);
                reflect_composite.push(reflect_frame);
                faces_drawn.push(f64::from(probes.as_ref().map_or(0, |p| p.stats().faces)));
                local_time.push(local_frame);
                contact_time.push(contact_frame);
                decal_tiles_time.push(decal_tiles_frame);
                tlas_time.push(tlas_frame);
                gi_resolve_time.push(gi_resolve_frame);
                camera_rays_time.push(camera_rays_frame);
                decal_time.push(decal_frame);
                local_views.push(f64::from(renderer.local_shadow_stats().views_drawn));
                if let Some(us) = graph.gpu_frame_time() {
                    frame_time.push(f64::from(us));
                }
            }
            // A frame after convergence has two windows (around the camera, then of the
            // whole volume): its passes' times are summed by name.
            let window_start = gi_window_us.first().map_or(0, |(_, t)| t.len());
            for time in graph.gpu_pass_times() {
                if time.name.starts_with("gi ")
                    && time.name != "gi resolve"
                    && time.name != "gi upsample"
                {
                    let us = f64::from(time.gpu_us);
                    if gi_converging {
                        match gi_update_us.iter_mut().find(|(n, _)| *n == time.name) {
                            Some((_, sum)) => *sum += us,
                            None => gi_update_us.push((time.name, us)),
                        }
                    } else {
                        match gi_window_us.iter_mut().find(|(n, _)| *n == time.name) {
                            Some((_, times)) if times.len() > window_start => {
                                *times.last_mut().unwrap() += us;
                            }
                            Some((_, times)) => times.push(us),
                            None => gi_window_us.push((time.name, vec![us])),
                        }
                    }
                }
                if time.name == "blas builds" {
                    blas_build_us += f64::from(time.gpu_us);
                    blas_build_frames += 1;
                    blas_build_max = blas_build_max.max(f64::from(time.gpu_us));
                }
            }
            if capture
                && let Some(file) = &screenshot
                && let Some((.., Some(memory), _)) = &ray_bed
            {
                let ptr = memory.mapped_ptr().unwrap();
                // SAFETY: the GPU is idle; the mapping holds a float per pixel.
                let distances = unsafe {
                    core::slice::from_raw_parts(
                        ptr.as_ptr().cast::<f32>(),
                        (WIDTH * HEIGHT) as usize,
                    )
                };
                let mut pixels = Vec::with_capacity(distances.len() * 4);
                for &t in distances {
                    let v = if t < 0.0 {
                        0
                    } else {
                        (255.0 / (1.0 + t * 0.15)) as u8
                    };
                    pixels.extend_from_slice(&[v, v, v, 255]);
                }
                let png = encode_png(WIDTH, HEIGHT, PngFormat::Rgba, &pixels).unwrap();
                std::fs::write(file.replace(".png", "-rays.png"), png).unwrap();
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
            "| {mode} | {upload_frames} | {:.0} | {:.0} | {:.0} | {:.0} | {:.1} | {:.0} | {:.1} | {:.0} | {:.1} | {:.1} | {:.1} | {:.0} | {:.0} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} | {:.1} | {:.1} | {:.1} |",
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
            median(&mut decal_tiles_time),
            median(&mut decal_time),
        );
        if let Some(g) = &gi {
            let grid = g.grid();
            gi_rows.push(format!(
                "| {mode} | {} ({}×{}×{}) | {:.1} | {:.0} | {:.0} |",
                grid.probe_count(),
                grid.counts[0],
                grid.counts[1],
                grid.counts[2],
                g.memory_bytes() as f64 / f64::from(1 << 20),
                median(&mut gi_resolve_time),
                median(&mut forward),
            ));
        }
        if let Some((_, rays, ..)) = &ray_bed {
            let stats = rays.stats();
            let us = median(&mut camera_rays_time);
            ray_rows.push(format!(
                "| {mode} | {} | {:.1} | {} | {:.1} | {:.1} | {:.0} | {:.0} |",
                stats.blas_count,
                stats.blas_bytes as f64 / f64::from(1 << 20),
                stats.triangles,
                stats.tlas_bytes as f64 / 1024.0,
                median(&mut tlas_time),
                us,
                f64::from(WIDTH * HEIGHT) / us.max(1e-3),
            ));
        }
        if probes.is_some() || screen_space.is_some() {
            reflection_rows.push(format!(
                "| {mode} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} |",
                median(&mut faces_drawn),
                median(&mut probe_capture),
                median(&mut prefilter),
                median(&mut ssr_depth),
                median(&mut ssr_trace),
                median(&mut ssr_spatial),
                median(&mut ssr_temporal),
                median(&mut reflect_composite),
            ));
        }
    }
    if !gi_rows.is_empty() {
        println!();
        let updates: Vec<String> = gi_update_us
            .iter()
            .map(|(name, us)| format!("{name} {us:.0} µs"))
            .collect();
        println!(
            "GI: {} probes; converging (all updates, GPU): {}",
            gi.as_ref().map_or(0, |g| g.grid().probe_count()),
            updates.join(", ")
        );
        if let Some(g) = &gi
            && !gi_window_us.is_empty()
        {
            let window: Vec<String> = gi_window_us
                .iter_mut()
                .map(|(name, times)| format!("{name} {:.0} µs", median(times)))
                .collect();
            let total: f64 = gi_window_us
                .iter_mut()
                .map(|(_, times)| median(times))
                .sum();
            println!(
                "GI after convergence, per frame (median; {} probes × {} rays, {:.0} % around the camera): {} = {total:.0} µs",
                g.window(),
                g.settings().rays,
                g.settings().local_share * 100.0,
                window.join(", ")
            );
        }
        println!("| materials | probes | MiB | GPU resolve µs | GPU forward µs |");
        println!("|---|---:|---:|---:|---:|");
        for row in &gi_rows {
            println!("{row}");
        }
    }
    if !ray_rows.is_empty() {
        println!();
        println!(
            "Rays: bottom levels built in {blas_build_us:.0} µs of GPU time over {blas_build_frames} frame(s), at most {blas_build_max:.0} µs in one; {:?}, budget {}",
            ray_detail.unwrap_or_default(),
            ray_budget.as_deref().unwrap_or("none")
        );
        println!(
            "| materials | bottom levels | MiB | triangles | top level KiB | GPU top level µs | GPU camera rays µs | Mrays/s |"
        );
        println!("|---|---:|---:|---:|---:|---:|---:|---:|");
        for row in &ray_rows {
            println!("{row}");
        }
    }
    if !reflection_rows.is_empty() {
        println!();
        if let Some(p) = &probes {
            println!("Reflection probes: {:?}", p.stats());
        }
        println!(
            "| materials | probe faces per frame | GPU probe capture µs | GPU probe mips + prefilter µs | \
             GPU SSR depth µs | GPU SSR trace µs | GPU SSR spatial µs | GPU SSR temporal µs | \
             GPU reflections composite µs |"
        );
        println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
        for row in &reflection_rows {
            println!("{row}");
        }
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
        if let Some(p) = probes.as_mut() {
            p.destroy(&mut allocator, &bindless);
        }
        if let Some(s) = screen_space.as_mut() {
            s.destroy(&mut allocator, &bindless);
        }
        for (texture, _) in &mut decal_textures {
            texture.destroy(&device, &mut allocator, &bindless, 0);
        }
        if let Some(mut g) = gi {
            g.destroy(&device, &mut allocator, &bindless);
        }
        if let Some((mut scene, mut rays, buffer, memory, _, readback, readback_memory, _)) =
            ray_bed.take()
        {
            rays.destroy(&device, &mut allocator);
            scene.destroy(&mut allocator);
            if let Some(memory) = memory {
                allocator.destroy_buffer(buffer, memory);
            }
            if let Some(memory) = readback_memory {
                allocator.destroy_buffer(readback, memory);
            }
        }
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(command_pool, None);
    }
}

/// Texels per side of `--decals`' textures.
const DECAL_TEXTURE: u32 = 64;

/// `--decals`' albedo: a round blot, white (the decals' tints color it), alpha falling
/// off at its rim.
fn decal_blot() -> Vec<u8> {
    let mut out = Vec::with_capacity((DECAL_TEXTURE * DECAL_TEXTURE * 4) as usize);
    for y in 0..DECAL_TEXTURE {
        for x in 0..DECAL_TEXTURE {
            let u = (x as f32 + 0.5) / DECAL_TEXTURE as f32 * 2.0 - 1.0;
            let v = (y as f32 + 0.5) / DECAL_TEXTURE as f32 * 2.0 - 1.0;
            let r = (u * u + v * v).sqrt();
            let alpha = ((1.0 - r) / 0.3).clamp(0.0, 1.0);
            let shade = (200.0 + 55.0 * (u * 9.0).sin() * (v * 7.0).cos()) as u8;
            out.extend_from_slice(&[shade, shade, shade, (alpha * 255.0) as u8]);
        }
    }
    out
}

/// `--decals`' normal map: a grid of bumps.
fn decal_bumps() -> Vec<u8> {
    let mut out = Vec::with_capacity((DECAL_TEXTURE * DECAL_TEXTURE * 4) as usize);
    let k = 6.0 * core::f32::consts::PI;
    for y in 0..DECAL_TEXTURE {
        for x in 0..DECAL_TEXTURE {
            let u = (x as f32 + 0.5) / DECAL_TEXTURE as f32;
            let v = (y as f32 + 0.5) / DECAL_TEXTURE as f32;
            // Heights sin(ku) sin(kv); the normal map's y runs up the image.
            let dx = 0.08 * k * (k * u).cos() * (k * v).sin();
            let dy = -0.08 * k * (k * u).sin() * (k * v).cos();
            let n = Vec3::new(-dx, -dy, 1.0).normalize();
            let byte = |c: f32| ((c * 0.5 + 0.5) * 255.0).round() as u8;
            out.extend_from_slice(&[byte(n.x), byte(n.y), byte(n.z), 255]);
        }
    }
    out
}
