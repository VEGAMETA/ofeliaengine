//! Image-based lighting (ADR 0046) against numbers computed on the CPU, under the
//! validation layer with synchronization validation.
//!
//! - **A white furnace** (the same radiance from every direction, no sun): metals of any
//!   roughness reflect it exactly (prefiltering keeps energy, the DFG table's
//!   multiple-scattering compensation restores what single scattering loses); a grey metal
//!   matches the split sum's DFG integral computed here; the background is the sky.
//! - **A gradient sky:** the diffuse light on an upward and a sideways face (the
//!   difference between a white and a grey dielectric, which share their specular) matches
//!   the irradiance integrated here: the spherical harmonics work.
//! - **A debug sky whose color is its direction:** looking along ±X, ±Y and ±Z, the
//!   background shows those directions: the cube faces are oriented as Vulkan samples them.
//! - **HDR panoramas** (ADR 0083): one whose color is its direction shows the right one
//!   along each axis, also turned; a white one is a white furnace; a sun found in one is
//!   left out of the lighting (an upward face gets the sky's light alone) or kept (the
//!   sun's too), and stays in the background either way.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_assets::hdr::{
    HdrImage, SunSearch, find_sun, linear_to_rgbe, panorama_direction, rgbe_to_linear,
};
use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_orthographic, vulkan_perspective};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, ViewFeatures,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, cube};
use engine_render::panorama::{Panorama, PanoramaDesc, PanoramaSun};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const SIZE: u32 = 128;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const PI: f32 = core::f32::consts::PI;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render environment test"),
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

/// The sky of `environment.slang` (`sky`), for a gradient.
fn sky_radiance(sky: &Sky, d: Vec3) -> Vec3 {
    if d.y >= 0.0 {
        sky.horizon.lerp(sky.zenith, d.y.sqrt())
    } else {
        sky.horizon.lerp(sky.ground, (-d.y * 5.0).clamp(0.0, 1.0))
    }
}

/// Irradiance on a surface with normal `n` under `sky`: ∫ L(ω) max(0, n·ω) dω.
fn irradiance(sky: &Sky, n: Vec3) -> Vec3 {
    let (steps_theta, steps_phi) = (400, 800);
    let mut sum = Vec3::ZERO;
    for i in 0..steps_theta {
        let theta = (i as f32 + 0.5) / steps_theta as f32 * PI;
        for j in 0..steps_phi {
            let phi = (j as f32 + 0.5) / steps_phi as f32 * 2.0 * PI;
            let d = Vec3::new(
                theta.sin() * phi.cos(),
                theta.cos(),
                theta.sin() * phi.sin(),
            );
            let solid = theta.sin() * (PI / steps_theta as f32) * (2.0 * PI / steps_phi as f32);
            sum += sky_radiance(sky, d) * n.dot(d).max(0.0) * solid;
        }
    }
    sum
}

/// The split sum's DFG integral (Filament's form) at n·v and perceptual roughness:
/// (∫ Gv · Fc, ∫ Gv), by the same GGX importance sampling as `environment.slang`.
fn dfg(n_dot_v: f32, perceptual: f32) -> (f32, f32) {
    let a = perceptual * perceptual;
    let v = Vec3::new((1.0 - n_dot_v * n_dot_v).sqrt(), 0.0, n_dot_v);
    let n = 4096u32;
    let (mut x, mut y) = (0.0f32, 0.0f32);
    for i in 0..n {
        let xi = (
            (i as f32 + 0.5) / n as f32,
            i.reverse_bits() as f32 * 2.328_306_4e-10,
        );
        let phi = 2.0 * PI * xi.0;
        let cos_theta = ((1.0 - xi.1) / (1.0 + (a * a - 1.0) * xi.1)).sqrt();
        let sin_theta = (1.0 - cos_theta * cos_theta).sqrt();
        let h = Vec3::new(sin_theta * phi.cos(), sin_theta * phi.sin(), cos_theta);
        let v_dot_h = v.dot(h);
        let l = 2.0 * v_dot_h * h - v;
        let (n_dot_l, n_dot_h) = (l.z.clamp(0.0, 1.0), h.z.clamp(0.0, 1.0));
        if n_dot_l <= 0.0 {
            continue;
        }
        let a2 = a * a;
        let gv_ = n_dot_l * (n_dot_v * n_dot_v * (1.0 - a2) + a2).sqrt();
        let gl = n_dot_v * (n_dot_l * n_dot_l * (1.0 - a2) + a2).sqrt();
        let visibility = 0.5 / (gv_ + gl);
        let gv = visibility * 4.0 * v_dot_h.clamp(0.0, 1.0) * n_dot_l / n_dot_h;
        let fc = (1.0 - v_dot_h.clamp(0.0, 1.0)).powi(5);
        x += gv * fc;
        y += gv;
    }
    (x / n as f32, y / n as f32)
}

/// What `forward.slang` shades for specular IBL under a uniform radiance of 1.
fn furnace_specular(f0: f32, n_dot_v: f32, perceptual: f32) -> f32 {
    let (x, y) = dfg(n_dot_v, perceptual);
    (x + (y - x) * f0) * (1.0 + f0 * (1.0 / y - 1.0))
}

struct Scene {
    mesh: MeshId,
    materials: Vec<MaterialId>,
}

/// A forward renderer drawing five unit cubes in a row along x at z = 0, 2 apart (white
/// and grey rough dielectrics, a white rough metal, a grey metal, a white smooth metal)
/// into a `SIZE²` HDR target that is read back, one frame at a time with the GPU idle in
/// between.
struct Harness {
    instance: Instance,
    device: Device,
    allocator: Allocator,
    bindless: Bindless,
    renderer: ForwardRenderer,
    geometry: GeometryPool,
    textures: TexturePool,
    materials: MaterialTable,
    ring: UploadRing,
    graph: RenderGraph,
    readback: vk::Buffer,
    readback_memory: Option<engine_rhi::Allocation>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    frame: u64,
    draws: Vec<DrawItem>,
}

const READBACK_BYTES: u64 = (SIZE * SIZE * 8) as u64;

impl Harness {
    fn new() -> Option<Self> {
        let (instance, device) = gpu()?;
        println!("adapter: {}", device.adapter().summary());
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let renderer =
            ForwardRenderer::new(&device, &bindless, HDR, DEPTH, ShadowSettings::default())
                .unwrap();
        let mut geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
        let textures = TexturePool::new(&device, 1 << 20, 1);
        let mut materials = MaterialTable::new(1);
        let descs = [
            MaterialDesc::solid([1.0; 3], 0.0, 1.0),
            MaterialDesc::solid([0.5; 3], 0.0, 1.0),
            MaterialDesc::solid([1.0; 3], 1.0, 0.6),
            MaterialDesc::solid([0.5; 3], 1.0, 0.5),
            MaterialDesc::solid([1.0; 3], 1.0, 0.0),
        ];
        let scene = Scene {
            mesh,
            materials: descs.iter().map(|&d| materials.add(d)).collect(),
        };
        let ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let (readback, readback_memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(READBACK_BYTES)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();
        let d = device.raw();
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
        let draws: Vec<DrawItem> = scene
            .materials
            .iter()
            .enumerate()
            .map(|(k, &material)| DrawItem {
                mesh: scene.mesh,
                material,
                world_from_object: Mat4::from_scale_rotation_translation(
                    Vec3::ONE,
                    Quat::IDENTITY,
                    Vec3::new(k as f32 * 2.0 - 4.0, 0.0, 0.0),
                ),
            })
            .collect();
        Some(Self {
            instance,
            device,
            allocator,
            bindless,
            renderer,
            geometry,
            textures,
            materials,
            ring,
            graph,
            readback,
            readback_memory: Some(readback_memory),
            pool,
            cmd,
            frame: 0,
            draws,
        })
    }

    /// Renders `draws` (the five cubes when `None`) seen by `camera` under `environment`,
    /// without sunlight, and reads the image back (rgb per pixel, rows from the top).
    fn render(
        &mut self,
        environment: &Environment,
        camera: Camera,
        draws: Option<&[DrawItem]>,
    ) -> Vec<[f32; 3]> {
        self.frame += 1;
        let frame = self.frame;
        let d = self.device.raw().clone();
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
        let no_sun = DirectionalLight {
            direction: Vec3::new(-0.3, -1.0, -0.2),
            color: Vec3::ZERO,
            ambient: Vec3::ZERO,
        };
        let Self {
            device,
            allocator,
            bindless,
            renderer,
            geometry,
            textures,
            materials,
            ring,
            graph,
            readback,
            draws: cubes,
            ..
        } = self;
        let draws = draws.unwrap_or(cubes.as_slice());
        ring.begin_slot(0);
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            textures
                .begin_frame(&mut fg, allocator, bindless, frame)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, device, allocator, textures, frame)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, device, allocator, frame)
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(HDR, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, SIZE, SIZE));
            renderer
                .add_passes(
                    &mut fg,
                    ring,
                    bindless,
                    &geo,
                    &mats,
                    draws,
                    &ForwardView {
                        camera,
                        light: no_sun,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
                        shadow_cache: None,
                        virtual_shadows: None,
                        lights: &[],
                        decals: &[],
                        environment: Some(environment),
                        reflections: Default::default(),
                        gi: None,
                        features: ViewFeatures::ALL,
                    },
                )
                .unwrap();
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: *readback,
                    size: READBACK_BYTES,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
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
                    // SAFETY: recording; layouts arranged by the graph.
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
            fg.execute_with_bindless(allocator, Some(&*bindless))
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
        let ptr = self.readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), READBACK_BYTES as usize) };
        raw.chunks_exact(8)
            .map(|p| {
                let h = |k: usize| half::to_f32(u16::from_le_bytes([p[2 * k], p[2 * k + 1]]));
                [h(0), h(1), h(2)]
            })
            .collect()
    }

    /// Frees everything (`environment` too) and checks the validation layer saw no error.
    fn finish(mut self, mut environment: Environment) {
        // SAFETY: the device is idle.
        unsafe {
            environment.destroy(&mut self.allocator, &self.bindless);
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.ring.destroy(&mut self.allocator);
            self.geometry.destroy(&mut self.allocator);
            self.materials.destroy(&mut self.allocator);
            self.textures.destroy(&mut self.allocator, &self.bindless);
            if let Some(memory) = self.readback_memory.take() {
                self.allocator.destroy_buffer(self.readback, memory);
            }
            self.device.raw().destroy_command_pool(self.pool, None);
        }
        let Self {
            instance,
            device,
            allocator,
            bindless,
            renderer,
            ..
        } = self;
        drop(environment);
        drop(renderer);
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

/// An orthographic view far away along `dir`, 12 units wide: flat faces are seen head-on
/// everywhere.
fn ortho(dir: Vec3, up: Vec3) -> Camera {
    let eye = -dir * 50.0;
    Camera {
        clip_from_world: vulkan_orthographic(-6.0, 6.0, -6.0, 6.0, 1.0, 100.0)
            * look_at(eye, Vec3::ZERO, up),
        position: eye,
    }
}

/// A narrow perspective view from the origin along `dir`.
fn looking(dir: Vec3, up: Vec3) -> Camera {
    Camera {
        clip_from_world: vulkan_perspective(0.5, 1.0, 0.1, 100.0) * look_at(Vec3::ZERO, dir, up),
        position: Vec3::ZERO,
    }
}

/// The pixel showing world point (x, y) of the orthographic front view (12 units over SIZE
/// pixels, y up).
fn front_pixel(image: &[[f32; 3]], x: f32, y: f32) -> [f32; 3] {
    let px = ((x + 6.0) / 12.0 * SIZE as f32) as u32;
    let py = ((6.0 - y) / 12.0 * SIZE as f32) as u32;
    image[(py * SIZE + px) as usize]
}

/// The pixel showing world x on the middle row of the top view (looking down with up = −Z:
/// world x left to right).
fn top_pixel(image: &[[f32; 3]], x: f32) -> Vec3 {
    let px = ((x + 6.0) / 12.0 * SIZE as f32) as u32;
    Vec3::from(image[((SIZE / 2) * SIZE + px) as usize])
}

fn center(image: &[[f32; 3]]) -> Vec3 {
    Vec3::from(image[((SIZE / 2) * SIZE + SIZE / 2) as usize])
}

/// What the five cubes reflect under a white furnace of radiance `one`, seen from the
/// front (n·v = 1).
fn furnace_expected(one: f32) -> [f32; 5] {
    [
        1.0 + furnace_specular(0.04, 1.0, 1.0),
        0.5 + furnace_specular(0.04, 1.0, 1.0),
        1.0,
        furnace_specular(0.5, 1.0, 0.5),
        1.0,
    ]
    .map(|e| e * one)
}

#[test]
fn image_based_lighting_matches_the_cpu() {
    let Some(mut h) = Harness::new() else {
        return;
    };
    let environment =
        Environment::new(&h.device, &mut h.allocator, &h.bindless, Sky::default()).unwrap();

    // 1. The white furnace, seen from the front (+Z faces, n·v = 1).
    environment.set_sky(Sky::uniform(Vec3::ONE));
    let front = h.render(&environment, ortho(Vec3::NEG_Z, Vec3::Y), None);
    let at = |k: usize| front_pixel(&front, k as f32 * 2.0 - 4.0, 0.0)[0];
    let expected = furnace_expected(1.0);
    let background = front_pixel(&front, 0.0, 5.0)[0];
    println!(
        "furnace: {:?} expected {expected:?}, background {background}",
        (0..5).map(at).collect::<Vec<_>>()
    );
    for (k, e) in expected.iter().enumerate() {
        assert!(
            (at(k) - e).abs() < e * 0.02,
            "material {k}: {} vs {e}",
            at(k)
        );
    }
    assert!((background - 1.0).abs() < 0.01, "background {background}");

    // 2. The gradient sky: diffuse light on upward and front-facing faces.
    let sky = Sky::default();
    environment.set_sky(sky);
    let top = h.render(&environment, ortho(Vec3::NEG_Y, Vec3::NEG_Z), None);
    let front = h.render(&environment, ortho(Vec3::NEG_Z, Vec3::Y), None);
    for (name, n, white, grey) in [
        ("up", Vec3::Y, top_pixel(&top, -4.0), top_pixel(&top, -2.0)),
        (
            "front",
            Vec3::Z,
            Vec3::from(front_pixel(&front, -4.0, 0.0)),
            Vec3::from(front_pixel(&front, -2.0, 0.0)),
        ),
    ] {
        let expected = irradiance(&sky, n) / PI * 0.5;
        let measured = white - grey;
        println!("diffuse {name}: {measured} expected {expected}");
        assert!(
            (measured - expected).abs().max_element() < expected.max_element() * 0.03,
            "{name}: {measured} vs {expected}"
        );
    }

    // 3. The debug sky: looking along each axis, the background is that direction.
    environment.set_debug_directions(true);
    let hidden = [DrawItem {
        world_from_object: Mat4::from_translation(Vec3::new(0.0, -1000.0, 0.0)),
        ..h.draws[0]
    }];
    for (dir, up) in AXES {
        let image = h.render(&environment, looking(dir, up), Some(&hidden));
        let center = center(&image);
        let expected = dir * 0.5 + 0.5;
        println!("looking along {dir}: {center}");
        assert!(
            (center - expected).abs().max_element() < 0.03,
            "{dir}: {center} vs {expected}"
        );
        // Off center, towards the top of the image: towards `up`.
        let above = Vec3::from(image[((SIZE / 8) * SIZE + SIZE / 2) as usize]);
        assert!(
            (above - center).dot(up) > 0.02,
            "{dir}: up is {up}, {above} vs {center}"
        );
    }
    h.finish(environment);
}

/// The six axes, each with an up direction for a camera looking along it.
const AXES: [(Vec3, Vec3); 6] = [
    (Vec3::X, Vec3::Y),
    (Vec3::NEG_X, Vec3::Y),
    (Vec3::Z, Vec3::Y),
    (Vec3::NEG_Z, Vec3::Y),
    (Vec3::Y, Vec3::Z),
    (Vec3::NEG_Y, Vec3::Z),
];

/// A `width × height` panorama of `radiance(direction)`, its pixels encoded as RGBE.
fn panorama_pixels(width: u32, height: u32, radiance: impl Fn(Vec3) -> Vec3) -> Vec<[u8; 4]> {
    (0..width * height)
        .map(|i| {
            let uv = [
                ((i % width) as f32 + 0.5) / width as f32,
                ((i / width) as f32 + 0.5) / height as f32,
            ];
            linear_to_rgbe(radiance(panorama_direction(uv)).to_array())
        })
        .collect()
}

/// HDR panoramas (ADR 0083): the background shows them the right way round and turned;
/// a white one is a white furnace; a sun found in one leaves the lighting for a directional
/// light, and stays in the background.
#[test]
fn hdr_panoramas_light_and_surround_the_scene() {
    let Some(mut h) = Harness::new() else {
        return;
    };
    let mut environment =
        Environment::new(&h.device, &mut h.allocator, &h.bindless, Sky::default()).unwrap();
    let set = |h: &mut Harness, environment: &mut Environment, pixels: &[[u8; 4]], sun| {
        let panorama = Panorama::new(
            &h.device,
            &mut h.allocator,
            &h.bindless,
            &PanoramaDesc {
                width: 512,
                height: 256,
                pixels,
                sun,
                background_size: 0,
            },
        )
        .unwrap();
        assert_eq!(panorama.background_size(), 128);
        if let Some(mut old) = environment.set_panorama(Some(panorama)) {
            // SAFETY: the device is idle between frames here.
            unsafe { old.destroy(&mut h.allocator, &h.bindless) };
        }
    };
    let hidden = [DrawItem {
        world_from_object: Mat4::from_translation(Vec3::new(0.0, -1000.0, 0.0)),
        ..h.draws[0]
    }];

    // 1. A panorama whose color is its direction: looking along each axis shows it, and
    // turned a quarter about +Y, −X shows its −Z and +Z its −X.
    let directions = panorama_pixels(512, 256, |d| d * 0.5 + 0.5);
    set(&mut h, &mut environment, &directions, None);
    for (dir, up) in AXES {
        let image = h.render(&environment, looking(dir, up), Some(&hidden));
        let expected = dir * 0.5 + 0.5;
        println!("panorama along {dir}: {}", center(&image));
        assert!(
            (center(&image) - expected).abs().max_element() < 0.03,
            "{dir}: {} vs {expected}",
            center(&image)
        );
    }
    environment.panorama().unwrap().set_rotation(PI / 2.0);
    for (dir, shown) in [(Vec3::NEG_X, Vec3::NEG_Z), (Vec3::Z, Vec3::NEG_X)] {
        let image = h.render(&environment, looking(dir, Vec3::Y), Some(&hidden));
        let expected = shown * 0.5 + 0.5;
        println!("turned panorama along {dir}: {}", center(&image));
        assert!(
            (center(&image) - expected).abs().max_element() < 0.03,
            "{dir}: {} vs {expected}",
            center(&image)
        );
    }

    // 2. A white panorama: a white furnace (radiance 1 as RGBE decodes to 1.0039).
    let one = rgbe_to_linear(linear_to_rgbe([1.0; 3]))[0];
    let white = panorama_pixels(512, 256, |_| Vec3::ONE);
    set(&mut h, &mut environment, &white, None);
    let front = h.render(&environment, ortho(Vec3::NEG_Z, Vec3::Y), None);
    let at = |k: usize| front_pixel(&front, k as f32 * 2.0 - 4.0, 0.0)[0];
    let expected = furnace_expected(one);
    let background = front_pixel(&front, 0.0, 5.0)[0];
    println!(
        "panorama furnace: {:?} expected {expected:?}, background {background}",
        (0..5).map(at).collect::<Vec<_>>()
    );
    for (k, e) in expected.iter().enumerate() {
        assert!(
            (at(k) - e).abs() < e * 0.02,
            "material {k}: {} vs {e}",
            at(k)
        );
    }
    assert!((background - one).abs() < 0.01, "background {background}");

    // 3. A uniform sky of 0.5 with a sun of illuminance 3 (luminance) in a disk of 0.5°.
    let toward = Vec3::new(0.3, 0.8, -0.5).normalize();
    let disk = 0.5f32.to_radians();
    let sun_radiance = 0.5 + 3.0 / (2.0 * PI * (1.0 - disk.cos()));
    let sunny = panorama_pixels(512, 256, |d| {
        Vec3::splat(if d.dot(toward) > disk.cos() {
            sun_radiance
        } else {
            0.5
        })
    });
    let found = find_sun(
        &HdrImage {
            width: 512,
            height: 256,
            pixels: sunny.clone(),
        },
        &SunSearch::default(),
    )
    .unwrap();
    println!("found {found:?}");
    let sun = PanoramaSun {
        direction: found.direction,
        illuminance: found.illuminance,
        cone: found.cone,
        sky: found.sky,
    };
    // The diffuse light on an upward face: the white and grey dielectrics share their
    // specular, so their difference is 0.5 / π of the irradiance.
    let diffuse_up = |h: &mut Harness, environment: &Environment| {
        let top = h.render(environment, ortho(Vec3::NEG_Y, Vec3::NEG_Z), None);
        (top_pixel(&top, -4.0) - top_pixel(&top, -2.0)).x
    };
    let sky_only = 0.5 / PI * (PI * 0.5);
    let with_sun = 0.5 / PI * (PI * 0.5 + found.illuminance.x * found.direction.y.max(0.0));
    set(&mut h, &mut environment, &sunny, Some(sun));
    let left_out = diffuse_up(&mut h, &environment);
    set(&mut h, &mut environment, &sunny, None);
    let kept = diffuse_up(&mut h, &environment);
    println!(
        "diffuse up: sun left out {left_out} (sky only: {sky_only}), kept {kept} (with the sun: {with_sun})"
    );
    assert!(
        (left_out - sky_only).abs() < sky_only * 0.03,
        "{left_out} vs {sky_only}"
    );
    // Nine harmonics hold a sun's irradiance to within a few percent.
    assert!(
        (kept - with_sun).abs() < with_sun * 0.1,
        "{kept} vs {with_sun}"
    );
    // The background keeps the sun either way.
    set(&mut h, &mut environment, &sunny, Some(sun));
    let image = h.render(&environment, looking(toward, Vec3::Y), Some(&hidden));
    println!("background at the sun: {}", center(&image));
    assert!(center(&image).x > 100.0, "{}", center(&image));
    // Its light turns with the panorama.
    let panorama = environment.panorama().unwrap();
    panorama.set_rotation(0.7);
    let turned = panorama.sun().unwrap().direction;
    assert!(turned.dot(Quat::from_rotation_y(0.7) * found.direction) > 0.999_99);
    h.finish(environment);
}

/// Cooked panoramas (ADR 0083, phase 2): BC6H cubes uploaded through the ring over the first
/// frames show and light like panoramas made on the GPU: the right direction along every
/// axis and turned, a white furnace, the sun left out of the lighting.
#[test]
fn cooked_panoramas_show_and_light_like_made_ones() {
    use engine_assets::panorama::{PanoramaCookSettings, cook_panorama};
    use engine_render::panorama::CookedPanoramaDesc;
    let Some(mut h) = Harness::new() else {
        return;
    };
    let mut environment =
        Environment::new(&h.device, &mut h.allocator, &h.bindless, Sky::default()).unwrap();
    let jobs = engine_core::jobs::ThreadPool::builder()
        .workers(4)
        .build()
        .unwrap();
    let hidden = [DrawItem {
        world_from_object: Mat4::from_translation(Vec3::new(0.0, -1000.0, 0.0)),
        ..h.draws[0]
    }];
    // Cooks `pixels` (2048 × 1024: faces of 512², 2 MiB of BC6H) and sets the result, then
    // draws until its cube has arrived; returns the frames that took.
    let set = |h: &mut Harness, environment: &mut Environment, pixels: Vec<[u8; 4]>, sun: bool| {
        let image = HdrImage {
            width: 2048,
            height: 1024,
            pixels,
        };
        let settings = PanoramaCookSettings {
            sun: sun.then(SunSearch::default),
            ..PanoramaCookSettings::default()
        };
        let cooked = cook_panorama(&image, &settings, Some(&jobs));
        let panorama = Panorama::from_cooked(
            &h.device,
            &mut h.allocator,
            &h.bindless,
            &CookedPanoramaDesc {
                size: cooked.size,
                levels: &cooked.levels,
                sun: cooked.sun.map(|s| PanoramaSun {
                    direction: s.direction,
                    illuminance: s.illuminance,
                    cone: s.cone,
                    sky: s.sky,
                }),
            },
        )
        .unwrap();
        assert!(panorama.is_cooked() && !panorama.is_ready());
        // Every level of six 512² faces down to 1 × 1, in whole 4 × 4 blocks of 16 bytes.
        assert_eq!(panorama.memory(), 6 * 349_552);
        if let Some(mut old) = environment.set_panorama(Some(panorama)) {
            // SAFETY: the device is idle between frames here.
            unsafe { old.destroy(&mut h.allocator, &h.bindless) };
        }
        let mut frames = 0;
        while !environment.panorama().unwrap().is_ready() {
            h.render(environment, looking(Vec3::NEG_Z, Vec3::Y), Some(&hidden));
            frames += 1;
            assert!(frames < 20, "the cooked cube never arrived");
        }
        frames
    };

    // 1. Color is direction: along every axis, and turned a quarter.
    let frames = set(
        &mut h,
        &mut environment,
        panorama_pixels(2048, 1024, |d| d * 0.5 + 0.5),
        false,
    );
    println!("a cooked 512² cube arrived in {frames} frames");
    for (dir, up) in AXES {
        let image = h.render(&environment, looking(dir, up), Some(&hidden));
        let expected = dir * 0.5 + 0.5;
        println!("cooked panorama along {dir}: {}", center(&image));
        assert!(
            (center(&image) - expected).abs().max_element() < 0.03,
            "{dir}: {} vs {expected}",
            center(&image)
        );
    }
    environment.panorama().unwrap().set_rotation(PI / 2.0);
    for (dir, shown) in [(Vec3::NEG_X, Vec3::NEG_Z), (Vec3::Z, Vec3::NEG_X)] {
        let image = h.render(&environment, looking(dir, Vec3::Y), Some(&hidden));
        let expected = shown * 0.5 + 0.5;
        assert!(
            (center(&image) - expected).abs().max_element() < 0.03,
            "turned, {dir}: {} vs {expected}",
            center(&image)
        );
    }

    // 2. A white furnace.
    let one = rgbe_to_linear(linear_to_rgbe([1.0; 3]))[0];
    set(
        &mut h,
        &mut environment,
        panorama_pixels(2048, 1024, |_| Vec3::ONE),
        false,
    );
    let front = h.render(&environment, ortho(Vec3::NEG_Z, Vec3::Y), None);
    let at = |k: usize| front_pixel(&front, k as f32 * 2.0 - 4.0, 0.0)[0];
    let expected = furnace_expected(one);
    println!(
        "cooked furnace: {:?} expected {expected:?}",
        (0..5).map(at).collect::<Vec<_>>()
    );
    for (k, e) in expected.iter().enumerate() {
        assert!(
            (at(k) - e).abs() < e * 0.02,
            "material {k}: {} vs {e}",
            at(k)
        );
    }

    // 3. The sun found while cooking is left out of the lighting.
    let toward = Vec3::new(0.3, 0.8, -0.5).normalize();
    let disk = 0.5f32.to_radians();
    let sun_radiance = 0.5 + 3.0 / (2.0 * PI * (1.0 - disk.cos()));
    let sunny = panorama_pixels(2048, 1024, |d| {
        Vec3::splat(if d.dot(toward) > disk.cos() {
            sun_radiance
        } else {
            0.5
        })
    });
    set(&mut h, &mut environment, sunny, true);
    let sun = environment.panorama().unwrap().sun().unwrap();
    assert!(sun.direction.dot(toward) > 0.9999, "{sun:?}");
    let top = h.render(&environment, ortho(Vec3::NEG_Y, Vec3::NEG_Z), None);
    let diffuse = (top_pixel(&top, -4.0) - top_pixel(&top, -2.0)).x;
    let sky_only = 0.5 / PI * (PI * 0.5);
    println!("cooked, sun left out: diffuse up {diffuse} (sky only: {sky_only})");
    assert!(
        (diffuse - sky_only).abs() < sky_only * 0.03,
        "{diffuse} vs {sky_only}"
    );
    h.finish(environment);
}
