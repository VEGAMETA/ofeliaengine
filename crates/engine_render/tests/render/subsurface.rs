//! Subsurface scattering (ADR 0059) against known answers, under the validation layer with
//! synchronization validation:
//!
//! - a surface lit evenly looks the same with and without scattering, at every budget,
//!   with and without occlusion culling and motion vectors (the profile only moves light);
//! - a profile of zero radius reproduces ordinary shading, with local lights, an
//!   environment and a partial mask;
//! - the light at the edge of a shadow spreads as the profile's half-plane response;
//! - no light crosses a depth discontinuity, nor comes from pixels of other materials;
//! - transmission through a thin sheet and a thick slab follows the profile's slab
//!   integral.
//!
//! The last three also with the sun's virtual shadow pages (ADR 0093 step 3d): the front's
//! sun then comes from the visibility pass's mask, transmission from the pages.
//!
//! Albedos are 8-bit sRGB codes; the albedo target stores them to within 0.2%.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, LocalLight, Occlusion,
    ShadowSettings, Temporal, ViewFeatures,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable, Subsurface, Transmission};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, cube};
use engine_render::post::HDR_FORMAT;
use engine_render::subsurface::{
    ProfileGaussian, SubsurfaceProfile, SubsurfaceQuality, SubsurfaceSettings,
};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_render::vsm::{VirtualShadows, VsmSettings};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation,
};

const SIZE: u32 = 128;
/// The camera sees a square of this half width at its target, 5 units away.
const HALF_WIDTH: f32 = 0.5;
const EYE: Vec3 = Vec3::new(0.0, 0.0, 5.0);

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render subsurface test"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: true,
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

/// The linear value of an 8-bit sRGB code.
fn srgb(code: u8) -> f32 {
    let c = f32::from(code) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The spacing of half floats around `x`.
fn half_ulp(x: f32) -> f32 {
    let x = x.abs().max(6.1e-5);
    (2.0f32).powi(x.log2().floor() as i32 - 10)
}

/// A box of `size` centred on `center`.
fn slab(mesh: MeshId, material: MaterialId, center: Vec3, size: Vec3) -> DrawItem {
    DrawItem {
        mesh,
        material,
        world_from_object: Mat4::from_scale_rotation_translation(size, Quat::IDENTITY, center),
    }
}

/// Everything a frame needs, kept across renders.
struct Scene {
    instance: Instance,
    device: Device,
    allocator: Allocator,
    bindless: Bindless,
    renderer: ForwardRenderer,
    geometry: GeometryPool,
    mesh: MeshId,
    textures: TexturePool,
    materials: MaterialTable,
    ring: UploadRing,
    graph: RenderGraph,
    occlusion: Occlusion,
    temporal: Temporal,
    environment: Environment,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
    frame_number: u64,
    /// The sun's virtual shadow pages, when the view reads them ([`Scene::pages`]).
    pages: Option<VirtualShadows>,
}

/// What one render draws.
struct Frame<'a> {
    draws: &'a [DrawItem],
    light: DirectionalLight,
    lights: &'a [LocalLight],
    environment: bool,
    occlusion: bool,
    temporal: bool,
}

impl Scene {
    fn new() -> Option<Self> {
        let (instance, device) = gpu()?;
        println!("adapter: {}", device.adapter().summary());
        let d = device.raw();
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let renderer = ForwardRenderer::new(
            &device,
            &bindless,
            HDR_FORMAT,
            vk::Format::D32_SFLOAT,
            ShadowSettings {
                distance: 12.0,
                ..ShadowSettings::default()
            },
        )
        .unwrap();
        let mut geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
        let textures = TexturePool::new(&device, 1 << 20, 1);
        let materials = MaterialTable::new(1);
        let ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let occlusion = Occlusion::new(&device, &mut allocator, 64).unwrap();
        let temporal = Temporal::new(&device, &mut allocator, 64).unwrap();
        let environment =
            Environment::new(&device, &mut allocator, &bindless, Sky::default()).unwrap();
        let (readback, readback_memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(u64::from(SIZE * SIZE * 8))
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();
        // SAFETY: plain object creation.
        let (pool, cmd) = unsafe {
            let pool = d
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
                        .command_pool(pool)
                        .command_buffer_count(1),
                )
                .unwrap()[0];
            (pool, cmd)
        };
        Some(Self {
            instance,
            device,
            allocator,
            bindless,
            renderer,
            geometry,
            mesh,
            textures,
            materials,
            ring,
            graph,
            occlusion,
            temporal,
            environment,
            pool,
            cmd,
            readback,
            readback_memory: Some(readback_memory),
            frame_number: 0,
            pages: None,
        })
    }

    /// The sun's shadow from virtual pages instead of the cascades; `false` when the
    /// device lacks what they need.
    fn pages(&mut self) -> bool {
        if !self
            .renderer
            .set_virtual_shadows(&self.device, &self.bindless, true)
            .unwrap()
        {
            println!("skipping: the device lacks what the pages need");
            return false;
        }
        self.pages = Some(
            VirtualShadows::new(
                &self.device,
                &mut self.allocator,
                &self.bindless,
                VsmSettings {
                    pool_pages: 1024,
                    ..VsmSettings::default()
                },
            )
            .unwrap(),
        );
        true
    }

    /// Renders `frame` twice (occlusion culling then has last frame's visibility) and
    /// returns the second image's pixels, row by row.
    fn render(&mut self, frame: &Frame<'_>) -> Vec<[f32; 3]> {
        let camera = Camera {
            clip_from_world: vulkan_perspective(2.0 * (HALF_WIDTH / EYE.z).atan(), 1.0, 0.5, 20.0)
                * look_at(EYE, Vec3::ZERO, Vec3::Y),
            position: EYE,
        };
        let d = self.device.raw();
        let image_bytes = u64::from(SIZE * SIZE * 8);
        for _ in 0..2 {
            self.frame_number += 1;
            let frame_number = self.frame_number;
            // SAFETY: one-shot recording and submission, then wait.
            unsafe {
                d.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                    .unwrap();
                d.begin_command_buffer(
                    self.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .unwrap();
            }
            self.ring.begin_slot(0);
            {
                // SAFETY: nothing is in flight.
                let mut fg = unsafe { self.graph.begin_slot(0, self.cmd) };
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
                let color = fg.create_texture("color", TextureDesc::new_2d(HDR_FORMAT, SIZE, SIZE));
                let depth = fg.create_texture(
                    "depth",
                    TextureDesc::new_2d(vk::Format::D32_SFLOAT, SIZE, SIZE),
                );
                let out = fg.import_buffer(
                    "readback",
                    ImportedBuffer {
                        buffer: self.readback,
                        size: image_bytes,
                        address: 0,
                        initial: BufferState::idle(),
                        final_state: Some(BufferState::host_read()),
                    },
                );
                self.renderer
                    .add_passes(
                        &mut fg,
                        &mut self.ring,
                        &self.bindless,
                        &geo,
                        &mats,
                        frame.draws,
                        &ForwardView {
                            camera,
                            light: frame.light,
                            color,
                            depth,
                            clear: Some([0.0, 0.0, 0.0, 1.0]),
                            occlusion: frame.occlusion.then_some(&self.occlusion),
                            temporal: frame.temporal.then_some(&self.temporal),
                            shadow_cache: None,
                            virtual_shadows: self.pages.as_ref(),
                            lights: frame.lights,
                            decals: &[],
                            environment: frame.environment.then_some(&self.environment),
                            reflections: Default::default(),
                            gi: None,
                            features: ViewFeatures::ALL,
                        },
                    )
                    .unwrap();
                fg.add_pass("read back")
                    .read(color, Access::TransferRead)
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
                                width: SIZE,
                                height: SIZE,
                                depth: 1,
                            },
                            ..Default::default()
                        };
                        // SAFETY: recording outside a rendering scope; layouts arranged by
                        // the graph.
                        unsafe {
                            ctx.device.cmd_copy_image_to_buffer(
                                ctx.cmd,
                                ctx.image(color),
                                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                ctx.buffer(out),
                                &[region],
                            );
                        }
                    });
                fg.execute_with_bindless(&mut self.allocator, Some(&self.bindless))
                    .unwrap();
            }
            // SAFETY: as above.
            unsafe {
                d.end_command_buffer(self.cmd).unwrap();
                let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(self.cmd)];
                d.queue_submit2(
                    self.device.graphics.raw,
                    &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                    vk::Fence::null(),
                )
                .unwrap();
                self.device.wait_idle().unwrap();
            }
        }
        let memory = self.readback_memory.as_ref().unwrap();
        let ptr = memory.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), image_bytes as usize) };
        bytes
            .chunks_exact(8)
            .map(|p| {
                core::array::from_fn(|c| {
                    engine_core::half::to_f32(u16::from_le_bytes([p[2 * c], p[2 * c + 1]]))
                })
            })
            .collect()
    }

    fn profiles(&mut self, profiles: &[SubsurfaceProfile]) {
        self.renderer
            .set_subsurface_profiles(&self.device, &self.bindless, profiles)
            .unwrap();
    }

    fn finish(mut self) {
        // SAFETY: the device is idle.
        unsafe {
            if let Some(mut pages) = self.pages.take() {
                pages.destroy(&mut self.allocator, &self.bindless);
            }
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.ring.destroy(&mut self.allocator);
            self.geometry.destroy(&mut self.allocator);
            self.materials.destroy(&mut self.allocator);
            self.textures.destroy(&mut self.allocator, &self.bindless);
            self.environment
                .destroy(&mut self.allocator, &self.bindless);
            if let Some(memory) = self.readback_memory.take() {
                self.allocator.destroy_buffer(self.readback, memory);
            }
            self.temporal.destroy(&mut self.allocator);
            self.occlusion.destroy(&mut self.allocator);
            self.device.raw().destroy_command_pool(self.pool, None);
        }
        let Self {
            instance,
            renderer,
            bindless,
            allocator,
            environment,
            device,
            ..
        } = self;
        drop(renderer);
        drop(environment);
        drop(bindless);
        drop(allocator);
        drop(device);
        let report = instance.validation_report();
        println!(
            "validation (with synchronization validation): errors={} warnings={}",
            report.errors, report.warnings
        );
        assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
    }
}

/// A profile with one Gaussian per channel of standard deviation `sigma` (world units).
fn gaussians(sigma: [f32; 3]) -> SubsurfaceProfile {
    let g: [ProfileGaussian; 3] = core::array::from_fn(|c| {
        let mut weight = [0.0; 3];
        weight[c] = 1.0;
        ProfileGaussian {
            variance: sigma[c] * sigma[c],
            weight,
        }
    });
    SubsurfaceProfile::new(&g).unwrap()
}

/// A skin-like albedo made of 8-bit sRGB codes.
fn albedo() -> [f32; 3] {
    [srgb(230), srgb(180), srgb(150)]
}

fn material(albedo: [f32; 3], subsurface: Option<Subsurface>) -> MaterialDesc {
    MaterialDesc {
        subsurface,
        ..MaterialDesc::solid(albedo, 0.0, 0.6)
    }
}

/// The sun from the viewer's side, lighting a plane facing +z evenly.
fn front_light(ambient: f32) -> DirectionalLight {
    DirectionalLight {
        direction: Vec3::new(0.4, -0.3, -1.0).normalize(),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(ambient),
    }
}

#[test]
fn even_light_is_unchanged_by_scattering() {
    let Some(mut scene) = Scene::new() else {
        return;
    };
    scene.profiles(&[gaussians([0.08, 0.05, 0.03])]);
    let plain = scene.materials.add(material(albedo(), None));
    let scattering = scene
        .materials
        .add(material(albedo(), Some(Subsurface::default())));
    let wall = |m| slab(scene.mesh, m, Vec3::ZERO, Vec3::new(10.0, 10.0, 0.02));
    let (plain_draws, scattering_draws) = ([wall(plain)], [wall(scattering)]);
    let frame = |draws, occlusion, temporal| Frame {
        draws,
        light: front_light(0.05),
        lights: &[],
        environment: false,
        occlusion,
        temporal,
    };
    let reference = scene.render(&frame(&plain_draws, false, false));
    let mut worst = 0.0f32;
    for (occlusion, temporal) in [(false, false), (true, false), (false, true), (true, true)] {
        for quality in [
            SubsurfaceQuality::Low,
            SubsurfaceQuality::Medium,
            SubsurfaceQuality::High,
            SubsurfaceQuality::Ultra,
        ] {
            scene
                .renderer
                .set_subsurface(Some(SubsurfaceSettings { quality }));
            let image = scene.render(&frame(&scattering_draws, occlusion, temporal));
            for (p, (a, b)) in image.iter().zip(&reference).enumerate() {
                for c in 0..3 {
                    let error = (a[c] - b[c]).abs() / b[c].max(1e-3);
                    worst = worst.max(error);
                    assert!(
                        error <= 5e-3,
                        "{quality:?}, occlusion {occlusion}, temporal {temporal}: pixel {p} \
                         channel {c}: {} vs {}",
                        a[c],
                        b[c]
                    );
                }
            }
        }
    }
    println!(
        "even light: worst relative difference {worst:.2e} (center {:?})",
        reference[(SIZE * SIZE / 2 + SIZE / 2) as usize]
    );
    scene.finish();
}

#[test]
fn zero_radius_matches_ordinary_shading() {
    let Some(mut scene) = Scene::new() else {
        return;
    };
    scene.profiles(&[SubsurfaceProfile::skin().with_scale(0.0)]);
    // Double-sided, so both cast shadows from every face: the same shadow map for both
    // (single-sided subsurface materials cast from their front faces).
    let two_sided = |subsurface| MaterialDesc {
        double_sided: true,
        ..material(albedo(), subsurface)
    };
    let plain = scene.materials.add(two_sided(None));
    let lamp = [LocalLight {
        position: Vec3::new(0.2, 0.1, 0.4),
        range: 3.0,
        color: Vec3::new(0.3, 0.2, 0.1),
        spot: None,
        shadow: false,
    }];
    let mut worst = 0.0f32;
    for (mask, environment) in [(1.0, false), (0.5, false), (1.0, true)] {
        let scattering = scene.materials.add(two_sided(Some(Subsurface {
            mask,
            ..Subsurface::default()
        })));
        // A ball-like bump in front of the wall so the light is not even.
        let draws = |m| {
            [
                slab(scene.mesh, m, Vec3::ZERO, Vec3::new(10.0, 10.0, 0.02)),
                slab(
                    scene.mesh,
                    m,
                    Vec3::new(-0.15, 0.0, 0.2),
                    Vec3::new(0.2, 0.3, 0.2),
                ),
            ]
        };
        let (plain_draws, scattering_draws) = (draws(plain), draws(scattering));
        let frame = |draws| Frame {
            draws,
            light: front_light(0.05),
            lights: &lamp,
            environment,
            occlusion: false,
            temporal: false,
        };
        let reference = scene.render(&frame(&plain_draws));
        let image = scene.render(&frame(&scattering_draws));
        for (p, (a, b)) in image.iter().zip(&reference).enumerate() {
            for c in 0..3 {
                // Two half-float roundings, and the 8-bit sRGB albedo target: both GPUs of
                // ref-1 store these codes 0.1-0.2% off (ADR 0059).
                let allowed = 2.0 * half_ulp(b[c]) + 3e-3 * b[c];
                let error = (a[c] - b[c]).abs() / allowed;
                worst = worst.max(error);
                assert!(
                    error <= 1.0,
                    "mask {mask}, environment {environment}: pixel {p} channel {c}: {} vs {}",
                    a[c],
                    b[c]
                );
            }
        }
    }
    println!(
        "zero radius: worst difference {:.0}% of the tolerance",
        worst * 100.0
    );
    scene.finish();
}

/// Averages each column of the rows between a quarter and three quarters of the height.
fn columns(image: &[[f32; 3]]) -> Vec<[f32; 3]> {
    let rows = SIZE / 4..3 * SIZE / 4;
    let n = rows.len() as f32;
    (0..SIZE)
        .map(|x| {
            let mut sum = [0.0f32; 3];
            for y in rows.clone() {
                let p = image[(y * SIZE + x) as usize];
                for c in 0..3 {
                    sum[c] += p[c] / n;
                }
            }
            sum
        })
        .collect()
}

/// The world x (at the plane) of a pixel column's center.
fn column_x(column: f32) -> f32 {
    ((column + 0.5) / SIZE as f32 * 2.0 - 1.0) * HALF_WIDTH * (EYE.z - 0.01) / EYE.z
}

#[test]
fn a_shadow_edge_spreads_like_the_half_plane_response() {
    a_shadow_edge_spreads_like_the_half_plane_response_with(false);
}

/// The sun's shadow from the pages: the same response.
#[test]
fn a_shadow_edge_spreads_like_the_half_plane_response_with_virtual_shadows() {
    a_shadow_edge_spreads_like_the_half_plane_response_with(true);
}

fn a_shadow_edge_spreads_like_the_half_plane_response_with(pages: bool) {
    let Some(mut scene) = Scene::new() else {
        return;
    };
    if pages && !scene.pages() {
        return;
    }
    let sigma = [0.08, 0.05, 0.03];
    let profile = gaussians(sigma);
    scene.profiles(&[profile]);
    scene.renderer.set_subsurface(Some(SubsurfaceSettings {
        quality: SubsurfaceQuality::Ultra,
    }));
    // The sun from the left at 45°; an occluder off screen at z = 1 shades x < 0.
    let light = DirectionalLight {
        direction: Vec3::new(1.0, 0.0, -1.0).normalize(),
        color: Vec3::splat(3.0),
        ambient: Vec3::ZERO,
    };
    let color = albedo();
    let plain = scene.materials.add(material(color, None));
    let scattering = scene
        .materials
        .add(material(color, Some(Subsurface::default())));
    let black = scene
        .materials
        .add(material([0.0; 3], Some(Subsurface::default())));
    let occluder = scene.materials.add(material([0.5; 3], None));
    let draws = |m| {
        [
            slab(scene.mesh, m, Vec3::ZERO, Vec3::new(10.0, 10.0, 0.02)),
            slab(
                scene.mesh,
                occluder,
                Vec3::new(-11.0, 0.0, 1.0),
                Vec3::new(20.0, 20.0, 0.02),
            ),
        ]
    };
    let frame = |draws| Frame {
        draws,
        light,
        lights: &[],
        environment: false,
        occlusion: false,
        temporal: false,
    };
    let (d_plain, d_scattering, d_black) = (draws(plain), draws(scattering), draws(black));
    let plain = columns(&scene.render(&frame(&d_plain)));
    let scattered = columns(&scene.render(&frame(&d_scattering)));
    // Specular light only: taken off both.
    let specular = columns(&scene.render(&frame(&d_black)));
    let diffuse: Vec<[f32; 3]> = plain
        .iter()
        .zip(&specular)
        .map(|(p, s)| core::array::from_fn(|c| p[c] - s[c]))
        .collect();
    let lit = diffuse[(SIZE - 4) as usize];
    // The edge where the unscattered light crosses half of the lit value.
    let edge = (1..SIZE as usize)
        .find(|&x| diffuse[x][0] >= 0.5 * lit[0])
        .map(|x| {
            let (a, b) = (diffuse[x - 1][0], diffuse[x][0]);
            let t = (0.5 * lit[0] - a) / (b - a);
            column_x(x as f32 - 1.0 + t)
        })
        .unwrap();
    println!("lit {lit:?}, shadow edge at x = {edge:.4}");
    let mut worst = 0.0f32;
    for x in 0..SIZE as usize {
        let at = column_x(x as f32) - edge;
        // Within the profile's reach of the edge, away from the image border.
        if at.abs() > 3.0 * sigma[0] || x < 8 || x > SIZE as usize - 8 {
            continue;
        }
        let response = profile.half_plane(at);
        for c in 0..3 {
            let expected = lit[c] * response[c];
            let got = scattered[x][c] - specular[x][c];
            let error = (got - expected).abs() / lit[c];
            worst = worst.max(error);
            assert!(
                error <= 0.02,
                "column {x} (x − edge = {at:.4}) channel {c}: {got} vs {expected}"
            );
        }
    }
    println!(
        "half-plane response: worst error {:.2}% of the lit value",
        worst * 100.0
    );
    scene.finish();
}

#[test]
fn no_light_crosses_depth_or_material_edges() {
    no_light_crosses_depth_or_material_edges_with(false);
}

/// The sun's shadow from the pages: nothing crosses either.
#[test]
fn no_light_crosses_depth_or_material_edges_with_virtual_shadows() {
    no_light_crosses_depth_or_material_edges_with(true);
}

fn no_light_crosses_depth_or_material_edges_with(pages: bool) {
    let Some(mut scene) = Scene::new() else {
        return;
    };
    if pages && !scene.pages() {
        return;
    }
    scene.profiles(&[gaussians([0.08, 0.05, 0.03])]);
    scene.renderer.set_subsurface(Some(SubsurfaceSettings {
        quality: SubsurfaceQuality::Ultra,
    }));
    // Lit from the left at 45°: the near plane (x < 0, z = 1) shades the far one's
    // visible part (x from 0 to about 1 at z = 0) while it is lit itself.
    let light = DirectionalLight {
        direction: Vec3::new(1.0, 0.0, -1.0).normalize(),
        color: Vec3::splat(3.0),
        ambient: Vec3::ZERO,
    };
    let far = scene
        .materials
        .add(material(albedo(), Some(Subsurface::default())));
    for near_scatters in [true, false] {
        let near = scene
            .materials
            .add(material(albedo(), near_scatters.then(Subsurface::default)));
        let draws = [
            slab(scene.mesh, far, Vec3::ZERO, Vec3::new(10.0, 10.0, 0.02)),
            slab(
                scene.mesh,
                near,
                Vec3::new(-5.0, 0.0, 1.0),
                Vec3::new(10.0, 10.0, 0.02),
            ),
        ];
        let image = scene.render(&Frame {
            draws: &draws,
            light,
            lights: &[],
            environment: false,
            occlusion: false,
            temporal: false,
        });
        let cols = columns(&image);
        let lit = cols[8];
        let edge = SIZE as usize / 2;
        println!(
            "near plane scatters: {near_scatters}; near {lit:?}, far next to it {:?}",
            cols[edge + 2]
        );
        assert!(lit[0] > 0.1, "the near plane is lit: {lit:?}");
        // The far plane's pixels right of the edge: in shadow, and nothing from the near
        // plane's light.
        for y in 0..SIZE {
            for x in edge as u32 + 2..edge as u32 + 40 {
                let p = image[(y * SIZE + x) as usize];
                for c in 0..3 {
                    assert!(
                        p[c] <= 1e-3 * lit[c],
                        "near plane scatters: {near_scatters}: pixel ({x}, {y}) channel {c}: {}",
                        p[c]
                    );
                }
            }
        }
    }
    scene.finish();
}

#[test]
fn transmission_follows_the_slab_integral() {
    transmission_follows_the_slab_integral_with(false);
}

/// Transmission through the pages: the same integral.
#[test]
fn transmission_follows_the_slab_integral_with_virtual_shadows() {
    transmission_follows_the_slab_integral_with(true);
}

fn transmission_follows_the_slab_integral_with(pages: bool) {
    let Some(mut scene) = Scene::new() else {
        return;
    };
    if pages && !scene.pages() {
        return;
    }
    let sigma = [0.4, 0.3, 0.2];
    let profile = gaussians(sigma);
    scene.profiles(&[profile]);
    // The sun straight from behind the planes: nothing reaches their front but what passes
    // through.
    let light = DirectionalLight {
        direction: Vec3::new(0.0, 0.0, 1.0),
        color: Vec3::splat(3.0),
        ambient: Vec3::ZERO,
    };
    let color = albedo();
    let cases = [
        // A thin sheet (1 mm) of thickness 0.15 by its material.
        (Transmission::Thin, 0.15, 0.001),
        // A slab 0.2 thick, measured in the shadow map.
        (Transmission::Thick, 0.0, 0.2),
    ];
    for (transmission, thickness, depth) in cases {
        let sheet = scene.materials.add(material(
            color,
            Some(Subsurface {
                transmission,
                thickness,
                ..Subsurface::default()
            }),
        ));
        let draws = [slab(
            scene.mesh,
            sheet,
            Vec3::new(0.0, 0.0, -depth * 0.5),
            Vec3::new(10.0, 10.0, depth),
        )];
        let image = scene.render(&Frame {
            draws: &draws,
            light,
            lights: &[],
            environment: false,
            occlusion: false,
            temporal: false,
        });
        let center = columns(&image)[SIZE as usize / 2];
        let through = thickness.max(depth);
        let t = profile.transmission(through);
        let expected: [f32; 3] =
            core::array::from_fn(|c| color[c] / core::f32::consts::PI * 3.0 * t[c]);
        println!("{transmission:?} through {through}: {center:?}, expected {expected:?}");
        // Thick slabs measure their depth in the shadow map, a few millimetres off.
        let tolerance = if transmission == Transmission::Thin {
            0.01
        } else {
            0.04
        };
        for c in 0..3 {
            let error = (center[c] - expected[c]).abs() / expected[c];
            assert!(
                error <= tolerance,
                "{transmission:?} channel {c}: {center:?} vs {expected:?}"
            );
        }
    }
    scene.finish();
}
