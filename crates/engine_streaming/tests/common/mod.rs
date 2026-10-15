//! A test bed for streaming: a pack's scene through the forward renderer with texture
//! feedback, the GPU budget and the scene streamer, one frame at a time, waited for.

#![allow(dead_code, unreachable_pub, clippy::unwrap_used, clippy::print_stdout)]

use engine_assets::{
    CookSettings, CookedInstance, CookedMaterial, CookedMesh, CookedScene, DerivedDataCache,
    EncodedTexture, LodSettings, TextureFormat, TextureSlot, TextureWrap, cook_scene,
    generate_lods, import_gltf, order_for_streaming, write_pack,
};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};
use engine_render::budget::{BudgetSettings, GpuBudget};
use engine_render::feedback::{FeedbackSettings, TextureFeedback};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, ViewFeatures,
};
use engine_render::graph::{Access, RenderGraph, TextureDesc};
use engine_render::material::MaterialTable;
use engine_render::mesh::{GeometryCapacity, GeometryPool};
use engine_render::streaming::LodView;
use engine_render::texture_pool::{MipFade, TexturePool};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};
use engine_streaming::{SceneStreamSettings, SceneStreamer, material_desc};
use std::path::{Path, PathBuf};

pub const WIDTH: u32 = 960;
pub const HEIGHT: u32 = 540;
/// Texture traffic recorded per frame on the GPU.
pub const FRAME_BUDGET: u64 = 8 << 20;

/// The test scenes: `ENGINE_TEST_ASSETS`, or `test-assets/` in the repository.
pub fn test_assets() -> PathBuf {
    std::env::var_os("ENGINE_TEST_ASSETS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets"),
        PathBuf::from,
    )
}

pub fn gpu(validation: bool, sync: bool) -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_streaming test"),
            validation,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: validation && sync,
        })?;
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    match attempt() {
        Ok(pair) => Some(pair),
        Err(e) if !required => {
            println!("skipping GPU test (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            None
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

/// Sponza cooked with detail levels into a pack in the temp directory (through the test
/// scenes' derived data cache, as the golden images are), or `None` without the scene.
pub fn sponza_pack() -> Option<PathBuf> {
    let source = test_assets().join("Sponza/Sponza.gltf");
    let bytes = std::fs::read(&source).ok()?;
    let out = std::env::temp_dir().join(format!(
        "engine_streaming_sponza_{}.pack",
        std::process::id()
    ));
    let jobs = ThreadPool::builder().build().unwrap();
    let scene = import_gltf(&bytes, source.parent()).unwrap();
    let cache = DerivedDataCache::new(test_assets().join(".ddc")).unwrap();
    let settings = CookSettings {
        lods: true,
        compression_level: 1,
        block_compression: true,
        ..CookSettings::default()
    };
    let (cooked, _) = cook_scene(&scene, source.parent(), Some(&cache), &jobs, &settings).unwrap();
    write_pack(&out, &cooked, &jobs, 1).unwrap();
    Some(out)
}

/// A small scene of its own: `count` quads in a row, each with a 512² BC7 texture of its
/// own, and a bumpy grid with detail levels stored for streaming.
pub fn synthetic_pack(count: usize) -> PathBuf {
    let mut scene = CookedScene::default();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    for t in 0..count {
        let levels: Vec<Vec<u8>> = (0..10u32)
            .map(|k| {
                let s = (512u32 >> k).max(1);
                (0..TextureFormat::Bc7Srgb.level_bytes(s, s))
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        (seed >> 32) as u8
                    })
                    .collect()
            })
            .collect();
        scene.textures.push(EncodedTexture {
            format: TextureFormat::Bc7Srgb,
            width: 512,
            height: 512,
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
            world_from_object: Mat4::from_translation(Vec3::new(t as f32 * 2.5 - 2.5, 0.0, -4.0))
                .to_cols_array(),
            skin: None,
        });
    }
    // A bumpy 1 × 1 grid of 48 × 48 quads facing +z, with detail levels.
    let n = 49u32;
    let mut positions = Vec::new();
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 / 48.0, y as f32 / 48.0);
            positions.push([
                fx * 2.0 - 1.0,
                fy * 2.0 - 1.0,
                0.2 * (fx * 9.0).sin() * (fy * 7.0).cos(),
            ]);
        }
    }
    let mut indices = Vec::new();
    for y in 0..n - 1 {
        for x in 0..n - 1 {
            let a = y * n + x;
            indices.extend_from_slice(&[a, a + 1, a + n, a + 1, a + n + 1, a + n]);
        }
    }
    let lods = generate_lods(&positions, &indices, &LodSettings::default());
    let mut mesh = CookedMesh {
        uvs: positions
            .iter()
            .map(|p| [p[0] * 0.5 + 0.5, 0.5 - p[1] * 0.5])
            .collect(),
        normals: vec![[0.0, 0.0, 1.0]; positions.len()],
        positions,
        indices,
        lods,
        min: [-1.0, -1.0, -0.2],
        max: [1.0, 1.0, 0.2],
        ..CookedMesh::default()
    };
    order_for_streaming(&mut mesh);
    scene.meshes.push(mesh);
    let out = std::env::temp_dir().join(format!(
        "engine_streaming_synthetic_{count}_{}.pack",
        std::process::id()
    ));
    let jobs = ThreadPool::builder().workers(2).build().unwrap();
    write_pack(&out, &scene, &jobs, 1).unwrap();
    out
}

/// Everything a frame needs.
pub struct Bed {
    pub instance: Instance,
    pub device: Device,
    pub allocator: Allocator,
    pub bindless: Bindless,
    pub renderer: ForwardRenderer,
    pub feedback: TextureFeedback,
    pub ring: UploadRing,
    pub graph: RenderGraph,
    pub geometry: GeometryPool,
    pub textures: TexturePool,
    pub materials: MaterialTable,
    pub budget: GpuBudget,
    pub streamer: Option<SceneStreamer>,
    pub draws: Vec<DrawItem>,
    pub min: Vec3,
    pub max: Vec3,
    command_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    pub frame_number: u64,
    /// The largest texture traffic of a frame so far, and the largest single level.
    pub worst_frame_bytes: u64,
    /// Mesh levels by distance in every direction (`LodView::around`).
    pub around: bool,
}

impl Bed {
    /// Opens the pack at `path` with a streaming pool of `limit` bytes.
    pub fn new(instance: Instance, device: Device, path: &Path, limit: u64) -> Self {
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let renderer = ForwardRenderer::new(
            &device,
            &bindless,
            vk::Format::R16G16B16A16_SFLOAT,
            vk::Format::D32_SFLOAT,
            ShadowSettings::default(),
        )
        .unwrap();
        let feedback =
            TextureFeedback::new(&device, &bindless, FeedbackSettings::default(), 1).unwrap();
        let ring = UploadRing::new(&device, &mut allocator, 1, 16 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let mut geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let mut textures = TexturePool::new(&device, FRAME_BUDGET, 1);
        // Levels fade in (ADR 0078) by frames, not by the wall clock: a test's frames
        // take far less than a game's.
        textures.set_mip_fade(Some(MipFade::Frames(4)));
        let mut materials = MaterialTable::new(1);
        let mut budget = GpuBudget::with_settings(BudgetSettings {
            streaming_limit: Some(limit),
            ..BudgetSettings::default()
        });
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let streamer = SceneStreamer::open(
            path,
            &jobs,
            &mut textures,
            &mut geometry,
            &mut allocator,
            &mut budget,
            SceneStreamSettings::default(),
        )
        .unwrap();
        let scene = &streamer.layout().scene;
        let ids = streamer.textures().to_vec();
        let material_ids: Vec<_> = scene
            .materials
            .iter()
            .map(|m| materials.add(material_desc(m, &ids)))
            .collect();
        let fallback = materials.default_material();
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        let mut draws = Vec::new();
        for i in &scene.instances {
            let world = Mat4::from_cols_array(&i.world_from_object);
            let mesh = &scene.meshes[i.mesh as usize];
            draws.push(DrawItem {
                mesh: streamer.meshes()[i.mesh as usize],
                material: i.material.map_or(fallback, |m| material_ids[m as usize]),
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
        let d = device.raw();
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
        Self {
            instance,
            device,
            allocator,
            bindless,
            renderer,
            feedback,
            ring,
            graph,
            geometry,
            textures,
            materials,
            budget,
            streamer: Some(streamer),
            draws,
            min,
            max,
            command_pool,
            cmd,
            frame_number: 0,
            worst_frame_bytes: 0,
            around: false,
        }
    }

    /// The camera looking from `eye` at `target`.
    pub fn camera(&self, eye: Vec3, target: Vec3) -> Camera {
        let size = (self.max - self.min).length().max(1.0);
        Camera {
            clip_from_world: vulkan_perspective(
                60f32.to_radians(),
                WIDTH as f32 / HEIGHT as f32,
                size * 0.0005,
                size * 2.0,
            ) * look_at(eye, target, Vec3::Y),
            position: eye,
        }
    }

    /// Changes the streaming pool.
    pub fn set_limit(&mut self, limit: u64) {
        let s = self.budget.settings();
        self.budget.set_settings(BudgetSettings {
            streaming_limit: Some(limit),
            ..s
        });
    }

    /// One frame drawn with `camera`, submitted and waited for.
    pub fn frame(&mut self, camera: Camera) {
        self.frame_number += 1;
        let frame_number = self.frame_number;
        let view = LodView::new(&camera.clip_from_world, camera.position, HEIGHT, 1.0);
        let view = if self.around { view.around() } else { view };
        if let Some(s) = self.streamer.as_mut() {
            s.update(
                &mut self.textures,
                &mut self.geometry,
                &mut self.allocator,
                &mut self.budget,
                Some(&mut self.feedback),
                Some(&view),
                &self.materials,
                frame_number,
            );
        }
        self.budget.update(&self.allocator);
        let d = self.device.raw();
        let cmd = self.cmd;
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
        self.ring.begin_slot(0);
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { self.graph.begin_slot(0, cmd) };
            let geo = self
                .geometry
                .begin_frame(&mut fg, &self.device, &mut self.allocator, frame_number)
                .unwrap();
            self.textures
                .begin_frame(&mut fg, &mut self.allocator, &self.bindless, frame_number)
                .unwrap();
            let mats = self
                .materials
                .begin_frame(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &self.textures,
                    frame_number,
                )
                .unwrap();
            let color = fg.create_texture(
                "color",
                TextureDesc::new_2d(vk::Format::R16G16B16A16_SFLOAT, WIDTH, HEIGHT),
            );
            let depth = fg.create_texture(
                "depth",
                TextureDesc::new_2d(vk::Format::D32_SFLOAT, WIDTH, HEIGHT),
            );
            let passes = self
                .renderer
                .add_passes(
                    &mut fg,
                    &mut self.ring,
                    &self.bindless,
                    &geo,
                    &mats,
                    &self.draws,
                    &ForwardView {
                        camera,
                        light: DirectionalLight {
                            direction: Vec3::new(-0.3, -1.0, -0.2),
                            color: Vec3::splat(3.0),
                            ambient: Vec3::splat(0.1),
                        },
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
                        shadow_cache: None,
                        lights: &[],
                        decals: &[],
                        environment: None,
                        reflections: Default::default(),
                        gi: None,
                        features: ViewFeatures::ALL,
                    },
                )
                .unwrap();
            self.feedback
                .add_pass(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &self.bindless,
                    &geo,
                    &mats,
                    &passes,
                    (WIDTH, HEIGHT),
                    0.0,
                    frame_number,
                )
                .unwrap();
            fg.add_pass("consume")
                .read(color, Access::TransferRead)
                .side_effects()
                .execute(|_| {});
            fg.execute_with_bindless(&mut self.allocator, Some(&self.bindless))
                .unwrap();
        }
        // SAFETY: as above.
        unsafe {
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                self.device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                vk::Fence::null(),
            )
            .unwrap();
            self.device.wait_idle().unwrap();
        }
        self.worst_frame_bytes = self.worst_frame_bytes.max(self.textures.last_frame_bytes());
    }

    /// Frames until the streamer and the pools have nothing left to do (at most `limit`),
    /// and a few more: the frame count. Levels still fading in (ADR 0078) count as work.
    pub fn settle(&mut self, camera: Camera, limit: usize) -> usize {
        let mut quiet = 0;
        for frame in 0..limit {
            self.frame(camera);
            let idle = self.streamer.as_ref().is_some_and(SceneStreamer::idle)
                && self.textures.pending().0 == 0
                && self.textures.pending_moves() == 0
                && self.textures.fading() == 0;
            quiet = if idle { quiet + 1 } else { 0 };
            // Feedback is a few frames behind, and a coarser level waits for its hold.
            if quiet >= 70 {
                return frame + 1;
            }
        }
        limit
    }

    /// Frees everything; the validation report's errors, if validated.
    pub fn finish(mut self) -> u32 {
        // SAFETY: the device is idle.
        unsafe {
            self.device.wait_idle().unwrap();
            if let Some(s) = self.streamer.take() {
                s.finish(&mut self.textures);
            }
            self.feedback.destroy(&mut self.allocator);
            self.renderer
                .destroy(&self.device, &mut self.allocator, &self.bindless);
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.ring.destroy(&mut self.allocator);
            self.geometry.destroy(&mut self.allocator);
            self.materials.destroy(&mut self.allocator);
            self.textures.destroy(&mut self.allocator, &self.bindless);
            self.device
                .raw()
                .destroy_command_pool(self.command_pool, None);
        }
        let Self {
            instance,
            device,
            allocator,
            bindless,
            renderer,
            feedback,
            ..
        } = self;
        drop((feedback, renderer, bindless, allocator, device));
        let report = instance.validation_report();
        println!(
            "validation: errors={} warnings={}",
            report.errors, report.warnings
        );
        if report.errors > 0 {
            println!("{:#?}", report.first_errors);
        }
        report.errors
    }
}
