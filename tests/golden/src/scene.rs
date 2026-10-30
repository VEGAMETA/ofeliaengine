//! A test bed: fixed scenes through the whole frame (culling, shadows, forward shading,
//! local lights, sky light, transparency, antialiasing or upscaling, bloom, tone mapping),
//! composed by the frame renderer (`engine_frame`, ADR 0072), rendered on one GPU and read
//! back as 8-bit sRGB, with the GPU time of every pass. The golden-image tests and the
//! performance gate both use it (ADR 0062).

use crate::decal_set::DecalSet;
use engine_anim::{local_to_model, skinning_matrices};
use engine_assets::gltf_models::apply_gltf_models;
use engine_assets::hdr::{SunSearch, decode_hdr, find_sun};
use engine_assets::material_file::{MaterialMap, SlangMaterialTypes, apply_material_map};
use engine_assets::pso_record::PsoRecord;
use engine_assets::{
    AlphaMode as GltfAlpha, CookSettings, CookedScene, DerivedDataCache, TextureFormat as Stored,
    TextureSlot, TextureWrap, cook_scene, import_gltf, write_pack,
};
use engine_brixelizer::{Brixelizer, BrixelizerGi, SdfDebugView, SdfSettings};
use engine_core::half;
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Mat4, Quat, Vec2, Vec3, Vec4, look_at, vulkan_perspective};
use engine_ecs::{Entity, GlobalTransform, Transform, World};
use engine_frame::{
    Antialiasing, Exposure, FrameContext, FrameGeneration, FrameRenderer, GeneratedTarget,
    Placement, PrepareContext, Quality, SceneDraws, SceneInput, UiCanvas, UiRenderer, View,
    ViewCamera, ViewKind, ViewSettings, ViewTarget,
};
use engine_render::ao::AoSettings;
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::budget::{BudgetSettings, GpuBudget};
use engine_render::color_vision::ColorVision;
use engine_render::dof::DofSettings;
use engine_render::environment::{Environment, Sky};
use engine_render::feedback::{FeedbackSettings, TextureFeedback};
use engine_render::forward::ENGINE_SHADER_DIR;
use engine_render::forward::{
    DirectionalLight, DrawItem, LocalLight, LocalShadowSettings, ShadowSettings, Spot, ViewGi,
};
use engine_render::gi::{GiSettings, ProbeGi, ProbeGrid};
use engine_render::gpu_scene::GpuScene;
use engine_render::grading::ColorGrading;
use engine_render::graph::{
    Access, BufferState, FrameGraph, ImportedBuffer, ImportedTexture, LoadOp, RenderGraph, Texture,
    TextureDesc, TextureState,
};
use engine_render::material::{
    AlphaMode, MaterialDesc, MaterialParams, MaterialTable, Subsurface, TextureRef,
};
use engine_render::mesh::{
    GeometryCapacity, GeometryPool, LodIndices, MeshData, MeshId, OwnedMeshData, cube,
};
use engine_render::panorama::{Panorama, PanoramaDesc, PanoramaSun};
use engine_render::particles::{ParticleEmitter, ParticleSystem, ParticleTarget};
use engine_render::post::{HDR_FORMAT, ToneCurve};
use engine_render::rays::{BlasDetail, RayScene, RaySettings};
use engine_render::reflections::{ProbeSettings, ReflectionProbe, ReflectionProbes, SsrSettings};
use engine_render::skinning::{SkinnedMeshData, Skinning};
use engine_render::subsurface::SubsurfaceProfile;
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_render::upload::UploadRing;
use engine_render::vsm::VsmSettings;
use engine_render2d::{Canvas, Quad};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation, Wrap,
};
use engine_runtime::{Camera, CameraView, MeshRenderer, Runtime, look_rotation};
use engine_streaming::material_files::LiveMaterialFiles;
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
    /// A 2D scene (ADR 0125) drawn by the player through a pixel-art camera: its images
    /// painted by the test (`crate::sprites`). Needs no files.
    Sprites,
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
    /// Decals (ADR 0082; they turn the depth prepass on), placed for the procedural
    /// scene: a scorch mark across the corner of the floor and the wall, a wet patch and
    /// a painted arrow on the floor, a ring around a sphere, a metal plate on the wall.
    pub decals: bool,
    /// Global illumination from a volume of probes about this far apart over the scene
    /// (ADR 0088; with `runtime`; skipped on GPUs without ray queries): converged from the
    /// first frame with every texture resident, before the capture.
    pub gi: Option<f32>,
    /// Global illumination from AMD's Brixelizer GI instead (ADR 0090), probe GI's
    /// reference: the distance field over the scene's meshes (built over the first frames,
    /// a cascade a frame) and the radiance cache fed [`BRIXELIZER_WARMUP`] frames with every
    /// texture resident before the capture.
    pub brixelizer_gi: bool,
    /// With `brixelizer_gi`: its warm-up instead of [`BRIXELIZER_WARMUP`] frames.
    pub brixelizer_warmup: Option<u32>,
    /// With `brixelizer_gi`: the captures show this debug view of Brixelizer's distance
    /// field, traced from the camera, instead of the frame (its values as sRGB colors).
    pub brixelizer_view: Option<SdfDebugView>,
    /// The sky's light, its radiance scaled (with `sky`; 0: a black sky, lighting nothing).
    pub sky_intensity: f32,
    /// The sun goes out (its color and ambient term zero) after this many captures: the
    /// next ones show how fast the GI lets go of its light.
    pub sun_off_after: Option<u32>,
    /// The GI shading's irradiance read back with every capture: its mean
    /// ([`Rendered::gi_means`]) and the first capture's image ([`Rendered::gi_image`]).
    pub gi_readback: bool,
    /// A UI layer over the frame (ADR 0091), drawn by `engine_frame::UiRenderer` with the
    /// frame's camera: a HUD (a crosshair, a bar, a frame for a map), a panel in the world
    /// beside the first view's center, a panel attached to the camera (bottom left).
    pub ui: bool,
    /// FSR 3.1's frame generation (ADR 0091; with the UI layer): the captures are the
    /// frames generated before the captured real frames, the UI drawn on them with their
    /// half-way camera. Not with particles, Brixelizer GI or the runtime.
    pub frame_generation: bool,
    /// The procedural scene's floor as a material type of its own (ADR 0092): a checker
    /// from `tests/golden/materials/stripes.slang`, its glTF fallback the plain floor.
    /// Captured once the type's pipelines are made, unless `material_first_sight`.
    pub stripes_floor: bool,
    /// With `stripes_floor`: the capture is the first frame, drawn before the type's
    /// pipelines exist (with its fallback).
    pub material_first_sight: bool,
    /// The procedural scene's bottom row of spheres as material types of the shading
    /// models (ADR 0092), left to right unlit, clear coat, cloth, hair and toon, from
    /// `tests/golden/materials/models.slang`; the top row stays glTF's (lit). Captured
    /// once the types' pipelines are made.
    pub shading_models: bool,
    /// Culling and drawing by meshlet (ADR 0085): the meshes with meshlets, in the view
    /// when it has the depth prepass and in the sun's shadow cascades.
    pub meshlets: bool,
    /// The meshes' detail levels made by the cook (ADR 0053), as a game's pack has them
    /// (`CookSettings::lods` is on by default): the view and the cascades then draw the
    /// levels their errors allow. Off, every mesh is drawn whole.
    pub lods: bool,
    /// The sun's cascades kept across frames (`ViewSettings::cached_shadows`, ADR 0093; on
    /// by default as there): a still camera draws them once.
    pub cached_shadows: bool,
    /// The sun's virtual shadow pages (`ViewSettings::virtual_shadows`, ADR 0093 step 3;
    /// off by default, as there): opaque surfaces take the sun's shadow from them.
    pub virtual_shadows: Option<VsmSettings>,
    /// Sponza's curtains (its materials 14 to 19) as cloth: material files
    /// (`tests/golden/materials/fabric.slang` with the curtains' own textures) applied by a
    /// material map when cooking, the scene written to a pack (version 5, the types'
    /// SPIR-V in it) and loaded back, the types drawn from the pack's SPIR-V (ADR 0092).
    pub fabric_curtains: bool,
    /// Color grading (`ViewSettings::grading`, ADR 0094; neutral by default).
    pub grading: ColorGrading,
    /// A color vision deficiency simulated or corrected (`ViewSettings::color_vision`, ADR
    /// 0097; off by default).
    pub color_vision: ColorVision,
    /// Depth of field (`ViewSettings::depth_of_field`, ADR 0095; off by default).
    pub depth_of_field: Option<DofSettings>,
    /// Gameplay or cinematic with its motion blur (`ViewSettings::kind`, ADRs 0067 and
    /// 0096; gameplay by default). Not on the runtime path: there the camera entity has
    /// its own.
    pub kind: ViewKind,
    /// Rendered by the player (`engine_app`, ADR 0099; [`crate::player`]): the scene as a
    /// pack it streams, the sun, lights, probes and GI volume as entities, offscreen. The
    /// fields it does not take fail the shot.
    pub player: bool,
    /// A video (ADR 0100; player shots only): `engine_video`'s test clip paused at its
    /// picture of 1.0 s, on a screen before the camera (a material's emission) and in the
    /// top left corner of the UI (a sprite of its texture).
    pub video: bool,
}

/// Frames Brixelizer GI runs with every texture resident before a shot is captured or
/// timed: its distance field's cascades are built in the first eight (the GI waits for
/// them), and its caches, fed by last frame's lit image, settle over the rest (by half
/// every 48 frames or so).
pub const BRIXELIZER_WARMUP: u32 = 128;

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
            decals: false,
            meshlets: false,
            lods: false,
            cached_shadows: true,
            virtual_shadows: None,
            gi: None,
            brixelizer_gi: false,
            brixelizer_warmup: None,
            brixelizer_view: None,
            sky_intensity: 1.0,
            sun_off_after: None,
            gi_readback: false,
            ui: false,
            frame_generation: false,
            stripes_floor: false,
            material_first_sight: false,
            shading_models: false,
            fabric_curtains: false,
            grading: ColorGrading::default(),
            color_vision: ColorVision::default(),
            depth_of_field: None,
            kind: ViewKind::Gameplay,
            player: false,
            video: false,
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
    /// GPU time of each pass over the frames before the first timed one, µs, by pass name:
    /// the scene's upload, the GI's convergence or warm-up, the probes' captures.
    pub startup_us: BTreeMap<&'static str, f64>,
    /// Whether async passes ran on the async compute queue.
    pub async_compute: bool,
    /// The antialiasing used (the shot's, or TAAU when its upscaler was unavailable).
    pub antialiasing: Antialiasing,
    /// Every captured frame ([`Shot::captures`]), in order, as `rgba8`.
    pub frames: Vec<Vec<u8>>,
    /// GPU memory the GI keeps between frames, bytes: probe GI's volume and its ray
    /// scene's acceleration structures, or Brixelizer GI's distance field (the SDK's
    /// resources, its scratch included) and the view's GI; 0 without GI.
    pub gi_bytes: u64,
    /// `gi_bytes` by part: probe GI's volume and ray scene, or Brixelizer GI's distance
    /// field and view.
    pub gi_parts: Vec<(&'static str, u64)>,
    /// The frames from the scene being resident to the GI being ready, and the GPU time
    /// the GI's passes took over them, µs (probe GI: the volume's updates and the ray
    /// scene's builds; Brixelizer GI: the distance field's builds and the warm-up).
    pub gi_settle: (u32, f64),
    /// With [`Shot::gi_readback`]: the GI shading's mean irradiance (rgb) over the pixels
    /// showing a surface, for every capture.
    pub gi_means: Vec<[f32; 3]>,
    /// With [`Shot::gi_readback`]: the first capture's GI irradiance (rgb, the surface's
    /// distance; 0 for the sky) at the GI's size (probe GI's: its resolve's scale of the
    /// render size): width, height, rows top to bottom.
    pub gi_image: Option<(u32, u32, Vec<[f32; 4]>)>,
    /// With [`Shot::ui`]: where the UI's parts are in the last capture, as its camera saw
    /// them (pixel rectangles `[x0, y0, x1, y1]`, clamped to the image): the HUD's bar and
    /// map frame, the panel in the world, the panel on the camera.
    pub ui_regions: Vec<(&'static str, [u32; 4])>,
}

/// The golden shots' PSO record (ADR 0092): the entry points their material types draw,
/// which their cooks compile.
pub fn golden_pso_record() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("pso.ron")
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

pub(crate) struct Loaded {
    pub(crate) cooked: Option<CookedScene>,
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

/// The shading models' material files in `tests/golden/materials`, in the bottom row's
/// order.
const SHADING_MODEL_FILES: [&str; 5] = ["unlit", "clearcoat", "cloth", "hair", "toon"];

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

/// Sponza on a summer day with everything daylight uses: the physically based sky and
/// atmosphere (the sun 75° up, high enough to reach the courtyard's floor, its light
/// through the air), light height fog, probe GI (through the runtime), ambient
/// occlusion, the sun's contact shadows, reflection probes and screen-space reflections,
/// temporal antialiasing at the output size, bloom. Exposed for the shade: GI leaves the
/// courtyard a tenth of the sky's light, the sunlit floor rolls off in the tone curve.
pub fn sponza_day(name: &'static str, size: [u32; 2]) -> Shot {
    Shot {
        size,
        sun: sun_at(75.0),
        exposure: 8.0,
        fog: Some(HeightFog {
            density: 0.004,
            base_height: 0.0,
            falloff: 0.3,
            ..HeightFog::default()
        }),
        ..sponza_everything(name, size)
    }
}

/// Sponza at night with everything on: the sun 6° below the horizon (the sky's last
/// blue), a bonfire in the courtyard (sparks added, smoke blended, its warm light with
/// shadows), warm lights along the hall and two cold ones from above, all with shadows;
/// height fog, probe GI, ambient occlusion, contact shadows, reflection probes and
/// screen-space reflections, temporal antialiasing, bloom; exposed for the firelight.
pub fn sponza_night(name: &'static str, size: [u32; 2]) -> Shot {
    let mut lights = sponza_lights();
    lights.push(LocalLight {
        position: Vec3::new(0.0, 0.6, 0.0),
        range: 14.0,
        color: Vec3::new(60.0, 28.0, 8.0),
        spot: None,
        shadow: true,
    });
    Shot {
        sun: sun_at(-6.0),
        lights,
        local_shadows: true,
        particles: true,
        fog: Some(HeightFog {
            density: 0.04,
            base_height: 0.0,
            falloff: 0.3,
            ..HeightFog::default()
        }),
        exposure: 4.0,
        ..sponza_everything(name, size)
    }
}

/// The features [`sponza_day`] and [`sponza_night`] share.
fn sponza_everything(name: &'static str, size: [u32; 2]) -> Shot {
    Shot {
        size,
        atmosphere: Some(Atmosphere::earth()),
        runtime: true,
        gi: Some(1.0),
        ao: Some(AoSettings::default()),
        contact_shadows: true,
        // Four probes along the courtyard, each across its whole width: boxes split
        // across the width (a grid of 2 along z) send the floor's reflections into the
        // open sky where a box's ceiling is, leaving bright patches.
        probe_grid: Some([4, 1, 1]),
        ssr: Some(SsrSettings::default()),
        antialiasing: Antialiasing::TAAU,
        quality: Quality::Native,
        ..Shot::new(name, Source::Gltf(PathBuf::from("Sponza/Sponza.gltf")))
    }
}

/// The sun `elevation` degrees up (below the horizon when negative), behind the default
/// view's camera (−x, a little +z: along the courtyard, lighting the far end and one
/// side's arcade, with no haze lit up ahead). Its color is the light outside the
/// atmosphere, which colors and dims it.
pub(crate) fn sun_at(elevation: f32) -> DirectionalLight {
    let e = elevation.to_radians();
    DirectionalLight {
        direction: -Vec3::new(-0.832 * e.cos(), e.sin(), 0.555 * e.cos()).normalize(),
        color: Vec3::splat(4.0),
        ambient: Vec3::ZERO,
    }
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

/// Sponza's curtains as fabric (`Shot::fabric_curtains`): a material map and material
/// files written for the scene in `dir`, in a folder of their own.
fn fabric_map(dir: &Path) -> MaterialMap {
    let out = std::env::temp_dir().join(format!("engine_golden_fabric_{}", std::process::id()));
    std::fs::create_dir_all(&out).expect("fabric folder");
    let module = Path::new(env!("CARGO_MANIFEST_DIR")).join("materials/fabric.slang");
    let slash = |p: &Path| p.to_string_lossy().replace('\\', "/");
    // Material, its color and normal map, a sheen tinted like the cloth.
    let curtains = [
        (
            14,
            "4675343432951571524.jpg",
            "7056944414013900257.jpg",
            "(0.1, 0.2, 0.1)",
        ),
        (
            15,
            "2775690330959970771.jpg",
            "2374361008830720677.jpg",
            "(0.1, 0.12, 0.25)",
        ),
        (
            16,
            "2185409758123873465.jpg",
            "332936164838540657.jpg",
            "(0.25, 0.08, 0.08)",
        ),
        (
            17,
            "17876391417123941155.jpg",
            "6593109234861095314.jpg",
            "(0.1, 0.12, 0.25)",
        ),
        (
            18,
            "11474523244911310074.jpg",
            "4601176305987539675.jpg",
            "(0.25, 0.08, 0.08)",
        ),
        (
            19,
            "9288698199695299068.jpg",
            "4910669866631290573.jpg",
            "(0.1, 0.2, 0.1)",
        ),
    ];
    // Version 2 `.material` files (ADR 0120), outside any project: absolute paths, each file
    // its own id; the look the replaced glTF material's.
    let quote = |p: &Path| engine_scene::text::quote(&slash(p));
    let mut map = String::from("{\n");
    for (material, color, normal, sheen) in curtains {
        let file = out.join(format!("fabric_{material}.material"));
        let id = || engine_scene::AssetId::new();
        let text = format!(
            "[material version=2 id={}]\n\nshader = asset({}, {})\ntype = \"FabricMaterial\"\nnumbers[\"sheen_roughness\"] = 0.5\nnumbers[\"roughness\"] = 0.9\nvectors[\"tint\"] = [1, 1, 1, 1]\nvectors[\"sheen\"] = [{}]\ntextures[\"albedo\"].image = asset({}, {})\ntextures[\"albedo\"].encoding = Color\ntextures[\"normal\"].image = asset({}, {})\ntextures[\"normal\"].encoding = Normal\nlook = Replaced\n",
            id(),
            quote(&module),
            id(),
            sheen.trim_matches(|c| c == '(' || c == ')'),
            quote(&dir.join(color)),
            id(),
            quote(&dir.join(normal)),
            id(),
        );
        std::fs::write(&file, text).expect("fabric file");
        map.push_str(&format!("    \"#{material}\": \"{}\",\n", slash(&file)));
    }
    map.push('}');
    MaterialMap::parse(&map, &out).expect("fabric map")
}

pub(crate) fn load(
    source: &Source,
    subsurface: bool,
    glossy_floor: bool,
    fabric: bool,
    lods: bool,
) -> Result<Loaded, Skip> {
    match source {
        Source::Procedural => Ok(Loaded {
            cooked: None,
            procedural: procedural(subsurface, glossy_floor),
        }),
        // Drawn by `crate::sprites`, not loaded here.
        Source::Sprites => Ok(Loaded {
            cooked: None,
            procedural: Vec::new(),
        }),
        Source::Gltf(path) => {
            let path = test_assets().join(path);
            let Ok(bytes) = std::fs::read(&path) else {
                return Err(Skip::NoScene(path));
            };
            let dir = path.parent();
            let mut scene = import_gltf(&bytes, dir).map_err(|_| Skip::NoScene(path.clone()))?;
            // Material files (the fabric), then glTF's unlit, clear coat and sheen as the
            // engine's built-in types (ADR 0092), compiled with Slang.
            let layered = scene.materials.iter().any(|m| m.layers.any());
            if fabric || layered {
                // Cooked as a game's would be: only the entry points the shots' PSO record
                // names (ADR 0092), unless the shots are recording one.
                let mut builder = SlangMaterialTypes::new(Path::new(ENGINE_SHADER_DIR))
                    .expect("Slang for material types");
                if std::env::var_os("ENGINE_GOLDEN_PSO_RECORD").is_none() {
                    builder = builder.with_pso_record(
                        PsoRecord::load(&golden_pso_record()).expect("tests/golden/pso.ron"),
                    );
                }
                if fabric {
                    let map = fabric_map(dir.expect("the scene's folder"));
                    apply_material_map(&mut scene, &map, Path::new(""), &mut builder)
                        .expect("material map");
                }
                apply_gltf_models(&mut scene, &mut builder).expect("glTF's material extensions");
            }
            let jobs = ThreadPool::builder()
                .build()
                .map_err(|_| Skip::NoScene(path.clone()))?;
            let cache = DerivedDataCache::new(test_assets().join(".ddc"))
                .map_err(|_| Skip::NoScene(path.clone()))?;
            let settings = CookSettings {
                lods,
                compression_level: 0,
                block_compression: true,
                ..CookSettings::default()
            };
            let (mut cooked, _) = cook_scene(&scene, dir, Some(&cache), &jobs, &settings)
                .map_err(|_| Skip::NoScene(path.clone()))?;
            if fabric {
                // Through a pack and back: version 5, the types' SPIR-V in it.
                let pack = std::env::temp_dir()
                    .join(format!("engine_golden_fabric_{}.pack", std::process::id()));
                write_pack(&pack, &cooked, &jobs, 1).expect("pack");
                cooked = engine_assets::load_pack(&pack, &jobs, 8).expect("pack").0;
                let _ = std::fs::remove_file(&pack);
            }
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
pub(crate) fn default_view(min: Vec3, max: Vec3) -> (Vec3, Vec3) {
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

pub(crate) fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// The UI layer of [`Shot::ui`] (ADR 0091).
struct UiLayer {
    hud: Canvas,
    panel: Canvas,
    visor: Canvas,
    world_from_panel: Mat4,
    view_from_visor: Mat4,
}

impl UiLayer {
    /// The HUD for an output of `size`; the panel in the world 30 % of the way from `eye`
    /// to `target`, right of the view's center, facing the camera.
    fn new(eye: Vec3, target: Vec3, size: (u32, u32)) -> Self {
        let (w, h) = (size.0 as f32, size.1 as f32);
        // Pixels of a 1920-wide HUD, scaled to the output.
        let px = w / 1920.0;
        let mut hud = Canvas::new();
        let center = Vec2::new(w * 0.5, h * 0.5);
        // The crosshair at least 2 pixels thick at any size.
        let (long, thick) = ((24.0 * px).max(10.0), (2.0 * px).max(2.0));
        hud.draw(&Quad::solid(center, Vec2::new(long, thick)).color(Vec4::ONE));
        hud.draw(&Quad::solid(center, Vec2::new(thick, long)).color(Vec4::ONE));
        hud.draw(
            &Quad::rounded_rect(
                Vec2::new(200.0 * px, h - 60.0 * px),
                Vec2::new(300.0, 24.0) * px,
                6.0 * px,
            )
            .color(Vec4::new(0.1, 0.1, 0.1, 0.6)),
        );
        hud.draw(
            &Quad::rounded_rect(
                Vec2::new(170.0 * px, h - 60.0 * px),
                Vec2::new(232.0, 16.0) * px,
                4.0 * px,
            )
            .color(Vec4::new(0.85, 0.15, 0.1, 1.0)),
        );
        hud.draw(
            &Quad::rounded_rect(
                Vec2::new(w - 140.0 * px, 140.0 * px),
                Vec2::new(220.0, 220.0) * px,
                10.0 * px,
            )
            .outline(4.0 * px)
            .color(Vec4::new(0.9, 0.9, 0.9, 0.8)),
        );
        hud.prepare();
        let mut panel = Canvas::new();
        panel.draw(
            &Quad::rounded_rect(Vec2::new(150.0, 75.0), Vec2::new(300.0, 150.0), 12.0)
                .color(Vec4::new(0.05, 0.3, 0.6, 0.85)),
        );
        for row in 0..4 {
            panel.draw(
                &Quad::solid(
                    Vec2::new(150.0, 30.0 + 30.0 * row as f32),
                    Vec2::new(240.0 - 40.0 * row as f32, 10.0),
                )
                .color(Vec4::new(0.9, 0.95, 1.0, 1.0)),
            );
        }
        panel.prepare();
        let mut visor = Canvas::new();
        visor.draw(
            &Quad::rounded_rect(Vec2::new(50.0, 25.0), Vec2::new(100.0, 50.0), 6.0)
                .color(Vec4::new(0.1, 0.8, 0.3, 0.7)),
        );
        visor.prepare();
        let rotation = look_rotation(eye, target, Vec3::Y);
        let distance = (target - eye).length() * 0.3;
        let center = eye + rotation * Vec3::new(distance * 0.35, distance * 0.05, -distance);
        // A canvas unit: a 300-unit panel spans about a sixth of the view's width.
        let unit = distance * 0.0012;
        Self {
            hud,
            panel,
            visor,
            world_from_panel: Mat4::from_rotation_translation(rotation, center)
                * Mat4::from_scale(Vec3::new(unit, -unit, 1.0))
                * Mat4::from_translation(Vec3::new(-150.0, -75.0, 0.0)),
            view_from_visor: Mat4::from_scale_rotation_translation(
                Vec3::new(0.0012, -0.0012, 1.0),
                Quat::IDENTITY,
                Vec3::new(-0.42, -0.2, -0.6),
            ),
        }
    }

    /// Pixel rectangles of the UI's parts as `camera` sees them in an output of `size`
    /// ([`Rendered::ui_regions`]).
    fn regions(&self, camera: &ViewCamera, size: (u32, u32)) -> Vec<(&'static str, [u32; 4])> {
        let (w, h) = (size.0 as f32, size.1 as f32);
        let px = w / 1920.0;
        let clamp = |min: Vec2, max: Vec2| {
            [
                min.x.clamp(0.0, w) as u32,
                min.y.clamp(0.0, h) as u32,
                max.x.clamp(0.0, w).ceil() as u32,
                max.y.clamp(0.0, h).ceil() as u32,
            ]
        };
        let rect = |center: Vec2, extent: Vec2| clamp(center - extent * 0.5, center + extent * 0.5);
        // A canvas's corners through `clip_from_canvas` (Vulkan's clip space: y down).
        let projected = |clip_from_canvas: Mat4, extent: Vec2| {
            let (mut min, mut max) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
            for corner in [
                Vec2::ZERO,
                Vec2::new(extent.x, 0.0),
                Vec2::new(0.0, extent.y),
                extent,
            ] {
                let clip = clip_from_canvas * Vec4::new(corner.x, corner.y, 0.0, 1.0);
                let ndc = Vec2::new(clip.x, clip.y) / clip.w.max(1e-6);
                let pixel = (ndc + Vec2::ONE) * 0.5 * Vec2::new(w, h);
                min = min.min(pixel);
                max = max.max(pixel);
            }
            clamp(min, max)
        };
        let aspect = w / h;
        vec![
            (
                "HUD bar",
                rect(
                    Vec2::new(200.0 * px, h - 60.0 * px),
                    Vec2::new(300.0, 24.0) * px,
                ),
            ),
            (
                "HUD map frame",
                rect(
                    Vec2::new(w - 140.0 * px, 140.0 * px),
                    Vec2::new(220.0, 220.0) * px,
                ),
            ),
            (
                "world panel",
                projected(
                    camera.clip_from_world(aspect) * self.world_from_panel,
                    Vec2::new(300.0, 150.0),
                ),
            ),
            (
                "camera panel",
                projected(
                    vulkan_perspective(camera.fov_y, aspect, camera.near, camera.far)
                        * self.view_from_visor,
                    Vec2::new(100.0, 50.0),
                ),
            ),
        ]
    }

    fn canvases(&self) -> [UiCanvas<'_>; 3] {
        [
            UiCanvas {
                canvas: &self.hud,
                placement: Placement::Screen,
            },
            UiCanvas {
                canvas: &self.panel,
                placement: Placement::World(self.world_from_panel),
            },
            UiCanvas {
                canvas: &self.visor,
                placement: Placement::Camera(self.view_from_visor),
            },
        ]
    }
}

/// The images of a [`Shot::frame_generation`] shot: the real frame (the output's sRGB
/// format, also viewed as its UNORM twin, which frame interpolation reads) and the frame
/// generated before it (created as the UNORM twin for storage, viewed as sRGB for the UI).
struct GenerationImages {
    images: [(vk::Image, vk::ImageView, Allocation); 2],
    format: vk::Format,
    raw: vk::Format,
    size: (u32, u32),
}

impl GenerationImages {
    fn new(
        device: &Device,
        allocator: &mut Allocator,
        format: vk::Format,
        size: (u32, u32),
    ) -> Self {
        let raw = match format {
            vk::Format::R8G8B8A8_SRGB => vk::Format::R8G8B8A8_UNORM,
            vk::Format::B8G8R8A8_SRGB => vk::Format::B8G8R8A8_UNORM,
            other => other,
        };
        let make = |allocator: &mut Allocator, generated: bool| {
            let formats = [format, raw];
            let mut list = vk::ImageFormatListCreateInfo::default().view_formats(&formats);
            let mut usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST;
            if generated {
                usage |= vk::ImageUsageFlags::STORAGE;
            }
            let info = vk::ImageCreateInfo::default()
                .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
                .push_next(&mut list)
                .image_type(vk::ImageType::TYPE_2D)
                .format(if generated { raw } else { format })
                .extent(vk::Extent3D {
                    width: size.0,
                    height: size.1,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage);
            let (image, memory) = allocator
                .create_image(&info, MemoryLocation::Device)
                .expect("frame generation image");
            // The sRGB view draws and samples: an sRGB format allows no storage.
            let mut view_usage = vk::ImageViewUsageCreateInfo::default()
                .usage(usage & !vk::ImageUsageFlags::STORAGE);
            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(engine_rhi::color_range())
                .push_next(&mut view_usage);
            // SAFETY: a view of the whole new image in a format its creation allows.
            let view = unsafe { device.raw().create_image_view(&view_info, None) }
                .expect("frame generation view");
            (image, view, memory)
        };
        let real = make(allocator, false);
        let generated = make(allocator, true);
        Self {
            images: [real, generated],
            format,
            raw,
            size,
        }
    }

    /// Imports both images into the frame: the real frame's target and the generated
    /// frame's (their contents are each frame's own).
    fn import(&self, fg: &mut FrameGraph<'_>) -> (Texture, GeneratedTarget) {
        let mut import =
            |name: &'static str, (image, view, _): &(vk::Image, vk::ImageView, Allocation)| {
                fg.import_texture(
                    name,
                    ImportedTexture {
                        image: *image,
                        view: *view,
                        desc: TextureDesc::new_2d(self.format, self.size.0, self.size.1),
                        initial: TextureState::undefined(),
                        final_state: Some(TextureState::transfer_source()),
                        sampled_index: None,
                        storage_index: None,
                    },
                )
            };
        let real = import("real frame", &self.images[0]);
        let generated = import("generated frame", &self.images[1]);
        (
            real,
            GeneratedTarget {
                texture: generated,
                format: self.raw,
                source_format: self.raw,
            },
        )
    }

    /// # Safety
    ///
    /// The GPU is done with the images.
    unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        for (image, view, memory) in self.images {
            // SAFETY: guaranteed by the caller.
            unsafe {
                device.raw().destroy_image_view(view, None);
                allocator.destroy_image(image, memory);
            }
        }
    }
}

/// Renders `shot`, after every texture is resident, and times `timed_frames` more frames.
///
/// # Panics
///
/// On Vulkan errors and, with validation, on validation errors.
#[allow(clippy::too_many_lines)]
pub fn render(shot: &Shot, options: RenderOptions) -> Result<Rendered, Skip> {
    if matches!(shot.source, Source::Sprites) {
        return crate::sprites::render(shot, options);
    }
    if shot.player {
        return crate::player::render(shot, options);
    }
    assert!(
        !shot.video,
        "{}: videos are drawn on the player's path",
        shot.name
    );
    let loaded = load(
        &shot.source,
        shot.subsurface,
        shot.glossy_floor,
        shot.fabric_curtains,
        shot.lods,
    )?;
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
    if shot.gi.is_some() && device.ray_query().is_none() {
        return Err(Skip::Unsupported(String::from(
            "probe GI needs ray queries",
        )));
    }
    assert!(
        shot.gi.is_none() || shot.runtime,
        "{}: probe GI traces the runtime's GPU scene",
        shot.name
    );
    assert!(
        shot.gi.is_none() || !shot.brixelizer_gi,
        "{}: one GI or the other",
        shot.name
    );
    let renderer = frame.forward_mut();
    renderer
        .set_global_illumination(&device, &bindless, shot.gi.is_some() || shot.brixelizer_gi)
        .expect("global illumination");
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
    if shot.decals {
        renderer
            .set_decals(&device, &bindless, true)
            .expect("decals");
    }
    if shot.meshlets {
        renderer
            .set_meshlet_culling(&device, &bindless, true)
            .expect("meshlet culling");
    }
    let mut decal_set = shot
        .decals
        .then(|| DecalSet::new(&device, &mut allocator, &bindless));
    let mut lights = shot.lights.clone();
    for light in lights.iter_mut() {
        light.shadow = shot.local_shadows;
    }
    let mut sun = shot.sun;
    let environment = shot.sky.then(|| {
        let mut environment =
            Environment::new(&device, &mut allocator, &bindless, Sky::default()).expect("sky");
        environment.set_intensity(shot.sky_intensity);
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
    // A cooked scene with materials of types (ADR 0092).
    let mut typed_scene = false;
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
        // With the cook's detail levels when the shot has them (`Shot::lods`).
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
                        meshlets: engine_streaming::gpu_meshlets(&l.meshlets),
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
                            meshlets: engine_streaming::gpu_meshlets(&m.meshlets),
                        },
                        &levels,
                    )
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
        // Material types (ADR 0092): registered with the pack's SPIR-V.
        let type_ids =
            engine_streaming::register_material_types(frame.forward_mut(), &cooked.material_types);
        let material_ids: Vec<_> = material_descs(&cooked, &ids)
            .into_iter()
            .enumerate()
            .map(|(k, m)| {
                let typed = cooked
                    .typed_materials
                    .iter()
                    .find(|t| t.material as usize == k);
                match typed {
                    Some(t) => materials.add_typed(
                        type_ids[t.ty as usize],
                        m,
                        Some(MaterialParams {
                            bytes: t.params.clone(),
                            textures: t
                                .textures
                                .iter()
                                .filter_map(|&(offset, slot)| {
                                    Some((
                                        offset,
                                        TextureRef {
                                            texture: ids[slot.texture as usize]?,
                                            wrap: slot.wrap.map(wrap),
                                        },
                                    ))
                                })
                                .collect(),
                        }),
                    ),
                    None => materials.add(m),
                }
            })
            .collect();
        typed_scene = !cooked.typed_materials.is_empty();
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
                            meshlets: &[],
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
    // Material types (ADR 0092) from material files: the floor (the first procedural item)
    // as a checker, the bottom row of spheres (items 3, 5, ..., 11) as the shading models.
    let stripes = shot.stripes_floor && !loaded.procedural.is_empty();
    let models = shot.shading_models && loaded.procedural.len() > 11;
    let typed = stripes || models || typed_scene;
    // The golden test bed is the materials' project (ADR 0120).
    let project = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut live = LiveMaterialFiles::new(Path::new(ENGINE_SHADER_DIR), project);
    let files = project.join("materials");
    for (k, (mesh, material, world)) in loaded.procedural.iter().enumerate() {
        let id = geometry.add_mesh(&mesh.as_data()).expect("mesh");
        let file = if stripes && k == 0 {
            Some(String::from("stripes"))
        } else if models && (3..=11).contains(&k) && k % 2 == 1 {
            Some(String::from(SHADING_MODEL_FILES[(k - 3) / 2]))
        } else {
            None
        };
        let material = match file {
            Some(name) => live
                .load(
                    &files.join(format!("{name}.material")),
                    *material,
                    frame.forward_mut(),
                    &mut materials,
                    &mut |_, _| None,
                )
                .unwrap_or_else(|e| panic!("{e}")),
            None => materials.add(*material),
        };
        draws.push(DrawItem {
            mesh: id,
            material,
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
        cached_shadows: shot.cached_shadows,
        virtual_shadows: shot.virtual_shadows,
        exposure: Exposure::Fixed(shot.exposure),
        curve: shot.curve,
        grading: shot.grading,
        color_vision: shot.color_vision,
        depth_of_field: shot.depth_of_field,
        kind: shot.kind,
        bloom: shot.bloom,
        clear: Some([0.02, 0.02, 0.05, 1.0]),
        screen_space_reflections: shot.ssr.filter(|_| shot.sky),
        frame_generation: if shot.frame_generation {
            FrameGeneration::Fsr
        } else {
            FrameGeneration::Off
        },
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
    assert!(
        !shot.frame_generation || !(shot.particles || shot.brixelizer_gi || shot.runtime),
        "{}: frame generation shots hook nothing into the scene and use no runtime",
        shot.name
    );
    // The UI layer (ADR 0091), and with frame generation the images of the real frame and
    // of the one generated before it.
    let ui_layer =
        (shot.ui || shot.frame_generation).then(|| UiLayer::new(eye, target, (width, height)));
    let canvases = ui_layer.as_ref().map(UiLayer::canvases);
    let mut ui = ui_layer.as_ref().map(|_| {
        UiRenderer::new(&device, &mut allocator, &bindless, output_format).expect("UI renderer")
    });
    let generation = shot
        .frame_generation
        .then(|| GenerationImages::new(&device, &mut allocator, output_format, (width, height)));
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
    // Probe GI (ADR 0088): the GPU scene's coarsest levels for ray queries, every
    // structure built in the first frame; the volume over the scene with the device's
    // defaults: converged an update a frame (all eleven in one submission take about a
    // second on the two-CU Radeon, and beside other tests' work the driver resets the GPU).
    let mut gi_rays = shot.gi.map(|_| {
        RayScene::new(
            &device,
            &bindless,
            RaySettings {
                detail: BlasDetail::Coarsest,
                ..RaySettings::default()
            },
            1,
        )
        .expect("ray scene")
    });
    let mut gi_volume = shot.gi.map(|spacing| {
        ProbeGi::new(
            &device,
            &mut allocator,
            &bindless,
            ProbeGrid::covering(min, max, spacing, 16_384),
            GiSettings::for_device(&device),
        )
        .expect("probe GI")
    });
    // Brixelizer GI (ADR 0090): the distance field over every draw (static), with the
    // SDK sample's settings; the view's GI is made for the render size on the first frame.
    let mut brixelizer = shot.brixelizer_gi.then(|| {
        let mut sdf =
            Brixelizer::new(&device, &mut allocator, SdfSettings::default()).expect("Brixelizer");
        let instances: Vec<(MeshId, Mat4)> = draws
            .iter()
            .map(|d| (d.mesh, d.world_from_object))
            .collect();
        sdf.set_instances(&geometry, &instances)
            .expect("Brixelizer instances");
        sdf
    });
    let mut brixelizer_gi: Option<BrixelizerGi> = None;
    let mut brixelizer_frames = 0u32;
    let mut gi_settle = (0u32, 0.0f64);
    // With `gi_readback`: the GI's irradiance at the render size, read back with each
    // capture (made on the first capture).
    let mut gi_readback: Option<(vk::Buffer, Allocation, (u32, u32))> = None;
    let mut gi_means = Vec::new();
    let mut gi_image = None;
    let mut ui_regions = Vec::new();

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
    let mut startup_us: BTreeMap<&'static str, f64> = BTreeMap::new();
    let mut frame_times = Vec::new();
    let mut frame_number = 0u64;
    let mut timed = 0;
    let mut captured = 0u32;
    let captures = shot.captures.max(1);
    let mut frames = Vec::with_capacity(captures as usize);
    let mut resident_frames = 0;
    let mut previous_faces = 0;
    let mut converging_before = [true; 2];
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
        // With GI, once the volume converged (Brixelizer GI: once warmed up).
        let warmup = shot.brixelizer_warmup.unwrap_or(BRIXELIZER_WARMUP);
        let gi_ready = gi_volume.as_ref().is_none_or(ProbeGi::is_converged)
            && (brixelizer.is_none() || brixelizer_frames >= warmup);
        // With a material type: once its pipelines are made (the frames before drew its
        // fallback), or at once for its first sight.
        let types_ready =
            !typed || shot.material_first_sight || frame.forward().material_types_pending() == 0;
        let capture = !uploading
            && captured < captures
            && resident_frames >= capture_at
            && probes_ready
            && gi_ready
            && types_ready;
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
                    orthographic: None,
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
        if let Some(sdf) = brixelizer.as_ref()
            && brixelizer_gi
                .as_ref()
                .is_none_or(|gi| gi.size() != prepared.render)
        {
            if let Some(mut old) = brixelizer_gi.take() {
                // SAFETY: the GPU is idle (every frame is waited for).
                unsafe { old.destroy(&device, &mut allocator, &bindless, frame_number) };
            }
            brixelizer_gi = Some(
                BrixelizerGi::new(
                    &device,
                    &mut allocator,
                    &bindless,
                    sdf,
                    prepared.render.0,
                    prepared.render.1,
                    engine_brixelizer::GiSettings::for_device(&device),
                )
                .expect("Brixelizer GI"),
            );
        }
        if brixelizer.is_some() && !uploading {
            brixelizer_frames += 1;
        }
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
            // The volume converges once every texture is resident (its rays see them).
            let ray_frame = match (gi_rays.as_mut(), &scene_frame) {
                (Some(rays), Some(scene)) => rays
                    .add_passes(
                        &mut fg,
                        &device,
                        &mut allocator,
                        &mut ring,
                        &bindless,
                        &geo,
                        scene,
                        frame_number,
                    )
                    .expect("ray scene"),
                _ => None,
            };
            let (sdf_frame, sdf_view) = match (brixelizer.as_ref(), shot.brixelizer_view) {
                (Some(sdf), Some(debug)) => {
                    let clip_from_world = vulkan_perspective(
                        camera.fov_y,
                        width as f32 / height as f32,
                        camera.near,
                        camera.far,
                    ) * camera.view_from_world;
                    let (sdf_frame, texture) = sdf.add_update_pass_with_debug(
                        &mut fg,
                        &geo,
                        camera.position,
                        engine_render::forward::Camera {
                            clip_from_world,
                            position: camera.position,
                        },
                        debug,
                        (width, height),
                    );
                    (Some(sdf_frame), texture)
                }
                (Some(sdf), None) => (
                    Some(sdf.add_update_pass(&mut fg, &geo, camera.position)),
                    None,
                ),
                _ => (None, None),
            };
            if let (Some(gi), Some(sdf_frame)) = (brixelizer_gi.as_ref(), sdf_frame.as_ref()) {
                gi.begin_frame(sdf_frame);
            }
            let gi = match brixelizer_gi.as_ref() {
                Some(gi) => Some(ViewGi::Screen(gi)),
                None => gi_volume.as_ref().map(|volume| ViewGi::Probes {
                    volume: volume.begin_frame(&mut fg),
                    rays: ray_frame.filter(|_| !uploading),
                }),
            };
            let screen_gi = brixelizer_gi.as_ref();
            // With frame generation the real frame and the generated one go into images of
            // their own (frame interpolation views them as UNORM: they allow both views).
            let (ldr, generated) = match &generation {
                Some(images) => {
                    let (real, generated) = images.import(&mut fg);
                    (real, Some(generated))
                }
                None => (
                    fg.create_texture("ldr", TextureDesc::new_2d(output_format, width, height)),
                    None,
                ),
            };
            if let Some(set) = &decal_set {
                set.upload(&mut fg, &mut ring);
            }
            let particles = particles.as_ref();
            let mut context = FrameContext {
                device: &device,
                allocator: &mut allocator,
                bindless: &bindless,
                ring: &mut ring,
                frame_number,
                dt: 1.0 / 60.0,
            };
            let scene_input = SceneInput {
                geometry: &geo,
                materials: &mats,
                draws: match &scene_frame {
                    Some(scene) => SceneDraws::Scene(scene),
                    None => SceneDraws::List(&draws),
                },
                skinning: Some(&skinned_frame),
                light: match shot.sun_off_after {
                    Some(after) if captured >= after => DirectionalLight {
                        color: Vec3::ZERO,
                        ambient: Vec3::ZERO,
                        ..sun
                    },
                    _ => sun,
                },
                lights: &lights,
                decals: decal_set.as_ref().map_or(&[], |s| s.decals.as_slice()),
                environment: environment.as_ref(),
                reflection_probes: probes.as_ref(),
                gi,
            };
            let view_target = ViewTarget {
                texture: ldr,
                rect: None,
                load: LoadOp::DontCare,
            };
            let view_frame = match generated {
                // Frame generation shots hook nothing into the scene (asserted above).
                Some(generated) => frame.add_view_generating(
                    &mut fg,
                    &mut context,
                    &mut view,
                    &scene_input,
                    &camera,
                    view_target,
                    generated,
                    |_, _| {},
                ),
                None => frame.add_view_with(
                    &mut fg,
                    &mut context,
                    &mut view,
                    &scene_input,
                    &camera,
                    view_target,
                    |fg, targets| {
                        // Brixelizer GI's radiance cache reads this frame's lit image next
                        // frame.
                        if let Some(gi) = screen_gi {
                            gi.add_capture_pass(fg, targets.color);
                        }
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
                ),
            }
            .expect("frame");
            // The UI layer over the real frame, and over the generated one with its camera.
            if let (Some(ui), Some(canvases)) = (ui.as_ref(), canvases.as_ref()) {
                ui.add_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    canvases,
                    ldr,
                    &camera,
                    &view_frame,
                )
                .expect("UI");
                if let (Some(generated), Some(g)) = (generated, view_frame.generated)
                    && g.shown
                {
                    ui.add_passes(
                        &mut fg,
                        &mut ring,
                        &bindless,
                        canvases,
                        generated.texture,
                        &g.camera,
                        &view_frame,
                    )
                    .expect("UI");
                }
            }
            // A frame generation shot captures the generated frame.
            let (shown, shown_camera) = match (generated, view_frame.generated) {
                (Some(generated), Some(g)) if g.shown => (generated.texture, g.camera),
                _ => (ldr, camera),
            };
            if capture && let Some(layer) = ui_layer.as_ref() {
                ui_regions = layer.regions(&shown_camera, (width, height));
            }
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
            if let Some(view) = sdf_view {
                // The distance field's view over the frame, converted to its format.
                fg.add_pass("brixelizer view")
                    .read(view, Access::TransferRead)
                    .write(ldr, Access::TransferWrite)
                    .execute(move |ctx| {
                        let layers = vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        };
                        let corner = vk::Offset3D {
                            x: width as i32,
                            y: height as i32,
                            z: 1,
                        };
                        let region = vk::ImageBlit {
                            src_subresource: layers,
                            src_offsets: [vk::Offset3D::default(), corner],
                            dst_subresource: layers,
                            dst_offsets: [vk::Offset3D::default(), corner],
                        };
                        // SAFETY: recording; layouts arranged by the graph.
                        unsafe {
                            ctx.device.cmd_blit_image(
                                ctx.cmd,
                                ctx.image(view),
                                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                ctx.image(ldr),
                                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                                &[region],
                                vk::Filter::NEAREST,
                            );
                        }
                    });
            }
            if capture
                && shot.gi_readback
                && let Some(irradiance) = view_frame.passes.gi
            {
                // Probe GI's irradiance is at its resolve's scale of the render size.
                let desc = fg.texture_desc(irradiance);
                let (w, h) = (desc.width, desc.height);
                if gi_readback.is_none() {
                    let (buffer, memory) = allocator
                        .create_buffer(
                            &vk::BufferCreateInfo::default()
                                .size(u64::from(w * h * 8))
                                .usage(vk::BufferUsageFlags::TRANSFER_DST),
                            MemoryLocation::Readback,
                        )
                        .expect("GI readback");
                    gi_readback = Some((buffer, memory, (w, h)));
                }
                let (buffer, _, size) = gi_readback.as_ref().expect("GI readback");
                assert_eq!(*size, (w, h), "{}: the render size changed", shot.name);
                let out = fg.import_buffer(
                    "gi readback",
                    ImportedBuffer {
                        buffer: *buffer,
                        size: u64::from(w * h * 8),
                        address: 0,
                        initial: BufferState::idle(),
                        final_state: Some(BufferState::host_read()),
                    },
                );
                fg.add_pass("read back gi")
                    .read(irradiance, Access::TransferRead)
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
                                width: w,
                                height: h,
                                depth: 1,
                            },
                            ..Default::default()
                        };
                        // SAFETY: recording; layouts arranged by the graph.
                        unsafe {
                            ctx.device.cmd_copy_image_to_buffer(
                                ctx.cmd,
                                ctx.image(irradiance),
                                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                ctx.buffer(out),
                                &[region],
                            );
                        }
                    });
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
                    .read(shown, Access::TransferRead)
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
                                ctx.image(shown),
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
        if timed == 0 {
            // Times of the frame before this one's recording, as below.
            for time in graph.gpu_pass_times() {
                *startup_us.entry(time.name).or_default() += f64::from(time.gpu_us);
            }
        }
        // Material types (ADR 0092): the pipelines a frame asked for are made before the
        // next one, so every run draws the same frames with them (the first with their
        // fallbacks) however long the compiles take, and temporal effects (screen-space
        // reflections' noise and history) see the same sequence. So are ambient occlusion's
        // bent-normal variants (ADR 0058).
        let bent = shot.ao.is_some_and(|ao| ao.bent_normals);
        if (typed || bent) && !shot.material_first_sight {
            assert!(
                frame
                    .forward_mut()
                    .wait_for_material_types(&device, std::time::Duration::from_secs(120)),
                "material types' pipelines not made in 2 minutes"
            );
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
        if capture && let Some((_, memory, (w, h))) = gi_readback.as_ref() {
            let ptr = memory.mapped_ptr().expect("mapped GI readback");
            // SAFETY: the GPU is idle; the mapping covers the image (RGBA16F).
            let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), (w * h * 8) as usize) };
            let texel =
                |p: &[u8], k: usize| half::to_f32(u16::from_le_bytes([p[2 * k], p[2 * k + 1]]));
            let pixels: Vec<[f32; 4]> = raw
                .chunks_exact(8)
                .map(|p| [texel(p, 0), texel(p, 1), texel(p, 2), texel(p, 3)])
                .collect();
            let (mut sum, mut n) = ([0.0f64; 3], 0u64);
            for p in pixels.iter().filter(|p| p[3] > 0.0) {
                for k in 0..3 {
                    sum[k] += f64::from(p[k]);
                }
                n += 1;
            }
            let n = n.max(1) as f64;
            gi_means.push([
                (sum[0] / n) as f32,
                (sum[1] / n) as f32,
                (sum[2] / n) as f32,
            ]);
            if gi_image.is_none() {
                gi_image = Some((*w, *h, pixels));
            }
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
            // reflection probes captured, and its GI volume converged: captures and the
            // convergence are not steady-state work).
            let faces = probes.as_ref().map_or(0, |p| p.stats().faces);
            let capturing = faces > 0 || previous_faces > 0;
            previous_faces = faces;
            // Not converged after this frame, the frame before or the one before that (whose
            // updates the frame before then made: the times are a frame late).
            let converging = gi_volume.as_ref().is_some_and(|g| !g.is_converged())
                || (brixelizer.is_some() && brixelizer_frames < warmup);
            let gi_settling = converging || converging_before[0] || converging_before[1];
            converging_before = [converging, converging_before[0]];
            if gi_settling && resident_frames > 0 && (gi_volume.is_some() || brixelizer.is_some()) {
                gi_settle.0 += 1;
                gi_settle.1 += graph
                    .gpu_pass_times()
                    .iter()
                    .filter(|t| {
                        ["gi ", "brixelizer", "blas", "tlas"]
                            .iter()
                            .any(|p| t.name.starts_with(p))
                    })
                    .map(|t| f64::from(t.gpu_us))
                    .sum::<f64>();
            }
            if resident_frames > 3 && !capturing && !gi_settling {
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
    let mut gi_parts = Vec::new();
    if let Some(volume) = gi_volume.as_ref() {
        gi_parts.push(("probe volume", volume.memory_bytes()));
    }
    if let Some(rays) = gi_rays.as_ref() {
        let stats = rays.stats();
        gi_parts.push((
            "acceleration structures",
            stats.blas_bytes + stats.tlas_bytes,
        ));
    }
    if let Some(sdf) = brixelizer.as_ref() {
        gi_parts.push(("distance field", sdf.gpu_memory()));
    }
    if let Some(gi) = brixelizer_gi.as_ref() {
        gi_parts.push(("view GI", gi.gpu_memory()));
    }
    let gi_bytes = gi_parts.iter().map(|(_, bytes)| bytes).sum();

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
        if let Some(ui) = ui.as_mut() {
            ui.destroy(&mut allocator);
        }
        if let Some(images) = generation {
            images.destroy(&device, &mut allocator);
        }
        if let Some(particles) = particles.as_mut() {
            particles.destroy(&mut allocator);
        }
        if let Some(game) = game.as_mut() {
            game.scene.destroy(&mut allocator);
        }
        if let Some(rays) = gi_rays.as_mut() {
            rays.destroy(&device, &mut allocator);
        }
        if let Some(volume) = gi_volume.as_mut() {
            volume.destroy(&device, &mut allocator, &bindless);
        }
        if let Some(gi) = brixelizer_gi.as_mut() {
            gi.destroy(&device, &mut allocator, &bindless, frame_number + 1);
        }
        if let Some(sdf) = brixelizer.as_mut() {
            sdf.destroy(&device, &mut allocator);
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
        if let Some(set) = decal_set.as_mut() {
            set.destroy(&device, &mut allocator, &bindless);
        }
        allocator.destroy_buffer(readback, readback_mem);
        if let Some((buffer, memory, _)) = gi_readback.take() {
            allocator.destroy_buffer(buffer, memory);
        }
        d.destroy_command_pool(command_pool, None);
    }
    // `ENGINE_GOLDEN_PSO_RECORD=<file>`: the material types' entry points this shot drew,
    // merged into a PSO record (ADR 0092); `tests/golden/pso.ron` is the shots' own, which
    // their cooks use (and every entry point a shot drew must be in it).
    if let Ok(path) = std::env::var("ENGINE_GOLDEN_PSO_RECORD") {
        let mut record = PsoRecord::new();
        for (desc, entries) in frame.forward().material_type_entries_drawn() {
            for entry in entries {
                record.add(&desc.module, &desc.type_name, &entry);
            }
        }
        record.save_merged(Path::new(&path)).expect("PSO record");
    } else {
        let misses = frame.forward().material_type_pack_misses();
        assert!(
            misses.is_empty(),
            "the shots' PSO record {} lacks {misses:?}: record it again with \
             ENGINE_GOLDEN_PSO_RECORD={} (both GPUs)",
            golden_pso_record().display(),
            golden_pso_record().display(),
        );
    }
    // Streamline goes before the device it is attached to (ADR 0063).
    drop((
        brixelizer_gi,
        brixelizer,
        game,
        ui,
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
        startup_us,
        async_compute,
        antialiasing,
        frames,
        gi_bytes,
        gi_parts,
        gi_settle,
        gi_means,
        gi_image,
        ui_regions,
    })
}
