//! A test bed: fixed scenes through the whole frame (culling, shadows, forward shading,
//! local lights, sky light, transparency, antialiasing or upscaling, bloom, tone mapping),
//! composed by the frame renderer (`engine_frame`, ADR 0072), rendered on one GPU and read
//! back as 8-bit sRGB, with the GPU time of every pass. The golden-image tests and the
//! performance gate both use it (ADR 0062).

use engine_anim::{local_to_model, skinning_matrices};
use engine_assets::hdr::{SunSearch, decode_hdr, find_sun};
use engine_assets::{
    AlphaMode as GltfAlpha, CookSettings, CookedScene, DerivedDataCache, TextureFormat as Stored,
    TextureSlot, TextureWrap, cook_scene, import_gltf, write_pack,
};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Mat4, Quat, Vec3, look_at};
use engine_ecs::{Entity, GlobalTransform, Transform, World};
use engine_frame::{
    Antialiasing, Exposure, FrameContext, FrameRenderer, PrepareContext, Quality, SceneDraws,
    SceneInput, View, ViewCamera, ViewSettings, ViewTarget,
};
use engine_render::ao::AoSettings;
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::budget::{BudgetSettings, GpuBudget};
use engine_render::environment::{Environment, Sky};
use engine_render::feedback::{FeedbackSettings, TextureFeedback};
use engine_render::forward::{
    DirectionalLight, DrawItem, LocalLight, LocalShadowSettings, ShadowSettings, Spot,
};
use engine_render::gpu_scene::GpuScene;
use engine_render::graph::{Access, BufferState, ImportedBuffer, LoadOp, RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable, Subsurface, TextureRef};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData, MeshId, OwnedMeshData, cube};
use engine_render::panorama::{Panorama, PanoramaDesc, PanoramaSun};
use engine_render::particles::{ParticleEmitter, ParticleSystem, ParticleTarget};
use engine_render::post::{HDR_FORMAT, ToneCurve};
use engine_render::reflections::{ProbeSettings, ReflectionProbe, ReflectionProbes, SsrSettings};
use engine_render::skinning::{SkinnedMeshData, Skinning};
use engine_render::subsurface::SubsurfaceProfile;
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation, Wrap,
};
use engine_runtime::{Camera, CameraView, MeshRenderer, Runtime, look_rotation};
use engine_streaming::{SceneStreamSettings, SceneStreamer};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The game runtime a `Shot::runtime` shot goes through.
struct Game {
    runtime: Runtime,
    scene: GpuScene,
    cameras: CameraView,
    /// The camera's entity.
    camera: Entity,
}

/// Where the scene comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Built here: spheres of graded roughness and metalness, a glass cube, a glowing
    /// cube, a floor and a wall. Needs no files.
    Procedural,
    /// A glTF file, relative to [`test_assets`] (fetched by
    /// `tools/fetch-test-assets.ps1`), cooked through the derived data cache in its `.ddc`.
    Gltf(PathBuf),
}

/// One fixed view of a scene and what the frame includes.
#[derive(Debug, Clone)]
pub struct Shot {
    /// Name of the golden image (`<name>.png`).
    pub name: &'static str,
    /// Where the scene comes from.
    pub source: Source,
    /// Output size in pixels.
    pub size: [u32; 2],
    /// Camera position, or `None` for the scene's default view (as `scene_bench`'s).
    pub eye: Option<Vec3>,
    /// Point looked at.
    pub target: Option<Vec3>,
    /// Vertical field of view, degrees.
    pub fov_y: f32,
    /// The sun.
    pub sun: DirectionalLight,
    /// Point and spot lights.
    pub lights: Vec<LocalLight>,
    /// Sky light and the sky behind the scene (ADR 0046).
    pub sky: bool,
    /// Bloom (ADR 0047).
    pub bloom: bool,
    /// Exposure (fixed: the automatic one adapts over frames).
    pub exposure: f32,
    /// Tone curve.
    pub curve: ToneCurve,
    /// Depth prepass.
    pub prepass: bool,
    /// Ambient occlusion (ADR 0058; it turns the depth prepass on).
    pub ao: Option<AoSettings>,
    /// Subsurface scattering (ADR 0059): in the procedural scene the floor and the
    /// dielectric spheres scatter light like wax (a profile of a few tenths of a unit).
    pub subsurface: bool,
    /// Async compute (ADR 0065) on or off, or `None` for the device's default.
    pub async_compute: Option<bool>,
    /// Skinned scenes (ADR 0068): every skin posed by its clip of this index at this time
    /// (seconds); `None` draws skinned meshes in their bind pose, unskinned.
    pub animation: Option<(usize, f32)>,
    /// Clip seconds per frame from the first temporal frame (the clip plays; the skinned
    /// motion pass gives the moving vertices their motion vectors). 0: a still pose.
    pub animation_rate: f32,
    /// A physically based sky and atmosphere (ADR 0069) instead of the gradient sky (with
    /// `sky`); it drives the sun's color.
    pub atmosphere: Option<Atmosphere>,
    /// Exponential height fog (ADR 0069; with `sky`).
    pub fog: Option<HeightFog>,
    /// Shadows of local lights (ADR 0071): every light casts them.
    pub local_shadows: bool,
    /// The sun's contact shadows (ADR 0071; they turn the depth prepass on).
    pub contact_shadows: bool,
    /// Antialiasing (ADR 0072): off, or a temporal upscaler. Temporal shots start
    /// accumulating once the scene is resident and are captured `temporal_frames` later.
    pub antialiasing: Antialiasing,
    /// The render size relative to `size` (the upscaler's ratio for `quality`).
    pub quality: Quality,
    /// A fixed render size (`ViewSettings::render_size`: pixel art, integer scaling).
    pub render_size: Option<(u32, u32)>,
    /// Two-phase occlusion culling (ADR 0035).
    pub occlusion_culling: bool,
    /// Frames accumulated before the capture of a temporal shot (the history's state at
    /// the capture is then the same on every run).
    pub temporal_frames: u32,
    /// Frames captured, one after the other (temporal stability is measured on
    /// sequences); [`Rendered::rgba8`] is the last, [`Rendered::frames`] all.
    pub captures: u32,
    /// Frames from the first temporal frame to the first capture, overriding
    /// `temporal_frames - 1` for temporal shots and 0 for the others: shots of different
    /// antialiasing then capture the same camera positions.
    pub capture_after: Option<u32>,
    /// The camera's motion per frame (eye and target move together), from the first
    /// temporal frame: ghosting and smearing show in the capture.
    pub camera_motion: Vec3,
    /// Through the game runtime (ADR 0064, ADR 0072): the draws become ECS entities
    /// extracted into a GPU scene, and the camera an entity with a `Camera`, resolved by
    /// a `CameraView` every frame. Implies `rigid_camera`.
    pub runtime: bool,
    /// The camera as a rotation and a position (a camera entity's transform) instead of
    /// `look_at`'s matrix: what the runtime path renders with.
    pub rigid_camera: bool,
    /// Reflection probes (ADR 0073; with `sky`): the scene's bounding box cut into this
    /// many boxes per axis, a probe in each.
    pub probe_grid: Option<[u32; 3]>,
    /// Screen-space reflections (ADR 0073; with `sky`).
    pub ssr: Option<SsrSettings>,
    /// The procedural scene's floor as polished metal (reflections show on it).
    pub glossy_floor: bool,
    /// A glTF scene's textures streamed from a pack with texture feedback, in a streaming
    /// pool of this many bytes (ADR 0076); `None`: every level uploaded. The shot is
    /// captured once streaming has settled.
    pub texture_budget: Option<u64>,
    /// GPU particles (ADR 0081): sparks added over the scene and smoke blended into it,
    /// from the floor's middle, simulated from the first frame in steps of 1/60 s; with
    /// temporal antialiasing they mark a reactive mask
    /// (`ForwardRenderer::set_reactive_effects`).
    pub particles: bool,
    /// An HDR panorama as the sky (ADR 0083; with `sky`): a `.hdr` file in the test
    /// assets, behind the scene and in its image-based light. Its sun, found in it, is the
    /// shot's sun (direction and color; `sun`'s ambient term stays).
    pub panorama: Option<&'static str>,
}

impl Shot {
    /// Defaults for a shot of `source`: default view, a sun, sky light, bloom, GT curve.
    pub fn new(name: &'static str, source: Source) -> Self {
        Self {
            name,
            source,
            size: [640, 360],
            eye: None,
            target: None,
            fov_y: 60.0,
            sun: DirectionalLight {
                direction: Vec3::new(-0.3, -1.0, -0.2),
                color: Vec3::splat(3.0),
                ambient: Vec3::splat(0.08),
            },
            lights: Vec::new(),
            sky: true,
            bloom: true,
            exposure: 1.0,
            curve: ToneCurve::Gt,
            prepass: false,
            ao: None,
            subsurface: false,
            async_compute: None,
            animation: None,
            animation_rate: 0.0,
            atmosphere: None,
            fog: None,
            local_shadows: false,
            contact_shadows: false,
            antialiasing: Antialiasing::OFF,
            quality: Quality::Native,
            render_size: None,
            occlusion_culling: false,
            temporal_frames: 32,
            captures: 1,
            capture_after: None,
            camera_motion: Vec3::ZERO,
            runtime: false,
            rigid_camera: false,
            probe_grid: None,
            ssr: None,
            glossy_floor: false,
            texture_budget: None,
            particles: false,
            panorama: None,
        }
    }
}

/// What [`render`] produced.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The adapter's name and driver.
    pub adapter: String,
    /// PCI vendor id of the adapter (0x10de NVIDIA, 0x1002 AMD, 0x8086 Intel).
    pub vendor_id: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The frame, 8-bit sRGB RGBA, rows top to bottom.
    pub rgba8: Vec<u8>,
    /// Median GPU time of each pass over the timed frames, µs, by pass name (passes of
    /// the same name in one frame are added).
    pub pass_us: BTreeMap<&'static str, f64>,
    /// Median GPU time of the frames end to end (first pass to last), µs: with async
    /// compute, less than the sum of the passes.
    pub frame_us: f64,
    /// Whether async passes ran on the async compute queue.
    pub async_compute: bool,
    /// The antialiasing used (the shot's, or TAAU when its upscaler was unavailable).
    pub antialiasing: Antialiasing,
    /// Every captured frame ([`Shot::captures`]), in order, as `rgba8`.
    pub frames: Vec<Vec<u8>>,
}

/// How to run [`render`].
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// The validation layer with synchronization validation; its errors fail the render.
    pub validation: bool,
    /// Frames timed after the scene is resident (0: only the captured frame).
    pub timed_frames: usize,
}

/// Why [`render`] could not run.
#[derive(Debug)]
pub enum Skip {
    /// No Vulkan device (and `ENGINE_REQUIRE_GPU` is not set).
    NoGpu(String),
    /// A glTF scene is missing (fetch it with `tools/fetch-test-assets.ps1`).
    NoScene(PathBuf),
    /// The shot needs DLSS, which is not available here (no Streamline DLLs in
    /// `ENGINE_STREAMLINE_DIR`, or not an NVIDIA RTX GPU).
    NoDlss(String),
    /// The shot's antialiasing cannot run on this GPU (`FrameRenderer::check`: an MSAA
    /// sample count it does not support).
    Unsupported(String),
}

/// The test scenes: `ENGINE_TEST_ASSETS`, or `test-assets/` in the repository (a git
/// worktree has none of its own: point the variable at the main checkout's).
pub fn test_assets() -> PathBuf {
    std::env::var_os("ENGINE_TEST_ASSETS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets"),
        PathBuf::from,
    )
}

/// Vendor directory name for golden images: `nvidia`, `amd`, `intel` or the id in hex.
pub fn vendor_name(vendor_id: u32) -> String {
    match vendor_id {
        0x10de => String::from("nvidia"),
        0x1002 => String::from("amd"),
        0x8086 => String::from("intel"),
        other => format!("{other:04x}"),
    }
}

struct Loaded {
    cooked: Option<CookedScene>,
    procedural: Vec<(OwnedMeshData, MaterialDesc, Mat4)>,
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

fn wrap(w: TextureWrap) -> Wrap {
    match w {
        TextureWrap::Repeat => Wrap::Repeat,
        TextureWrap::MirroredRepeat => Wrap::MirroredRepeat,
        TextureWrap::ClampToEdge => Wrap::ClampToEdge,
    }
}

fn material_descs(scene: &CookedScene, ids: &[Option<TextureId>]) -> Vec<MaterialDesc> {
    let find = |slot: Option<TextureSlot>| -> Option<TextureRef> {
        let slot = slot?;
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

/// A UV sphere of radius 0.5: `rings` × `segments` quads.
pub fn sphere(rings: u32, segments: u32) -> OwnedMeshData {
    let mut mesh = OwnedMeshData::default();
    for r in 0..=rings {
        let v = r as f32 / rings as f32;
        let theta = v * core::f32::consts::PI;
        for s in 0..=segments {
            let u = s as f32 / segments as f32;
            let phi = u * core::f32::consts::TAU;
            let n = Vec3::new(
                theta.sin() * phi.cos(),
                theta.cos(),
                -theta.sin() * phi.sin(),
            );
            mesh.positions.push((n * 0.5).to_array());
            mesh.normals.push(n.to_array());
            // Tangent along increasing u (around the axis).
            let t = Vec3::new(-phi.sin(), 0.0, -phi.cos());
            mesh.tangents.push(t.extend(1.0).to_array());
            mesh.uvs.push([u, v]);
        }
    }
    let row = segments + 1;
    for r in 0..rings {
        for s in 0..segments {
            let (a, b) = (r * row + s, (r + 1) * row + s);
            mesh.indices.extend([a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    mesh
}

/// The procedural scene: spheres of graded roughness (top row metal, bottom row
/// dielectric), a glass cube, a glowing cube, a floor and a wall.
fn procedural(subsurface: bool, glossy_floor: bool) -> Vec<(OwnedMeshData, MaterialDesc, Mat4)> {
    let ball = sphere(24, 48);
    let block = cube();
    let floor = if glossy_floor {
        MaterialDesc::solid([0.75, 0.75, 0.78], 1.0, 0.12)
    } else {
        MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 0.8)
    };
    let mut items = vec![
        (
            block.clone(),
            MaterialDesc {
                // Shadow edges show scattering best: a red fringe (red travels furthest).
                subsurface: subsurface.then(Subsurface::default),
                ..floor
            },
            Mat4::from_scale_rotation_translation(
                Vec3::new(14.0, 0.2, 10.0),
                Quat::IDENTITY,
                Vec3::new(0.0, -0.1, 0.0),
            ),
        ),
        (
            block.clone(),
            MaterialDesc::solid([0.7, 0.65, 0.6], 0.0, 0.6),
            Mat4::from_scale_rotation_translation(
                Vec3::new(14.0, 5.0, 0.2),
                Quat::IDENTITY,
                Vec3::new(0.0, 2.5, -3.0),
            ),
        ),
    ];
    for i in 0..5 {
        let roughness = 0.1 + 0.2 * i as f32;
        let x = -3.0 + 1.5 * i as f32;
        items.push((
            ball.clone(),
            MaterialDesc::solid([1.0, 0.78, 0.34], 1.0, roughness),
            Mat4::from_translation(Vec3::new(x, 2.0, -1.0)),
        ));
        items.push((
            ball.clone(),
            MaterialDesc {
                subsurface: subsurface.then(Subsurface::default),
                ..MaterialDesc::solid([0.8, 0.1, 0.1], 0.0, roughness)
            },
            Mat4::from_translation(Vec3::new(x, 0.5, -1.0)),
        ));
    }
    items.push((
        block.clone(),
        MaterialDesc {
            base_color: [0.4, 0.8, 1.0, 0.35],
            roughness: 0.05,
            alpha: AlphaMode::Blend,
            ..MaterialDesc::default()
        },
        Mat4::from_scale_rotation_translation(
            Vec3::splat(0.9),
            Quat::from_rotation_y(0.6),
            Vec3::new(-1.5, 0.45, 1.2),
        ),
    ));
    items.push((
        block,
        MaterialDesc {
            emissive: [6.0, 3.0, 0.8],
            ..MaterialDesc::solid([0.1, 0.1, 0.1], 0.0, 0.5)
        },
        Mat4::from_scale_rotation_translation(
            Vec3::splat(0.6),
            Quat::from_rotation_y(0.3),
            Vec3::new(1.8, 0.3, 1.0),
        ),
    ));
    items
}

/// Lights for the procedural scene: a warm point light and a cold spot light.
pub fn procedural_lights() -> Vec<LocalLight> {
    vec![
        LocalLight {
            position: Vec3::new(-2.5, 1.2, 0.5),
            range: 4.0,
            color: Vec3::new(3.0, 1.8, 0.8),
            spot: None,
            shadow: false,
        },
        LocalLight {
            position: Vec3::new(2.5, 3.5, 0.0),
            range: 6.0,
            color: Vec3::new(6.0, 9.0, 18.0),
            spot: Some(Spot {
                direction: Vec3::new(0.0, -1.0, -0.3).normalize(),
                inner: 0.3,
                outer: 0.6,
            }),
            shadow: false,
        },
    ]
}

/// Warm point lights along Sponza's hall, alternating sides, and two cold spot lights
/// from above.
pub fn sponza_lights() -> Vec<LocalLight> {
    let mut lights: Vec<LocalLight> = (0..5)
        .map(|k| LocalLight {
            position: Vec3::new(
                -6.0 + 4.0 * k as f32,
                1.4,
                if k % 2 == 0 { 2.2 } else { -2.2 },
            ),
            range: 8.0,
            color: Vec3::new(24.0, 15.0, 8.0),
            spot: None,
            shadow: true,
        })
        .collect();
    for x in [-2.0, 6.0] {
        lights.push(LocalLight {
            position: Vec3::new(x, 7.0, 0.0),
            range: 12.0,
            color: Vec3::new(90.0, 100.0, 140.0),
            spot: Some(Spot {
                direction: Vec3::new(0.15, -1.0, 0.1).normalize(),
                inner: 0.4,
                outer: 0.65,
            }),
            shadow: true,
        });
    }
    lights
}

/// Streamline when the shot asks for DLSS (ADR 0063), loaded before the instance.
type Dlss = Option<engine_dlss::Streamline>;

fn gpu(validation: bool, dlss: bool) -> Result<(Instance, Device, Dlss), Skip> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let streamline = if dlss {
        match engine_dlss::Streamline::load(&engine_dlss::StreamlineDesc::default()) {
            Ok(streamline) => Some(streamline),
            Err(e) => return Err(Skip::NoDlss(e.to_string())),
        }
    } else {
        None
    };
    let desc = InstanceDesc {
        app_name: String::from("engine_golden"),
        validation,
        debug_utils: true,
        surface: false,
        headless_surface: false,
        sync_validation: validation,
    };
    let attempt = || -> Result<Result<(Instance, Device, Dlss), Skip>, engine_rhi::RhiError> {
        let instance = match &streamline {
            Some(s) => Instance::with_extensions(&desc, &s.instance_extensions())?,
            None => Instance::new(&desc)?,
        };
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        let Some(streamline) = &streamline else {
            let device = Device::new(&instance, &adapter)?;
            return Ok(Ok((instance, device, None)));
        };
        if let Err(e) = streamline.check_adapter(&instance, &adapter) {
            return Ok(Err(Skip::NoDlss(format!("{}: {e}", adapter.name()))));
        }
        let device = Device::with_extensions(&instance, &adapter, &streamline.device_extensions())?;
        if let Err(e) = streamline.attach(&device) {
            return Ok(Err(Skip::NoDlss(e.to_string())));
        }
        Ok(Ok((instance, device, Some(streamline.clone()))))
    };
    match attempt() {
        Ok(result) => result,
        Err(e) if !required => Err(Skip::NoGpu(e.to_string())),
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

fn load(source: &Source, subsurface: bool, glossy_floor: bool) -> Result<Loaded, Skip> {
    match source {
        Source::Procedural => Ok(Loaded {
            cooked: None,
            procedural: procedural(subsurface, glossy_floor),
        }),
        Source::Gltf(path) => {
            let path = test_assets().join(path);
            let Ok(bytes) = std::fs::read(&path) else {
                return Err(Skip::NoScene(path));
            };
            let dir = path.parent();
            let scene = import_gltf(&bytes, dir).map_err(|_| Skip::NoScene(path.clone()))?;
            let jobs = ThreadPool::builder()
                .build()
                .map_err(|_| Skip::NoScene(path.clone()))?;
            let cache = DerivedDataCache::new(test_assets().join(".ddc"))
                .map_err(|_| Skip::NoScene(path.clone()))?;
            let settings = CookSettings {
                lods: false,
                compression_level: 0,
                block_compression: true,
                ..CookSettings::default()
            };
            let (cooked, _) = cook_scene(&scene, dir, Some(&cache), &jobs, &settings)
                .map_err(|_| Skip::NoScene(path.clone()))?;
            Ok(Loaded {
                cooked: Some(cooked),
                procedural: Vec::new(),
            })
        }
    }
}

/// Probes over the scene's bounding box `min`..`max` cut into `grid` boxes: each
/// captured from its centre, reflecting and correcting parallax with its box.
pub fn probe_grid(min: Vec3, max: Vec3, grid: [u32; 3]) -> Vec<ReflectionProbe> {
    let g = Vec3::new(grid[0] as f32, grid[1] as f32, grid[2] as f32).max(Vec3::ONE);
    let cell = (max - min) / g;
    let mut probes = Vec::new();
    for z in 0..grid[2].max(1) {
        for y in 0..grid[1].max(1) {
            for x in 0..grid[0].max(1) {
                let center = min + cell * (Vec3::new(x as f32, y as f32, z as f32) + 0.5);
                probes.push(ReflectionProbe::room(center, cell * 0.5));
            }
        }
    }
    probes
}

/// A camera like `scene_bench`'s default: down the longest horizontal axis of a wide
/// scene, from the front of a compact one.
fn default_view(min: Vec3, max: Vec3) -> (Vec3, Vec3) {
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

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Renders `shot`, after every texture is resident, and times `timed_frames` more frames.
///
/// # Panics
///
/// On Vulkan errors and, with validation, on validation errors.
#[allow(clippy::too_many_lines)]
pub fn render(shot: &Shot, options: RenderOptions) -> Result<Rendered, Skip> {
    let loaded = load(&shot.source, shot.subsurface, shot.glossy_floor)?;
    // The HDR panorama, read and its sun found before the GPU starts.
    let panorama = match shot.panorama.filter(|_| shot.sky) {
        Some(name) => {
            let path = test_assets().join(name);
            let bytes = std::fs::read(&path).map_err(|_| Skip::NoScene(path.clone()))?;
            let image = decode_hdr(&bytes).map_err(|_| Skip::NoScene(path.clone()))?;
            let sun = find_sun(&image, &SunSearch::default());
            Some((image, sun))
        }
        None => None,
    };
    let (instance, device, streamline) = gpu(
        options.validation,
        shot.antialiasing.upscaler() == Some(engine_frame::UpscalerKind::Dlss),
    )?;
    let [width, height] = shot.size;
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).expect("bindless");
    let depth_format = vk::Format::D32_SFLOAT;
    let output_format = vk::Format::R8G8B8A8_SRGB;
    let mut frame = FrameRenderer::new(
        &device,
        &bindless,
        output_format,
        depth_format,
        ShadowSettings::default(),
        1,
    )
    .expect("frame renderer");
    frame.set_streamline(streamline.clone());
    let wanted = ViewSettings {
        antialiasing: shot.antialiasing,
        quality: shot.quality,
        render_size: shot.render_size,
        ..ViewSettings::default()
    };
    if let Err(e) = frame.check(&wanted, (width, height)) {
        return Err(Skip::Unsupported(e.to_string()));
    }
    let renderer = frame.forward_mut();
    renderer.set_depth_prepass(shot.prepass);
    renderer
        .set_ambient_occlusion(&device, &bindless, shot.ao)
        .expect("ambient occlusion");
    if shot.subsurface {
        renderer
            .set_subsurface_profiles(
                &device,
                &bindless,
                &[SubsurfaceProfile::from_radius([0.3, 0.12, 0.06])],
            )
            .expect("subsurface profiles");
    }
    if shot.local_shadows {
        renderer
            .set_local_shadows(
                &device,
                &mut allocator,
                &bindless,
                Some(LocalShadowSettings::default()),
            )
            .expect("local shadows");
    }
    if shot.contact_shadows {
        renderer
            .set_contact_shadows(&device, &bindless, Some(Default::default()))
            .expect("contact shadows");
    }
    let mut lights = shot.lights.clone();
    for light in lights.iter_mut() {
        light.shadow = shot.local_shadows;
    }
    let mut sun = shot.sun;
    let environment = shot.sky.then(|| {
        let mut environment =
            Environment::new(&device, &mut allocator, &bindless, Sky::default()).expect("sky");
        environment
            .set_atmosphere(&device, &mut allocator, &bindless, shot.atmosphere)
            .expect("atmosphere");
        environment.set_fog(shot.fog);
        if let Some((image, found)) = &panorama {
            let panorama = Panorama::new(
                &device,
                &mut allocator,
                &bindless,
                &PanoramaDesc {
                    width: image.width,
                    height: image.height,
                    pixels: &image.pixels,
                    sun: found.map(|s| PanoramaSun {
                        direction: s.direction,
                        illuminance: s.illuminance,
                        cone: s.cone,
                        sky: s.sky,
                    }),
                    background_size: 0,
                },
            )
            .expect("panorama");
            if let Some(s) = panorama.sun() {
                sun = DirectionalLight {
                    direction: -s.direction,
                    color: s.illuminance,
                    ambient: sun.ambient,
                };
            }
            environment.set_panorama(Some(panorama));
        }
        environment
    });
    let reflecting = shot.sky && (shot.probe_grid.is_some() || shot.ssr.is_some());
    frame
        .forward_mut()
        .set_reflections(&device, &bindless, reflecting)
        .expect("reflections");
    let mut probes = shot.probe_grid.filter(|_| shot.sky).map(|grid| {
        let capacity = grid.iter().product::<u32>().max(1);
        ReflectionProbes::new(
            &device,
            &mut allocator,
            &bindless,
            ProbeSettings {
                capacity,
                ..ProbeSettings::default()
            },
        )
        .expect("reflection probes")
    });
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 64 << 20).expect("ring");
    let mut graph = RenderGraph::new(&device, 1);
    if let Some(on) = shot.async_compute {
        graph.set_async_compute(on);
    }
    let readback_size = u64::from(width * height * 4);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(readback_size)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .expect("readback");
    // SAFETY: plain object creation.
    let (command_pool, cmd) = unsafe {
        let command_pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
            .expect("command pool");
        let cmd = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .command_buffer_count(1),
            )
            .expect("command buffer")[0];
        (command_pool, cmd)
    };

    let mut geometry = GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1)
        .expect("geometry");
    let mut skinning = Skinning::new(&device, &mut allocator, &bindless, depth_format, 1 << 14, 1)
        .expect("skinning");
    // Skinned instances: (skinning id, skin, world transform).
    let mut skinned = Vec::new();
    let mut skins = Vec::new();
    let mut textures = TexturePool::new(&device, 256 << 20, 1);
    // A captured frame shows the levels resident then, not a fade of them (ADR 0078).
    textures.set_mip_fade(None);
    let mut materials = MaterialTable::new(1);
    let mut draws: Vec<DrawItem> = Vec::new();
    let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let mut grow = |world: Mat4, lo: [f32; 3], hi: [f32; 3]| {
        for corner in 0..8 {
            let pick = |bit: usize, axis: usize| {
                if corner & bit != 0 {
                    hi[axis]
                } else {
                    lo[axis]
                }
            };
            let c = world.transform_point3(Vec3::new(pick(1, 0), pick(2, 1), pick(4, 2)));
            min = min.min(c);
            max = max.max(c);
        }
    };
    // Streaming (ADR 0076): the cooked scene into a pack, its textures streamed by
    // feedback within the shot's budget.
    let mut streaming = None;
    let mut loaded = loaded;
    if let Some(limit) = shot.texture_budget
        && let Some(cooked) = loaded.cooked.take()
    {
        let pack = std::env::temp_dir().join(format!(
            "engine_golden_{}_{}.pack",
            shot.name,
            std::process::id()
        ));
        let jobs = ThreadPool::builder().build().expect("jobs");
        write_pack(&pack, &cooked, &jobs, 1).expect("pack");
        let mut budget = GpuBudget::with_settings(BudgetSettings {
            streaming_limit: Some(limit),
            ..BudgetSettings::default()
        });
        let streamer = SceneStreamer::open(
            &pack,
            &jobs,
            &mut textures,
            &mut geometry,
            &mut allocator,
            &mut budget,
            SceneStreamSettings::default(),
        )
        .expect("streaming");
        let feedback = TextureFeedback::new(&device, &bindless, FeedbackSettings::default(), 1)
            .expect("feedback");
        let scene = &streamer.layout().scene;
        let ids = streamer.textures().to_vec();
        let material_ids: Vec<_> = scene
            .materials
            .iter()
            .map(|m| materials.add(engine_streaming::material_desc(m, &ids)))
            .collect();
        let fallback = materials.default_material();
        for instance in &scene.instances {
            let world = Mat4::from_cols_array(&instance.world_from_object);
            let mesh = &scene.meshes[instance.mesh as usize];
            draws.push(DrawItem {
                mesh: streamer.meshes()[instance.mesh as usize],
                material: instance
                    .material
                    .map_or(fallback, |i| material_ids[i as usize]),
                world_from_object: world,
            });
            grow(world, mesh.min, mesh.max);
        }
        streaming = Some((streamer, budget, feedback, pack));
    }
    if let Some(mut cooked) = loaded.cooked {
        let meshes: Vec<MeshId> = cooked
            .meshes
            .iter()
            .map(|m| {
                geometry
                    .add_mesh(&MeshData {
                        positions: &m.positions,
                        normals: &m.normals,
                        tangents: &m.tangents,
                        uvs: &m.uvs,
                        indices: &m.indices,
                    })
                    .expect("mesh")
            })
            .collect();
        let ids: Vec<Option<TextureId>> = std::mem::take(&mut cooked.textures)
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
        let material_ids: Vec<_> = material_descs(&cooked, &ids)
            .into_iter()
            .map(|m| materials.add(m))
            .collect();
        let fallback = materials.default_material();
        skins = std::mem::take(&mut cooked.skins);
        for instance in &cooked.instances {
            let world = Mat4::from_cols_array(&instance.world_from_object);
            let mesh = &cooked.meshes[instance.mesh as usize];
            let mut drawn = meshes[instance.mesh as usize];
            if let (Some(_), Some(skin)) = (shot.animation, instance.skin) {
                let source = skinning
                    .add_skin(&SkinnedMeshData {
                        mesh: MeshData {
                            positions: &mesh.positions,
                            normals: &mesh.normals,
                            tangents: &mesh.tangents,
                            uvs: &mesh.uvs,
                            indices: &mesh.indices,
                        },
                        joints: &mesh.joints,
                        weights: &mesh.weights,
                        joint_count: skins[skin as usize].joints.len() as u16,
                    })
                    .expect("skin");
                let (id, out) = skinning
                    .add_instance(source, &mut geometry)
                    .expect("skinned instance");
                skinned.push((id, skin as usize, world));
                drawn = out;
            }
            draws.push(DrawItem {
                mesh: drawn,
                material: instance
                    .material
                    .map_or(fallback, |i| material_ids[i as usize]),
                world_from_object: world,
            });
            grow(world, mesh.min, mesh.max);
        }
    }
    for (mesh, material, world) in &loaded.procedural {
        let id = geometry.add_mesh(&mesh.as_data()).expect("mesh");
        draws.push(DrawItem {
            mesh: id,
            material: materials.add(*material),
            world_from_object: *world,
        });
        grow(*world, [-0.5; 3], [0.5; 3]);
    }

    if let (Some(probes), Some(grid)) = (probes.as_mut(), shot.probe_grid) {
        for probe in probe_grid(min, max, grid) {
            probes.add(probe).expect("probe capacity");
        }
    }
    let (default_eye, default_target) = default_view(min, max);
    let eye = shot.eye.unwrap_or(default_eye);
    let target = shot.target.unwrap_or(default_target);
    let extent = (max - min).length();
    let temporal = shot.antialiasing.is_temporal();
    // Frames after residency before the capture: the sky's environment is built on the
    // first; a temporal shot then accumulates `temporal_frames` frames from a camera cut
    // at `TEMPORAL_START` (its history starts there), the last one captured.
    const TEMPORAL_START: usize = 2;
    const SSR_FRAMES: usize = 8;
    let capture_at = TEMPORAL_START
        + match shot.capture_after {
            Some(frames) => frames as usize,
            None if temporal => shot.temporal_frames.max(1) as usize - 1,
            None => 0,
        }
        // Screen-space reflections' own temporal filter converges over a few frames.
        + if shot.ssr.is_some() { SSR_FRAMES } else { 0 };
    let settings = ViewSettings {
        antialiasing: shot.antialiasing,
        quality: shot.quality,
        render_size: shot.render_size,
        occlusion_culling: shot.occlusion_culling,
        exposure: Exposure::Fixed(shot.exposure),
        curve: shot.curve,
        bloom: shot.bloom,
        clear: Some([0.02, 0.02, 0.05, 1.0]),
        screen_space_reflections: shot.ssr.filter(|_| shot.sky),
        ..ViewSettings::default()
    };
    let mut view = View::new(&device, draws.len() as u32);
    if shot.particles {
        frame.forward_mut().set_reactive_effects(true);
    }
    let mut particles = shot.particles.then(|| {
        let mut system =
            ParticleSystem::new(&device, &mut allocator, &bindless, 16_384, HDR_FORMAT)
                .expect("particles");
        // Sparks: fast, added (they glow), falling back.
        system.add_emitter(ParticleEmitter {
            transform: engine_core::math::Affine3A::from_translation(Vec3::new(0.0, 0.05, 0.0)),
            rate: 1_500.0,
            lifetime: [0.6, 1.2],
            radius: 0.05,
            direction: Vec3::Y,
            spread: 0.35,
            speed: [3.0, 5.0],
            acceleration: Vec3::new(0.0, -9.81, 0.0),
            drag: 0.2,
            size: [0.05, 0.02],
            color: [
                engine_core::math::Vec4::new(8.0, 3.0, 0.8, 1.0),
                engine_core::math::Vec4::new(2.0, 0.3, 0.05, 0.0),
            ],
            additive: 1.0,
            seed: 1,
            ..ParticleEmitter::default()
        });
        // Smoke: slow, blended, growing.
        system.add_emitter(ParticleEmitter {
            transform: engine_core::math::Affine3A::from_translation(Vec3::new(0.0, 0.2, 0.0)),
            rate: 120.0,
            lifetime: [2.0, 3.0],
            radius: 0.2,
            direction: Vec3::Y,
            spread: 0.5,
            speed: [0.4, 0.8],
            acceleration: Vec3::new(0.1, 0.3, 0.0),
            drag: 0.3,
            size: [0.3, 1.2],
            color: [
                engine_core::math::Vec4::new(0.4, 0.4, 0.42, 0.5),
                engine_core::math::Vec4::new(0.5, 0.5, 0.5, 0.0),
            ],
            additive: 0.0,
            spin: [-0.5, 0.5],
            soft_distance: 0.5,
            seed: 2,
            ..ParticleEmitter::default()
        });
        system
    });
    let mut antialiasing = Antialiasing::OFF;
    let (near, far) = (extent * 0.001, extent * 2.0);
    let rotation = look_rotation(eye, target, Vec3::Y);
    let mut game = shot.runtime.then(|| {
        let mut world = World::new();
        for draw in &draws {
            world.spawn((
                GlobalTransform(Affine3A::from_mat4(draw.world_from_object)),
                MeshRenderer::new(draw.mesh, draw.material),
            ));
        }
        let camera = world.spawn((
            Transform::from_translation(eye).with_rotation(rotation),
            Camera {
                exposure: shot.exposure,
                ..Camera::perspective(shot.fov_y.to_radians(), near, far)
                    .with_antialiasing(shot.antialiasing, shot.quality)
            },
        ));
        let cameras = CameraView::new(&mut world, settings);
        Game {
            scene: GpuScene::new(&device, &bindless, draws.len() as u32 + 16, 1).expect("scene"),
            runtime: Runtime::new(world, 64.0),
            cameras,
            camera,
        }
    });

    // The skins' skinning matrices at the shot's time plus `elapsed` clip seconds.
    let palettes_at = |elapsed: f32| -> Vec<Vec<[f32; 12]>> {
        skins
            .iter()
            .map(|skin| {
                let n = skin.skeleton.joint_count();
                let mut pose = skin.skeleton.rest().clone();
                if let Some(clip) = shot.animation.and_then(|(clip, _)| skin.clips.get(clip)) {
                    let time = shot.animation.map_or(0.0, |(_, t)| t) + elapsed;
                    clip.sample(time, skin.skeleton.rest(), &mut pose);
                }
                let mut model = vec![Affine3A::IDENTITY; n];
                local_to_model(&skin.skeleton, &pose, &Affine3A::IDENTITY, &mut model);
                let inverse_bind: Vec<Affine3A> = skin
                    .inverse_bind
                    .iter()
                    .map(|m| Affine3A::from_mat4(Mat4::from_cols_array(m)))
                    .collect();
                let mut palette = vec![[0.0; 12]; skin.joints.len()];
                skinning_matrices(&model, &skin.joints, &inverse_bind, &mut palette);
                palette
            })
            .collect()
    };
    let mut palettes = palettes_at(0.0);
    let mut timings: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
    let mut frame_times = Vec::new();
    let mut frame_number = 0u64;
    let mut timed = 0;
    let mut captured = 0u32;
    let captures = shot.captures.max(1);
    let mut frames = Vec::with_capacity(captures as usize);
    let mut resident_frames = 0;
    let mut previous_faces = 0;
    // Frames in a row with nothing to stream (feedback lags a few frames, and a coarser
    // level waits for its hold).
    let mut quiet = 0;
    while timed < options.timed_frames || captured < captures {
        if let Some((streamer, budget, feedback, _)) = streaming.as_mut() {
            streamer.update(
                &mut textures,
                &mut geometry,
                &mut allocator,
                budget,
                Some(feedback),
                None,
                &materials,
                frame_number + 1,
            );
            budget.update(&allocator);
            let idle = streamer.idle() && textures.pending_moves() == 0;
            quiet = if idle { quiet + 1 } else { 0 };
        }
        let uploading = textures.pending().0 > 0 || (streaming.is_some() && quiet < 70);
        // With probes, once every probe was captured with the textures resident.
        let probes_ready = probes.as_ref().is_none_or(|p| p.stats().pending == 0);
        let capture =
            !uploading && captured < captures && resident_frames >= capture_at && probes_ready;
        // Frames since the cut that starts a temporal shot's history (the jitter sequence
        // runs from the first frame: the scene's upload takes the same frames every run).
        let accumulated = (!uploading && resident_frames >= TEMPORAL_START)
            .then(|| resident_frames - TEMPORAL_START);
        frame_number += 1;
        let shift = shot.camera_motion * accumulated.unwrap_or(0) as f32;
        let (settings, camera) = match game.as_mut() {
            Some(game) => {
                let entity = game.camera;
                game.runtime.frame(
                    1.0 / 60.0,
                    &(),
                    &mut game.scene,
                    |_, (), _| {},
                    |world, (), _| {
                        // The camera follows the shot every rendered frame.
                        if let Some(t) = world.get_mut::<Transform>(entity) {
                            t.translation = eye + shift;
                        }
                        if accumulated == Some(0)
                            && let Some(camera) = world.get_mut::<Camera>(entity)
                        {
                            camera.cut();
                        }
                    },
                );
                let main = game.cameras.resolve(&game.runtime.world).expect("camera");
                (main.settings, main.camera)
            }
            None if shot.rigid_camera => (
                settings,
                ViewCamera {
                    cut: accumulated == Some(0),
                    ..ViewCamera::from_rigid(
                        rotation,
                        eye + shift,
                        shot.fov_y.to_radians(),
                        near,
                        far,
                    )
                },
            ),
            None => (
                settings,
                ViewCamera {
                    view_from_world: look_at(eye + shift, target + shift, Vec3::Y),
                    position: eye + shift,
                    fov_y: shot.fov_y.to_radians(),
                    near,
                    far,
                    cut: accumulated == Some(0),
                },
            ),
        };
        let prepared = frame
            .prepare(
                &mut view,
                &settings,
                (width, height),
                PrepareContext {
                    device: &device,
                    allocator: &mut allocator,
                    bindless: &bindless,
                    materials: &mut materials,
                    frame_number,
                    skinning: Some(&mut skinning),
                },
            )
            .expect("prepare");
        antialiasing = prepared.antialiasing;
        if shot.animation_rate != 0.0 {
            palettes = palettes_at(shot.animation_rate * accumulated.unwrap_or(0) as f32);
        }
        for &(id, skin, world) in &skinned {
            skinning.set_pose(id, &palettes[skin], &world);
        }
        skinning.update_bounds(&mut geometry, None);
        // The texture LOD bias the frame's materials sample with (texture feedback).
        let lod_bias = materials.texture_lod_bias();
        // SAFETY: the previous frame finished (waited below).
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .expect("reset");
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .expect("begin");
        }
        ring.begin_slot(0);
        let particle_frame = particles.as_mut().and_then(|p| {
            p.prepare(
                &mut ring,
                1.0 / 60.0,
                camera.position,
                -camera.view_from_world.row(2).truncate(),
                camera.far,
            )
        });
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                .expect("textures");
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                .expect("materials");
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                .expect("geometry");
            let skinned_frame = skinning
                .begin_frame(
                    &mut fg,
                    &mut ring,
                    &device,
                    &mut allocator,
                    &bindless,
                    &geo,
                    frame_number,
                )
                .expect("skinning");
            let scene_frame = match game.as_mut() {
                Some(game) => game
                    .scene
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
                    .expect("scene"),
                None => None,
            };
            let ldr = fg.create_texture("ldr", TextureDesc::new_2d(output_format, width, height));
            let particles = particles.as_ref();
            let view_frame = frame
                .add_view_with(
                    &mut fg,
                    &mut FrameContext {
                        device: &device,
                        allocator: &mut allocator,
                        bindless: &bindless,
                        ring: &mut ring,
                        frame_number,
                        dt: 1.0 / 60.0,
                    },
                    &mut view,
                    &SceneInput {
                        geometry: &geo,
                        materials: &mats,
                        draws: match &scene_frame {
                            Some(scene) => SceneDraws::Scene(scene),
                            None => SceneDraws::List(&draws),
                        },
                        skinning: Some(&skinned_frame),
                        light: sun,
                        lights: &lights,
                        environment: environment.as_ref(),
                        reflection_probes: probes.as_ref(),
                    },
                    &camera,
                    ViewTarget {
                        texture: ldr,
                        rect: None,
                        load: LoadOp::DontCare,
                    },
                    |fg, targets| {
                        if let (Some(particles), Some(step)) = (particles, particle_frame) {
                            particles.add_passes(
                                fg,
                                &bindless,
                                &step,
                                &ParticleTarget {
                                    color: targets.color,
                                    depth: targets.depth,
                                    reactive: targets.motion.and_then(|m| m.reactive),
                                    clip_from_world: targets
                                        .motion
                                        .map_or(targets.camera.clip_from_world, |m| {
                                            m.clip_from_world
                                        }),
                                    camera: targets.camera,
                                },
                            );
                        }
                    },
                )
                .expect("frame");
            if let Some((_, _, feedback, _)) = streaming.as_mut() {
                feedback
                    .add_pass(
                        &mut fg,
                        &device,
                        &mut allocator,
                        &bindless,
                        &geo,
                        &mats,
                        &view_frame.passes,
                        view_frame.render,
                        lod_bias,
                        frame_number,
                    )
                    .expect("feedback");
            }
            if capture {
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
                                width,
                                height,
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
            } else {
                fg.add_pass("consume")
                    .read(ldr, Access::TransferRead)
                    .side_effects()
                    .execute(|_| {});
            }
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .expect("graph");
        }
        // SAFETY: as above.
        unsafe {
            d.end_command_buffer(cmd).expect("end");
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                vk::Fence::null(),
            )
            .expect("submit");
            device.wait_idle().expect("wait");
        }
        if uploading {
            continue;
        }
        resident_frames += 1;
        if resident_frames == 1 {
            // Shading variants no frame drew with yet are made on the device's warm-up
            // thread (ADR 0079): timed frames do not share the CPU (and the iGPU's power)
            // with it.
            device.wait_for_pipeline_warm_up();
        }
        if resident_frames == 1
            && let Some(p) = probes.as_ref()
        {
            // Captures made while textures streamed in: again, with all of them.
            p.refresh_all();
        }
        if capture {
            captured += 1;
            let ptr = readback_mem.mapped_ptr().expect("mapped readback");
            // SAFETY: the GPU is idle; the mapping covers the image.
            frames.push(
                unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) }
                    .to_vec(),
            );
        } else if captured > 0 || resident_frames > 2 {
            // Times of the frame before this one's recording (the graph reads them when
            // its slot comes around): every frame after the scene is resident (and its
            // reflection probes captured: captures are not steady-state work).
            let faces = probes.as_ref().map_or(0, |p| p.stats().faces);
            let capturing = faces > 0 || previous_faces > 0;
            previous_faces = faces;
            if resident_frames > 3 && !capturing {
                let mut frame: BTreeMap<&'static str, f64> = BTreeMap::new();
                // The test bed's own passes are not the renderer's.
                for time in graph
                    .gpu_pass_times()
                    .iter()
                    .filter(|t| !matches!(t.name, "read back" | "consume"))
                {
                    *frame.entry(time.name).or_default() += f64::from(time.gpu_us);
                }
                for (name, us) in frame {
                    timings.entry(name).or_default().push(us);
                }
                if let Some(us) = graph.gpu_frame_time() {
                    frame_times.push(f64::from(us));
                }
                timed += 1;
            }
        }
    }

    let rgba8 = frames.last().cloned().unwrap_or_default();
    let adapter = device.adapter().summary();
    let vendor_id = device.adapter().vendor_id;
    let pass_us = timings
        .into_iter()
        .map(|(name, mut v)| (name, median(&mut v)))
        .collect();
    let frame_us = median(&mut frame_times);
    let async_compute = graph.async_compute();

    let mut environment = environment;
    let mut streaming = streaming;
    // SAFETY: the device is idle; everything below was created above and is not used
    // again.
    unsafe {
        if let Some((streamer, _, mut feedback, pack)) = streaming.take() {
            streamer.finish(&mut textures);
            feedback.destroy(&mut allocator);
            drop(feedback);
            let _ = std::fs::remove_file(pack);
        }
        frame.destroy(&device, &mut allocator, &bindless);
        view.destroy(&mut allocator, &bindless);
        if let Some(particles) = particles.as_mut() {
            particles.destroy(&mut allocator);
        }
        if let Some(game) = game.as_mut() {
            game.scene.destroy(&mut allocator);
        }
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        skinning.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        if let Some(environment) = environment.as_mut() {
            environment.destroy(&mut allocator, &bindless);
        }
        if let Some(probes) = probes.as_mut() {
            probes.destroy(&mut allocator, &bindless);
        }
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(command_pool, None);
    }
    // Streamline goes before the device it is attached to (ADR 0063).
    drop((
        game,
        frame,
        view,
        skinning,
        streamline,
        environment,
        probes,
        bindless,
        allocator,
    ));
    let report = instance.validation_report();
    assert_eq!(
        report.errors, 0,
        "validation errors: {:#?}",
        report.first_errors
    );
    Ok(Rendered {
        adapter,
        vendor_id,
        width,
        height,
        rgba8,
        pass_us,
        frame_us,
        async_compute,
        antialiasing,
        frames,
    })
}
