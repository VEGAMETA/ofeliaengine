//! A test bed: fixed scenes through the whole frame (culling, shadows, forward shading,
//! local lights, sky light, transparency, bloom, tone mapping), rendered on one GPU and
//! read back as 8-bit sRGB, with the GPU time of every pass. The golden-image tests and
//! the performance gate both use it (ADR 0062).

use engine_assets::{
    AlphaMode as GltfAlpha, CookSettings, CookedScene, DerivedDataCache, TextureFormat as Stored,
    TextureSlot, TextureWrap, cook_scene, import_gltf,
};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::ao::AoSettings;
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, LocalLight, ShadowSettings,
    Spot,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable, Subsurface, TextureRef};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData, MeshId, OwnedMeshData, cube};
use engine_render::post::{Composite, HDR_FORMAT, ToneCurve, Tonemap};
use engine_render::postfx::{Bloom, BloomSettings};
use engine_render::subsurface::SubsurfaceProfile;
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation, Wrap,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// A physically based sky and atmosphere (ADR 0069) instead of the gradient sky (with
    /// `sky`); it drives the sun's color.
    pub atmosphere: Option<Atmosphere>,
    /// Exponential height fog (ADR 0069; with `sky`).
    pub fog: Option<HeightFog>,
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
            atmosphere: None,
            fog: None,
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
fn procedural(subsurface: bool) -> Vec<(OwnedMeshData, MaterialDesc, Mat4)> {
    let ball = sphere(24, 48);
    let block = cube();
    let mut items = vec![
        (
            block.clone(),
            MaterialDesc {
                // Shadow edges show scattering best: a red fringe (red travels furthest).
                subsurface: subsurface.then(Subsurface::default),
                ..MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 0.8)
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
        },
    ]
}

fn gpu(validation: bool) -> Result<(Instance, Device), Skip> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_golden"),
            validation,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: validation,
        })?;
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    match attempt() {
        Ok(pair) => Ok(pair),
        Err(e) if !required => Err(Skip::NoGpu(e.to_string())),
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

fn load(source: &Source, subsurface: bool) -> Result<Loaded, Skip> {
    match source {
        Source::Procedural => Ok(Loaded {
            cooked: None,
            procedural: procedural(subsurface),
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
    let loaded = load(&shot.source, shot.subsurface)?;
    let (instance, device) = gpu(options.validation)?;
    let [width, height] = shot.size;
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).expect("bindless");
    let depth_format = vk::Format::D32_SFLOAT;
    let mut renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .expect("renderer");
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
    let output_format = vk::Format::R8G8B8A8_SRGB;
    let tonemap = Tonemap::new(&device, &bindless, output_format).expect("tonemap");
    let bloom = shot
        .bloom
        .then(|| Bloom::new(&device, &bindless, BloomSettings::default()).expect("bloom"));
    let environment = shot.sky.then(|| {
        let mut environment =
            Environment::new(&device, &mut allocator, &bindless, Sky::default()).expect("sky");
        environment
            .set_atmosphere(&device, &mut allocator, &bindless, shot.atmosphere)
            .expect("atmosphere");
        environment.set_fog(shot.fog);
        environment
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
    let mut textures = TexturePool::new(&device, 256 << 20, 1);
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

    let (default_eye, default_target) = default_view(min, max);
    let eye = shot.eye.unwrap_or(default_eye);
    let target = shot.target.unwrap_or(default_target);
    let extent = (max - min).length();
    let camera = Camera {
        clip_from_world: vulkan_perspective(
            shot.fov_y.to_radians(),
            width as f32 / height as f32,
            extent * 0.001,
            extent * 2.0,
        ) * look_at(eye, target, Vec3::Y),
        position: eye,
    };

    let mut timings: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
    let mut frame_times = Vec::new();
    let mut frame_number = 0u64;
    let mut timed = 0;
    let mut captured = false;
    let mut resident_frames = 0;
    while timed < options.timed_frames || !captured {
        let uploading = textures.pending().0 > 0;
        // The captured frame: the second after everything is resident (the sky's
        // environment is built on the first).
        let capture = !uploading && !captured && resident_frames >= 2;
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
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            frame_number += 1;
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                .expect("textures");
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                .expect("materials");
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                .expect("geometry");
            let hdr = fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, width, height));
            let depth =
                fg.create_texture("depth", TextureDesc::new_2d(depth_format, width, height));
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
                        light: shot.sun,
                        color: hdr,
                        depth,
                        clear: Some([0.02, 0.02, 0.05, 1.0]),
                        occlusion: None,
                        temporal: None,
                        lights: &shot.lights,
                        environment: environment.as_ref(),
                    },
                )
                .expect("forward");
            let ldr = fg.create_texture("ldr", TextureDesc::new_2d(output_format, width, height));
            let bloomed = bloom
                .as_ref()
                .map(|b| b.add_passes(&mut fg, &bindless, hdr));
            tonemap.add_composite_pass(
                &mut fg,
                &bindless,
                hdr,
                ldr,
                Composite {
                    curve: shot.curve,
                    exposure: shot.exposure,
                    bloom: bloomed,
                    ..Composite::default()
                },
            );
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
        if capture {
            captured = true;
        } else if captured || resident_frames > 2 {
            // Times of the frame before this one's recording (the graph reads them when
            // its slot comes around): every frame after the scene is resident.
            if resident_frames > 3 {
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

    let ptr = readback_mem.mapped_ptr().expect("mapped readback");
    // SAFETY: the GPU is idle; the mapping covers the image.
    let rgba8 =
        unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) }.to_vec();
    let adapter = device.adapter().summary();
    let vendor_id = device.adapter().vendor_id;
    let pass_us = timings
        .into_iter()
        .map(|(name, mut v)| (name, median(&mut v)))
        .collect();
    let frame_us = median(&mut frame_times);
    let async_compute = graph.async_compute();

    let mut environment = environment;
    // SAFETY: the device is idle; everything below was created above and is not used
    // again.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        if let Some(environment) = environment.as_mut() {
            environment.destroy(&mut allocator, &bindless);
        }
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(command_pool, None);
    }
    drop((renderer, tonemap, bloom, environment, bindless, allocator));
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
    })
}
