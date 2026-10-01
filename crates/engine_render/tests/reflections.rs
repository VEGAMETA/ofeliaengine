//! Local reflections (ADR 0073) against answers computed on the CPU, under the validation
//! layer with synchronization validation.
//!
//! - **Parallax correction:** a mirror floor in a closed room whose walls and ceiling glow
//!   in different colors, a probe captured off the room's centre: every floor pixel
//!   reflects the wall that the reflected ray meets (analytic ray–box intersection),
//!   where a probe read along the plain reflected direction would show another one.
//! - **Prefiltering keeps energy:** white metals of every roughness inside a room that
//!   glows evenly reflect exactly the room's radiance (every mip of the probe holds it).
//! - **Captures are cached:** a static scene captures each probe once; a probe whose
//!   surroundings did not change is never drawn again; the face budget is kept.
//! - **Screen-space reflections:** a mirror floor in front of a glowing block reflects
//!   the block exactly where the reflected rays meet its visible face (analytic), and
//!   nothing elsewhere, at full and half resolution.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::too_many_lines)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, Vec4, look_at, vulkan_perspective};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, Reflections, ShadowSettings,
};
use engine_render::gpu_scene::{GpuScene, InstanceId, SceneInstance};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, cube};
use engine_render::reflections::{
    ProbeBox, ProbeRefresh, ProbeSettings, ReflectionProbe, ReflectionProbes,
    ScreenSpaceReflections, SsrSettings,
};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const SIZE: u32 = 160;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const FOV: f32 = 1.2;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render reflections test"),
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

/// A device, a renderer with reflections on, a unit cube, a black sky; renders frames and
/// reads their color back.
struct Bed {
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
    environment: Environment,
    readback: vk::Buffer,
    readback_memory: engine_rhi::Allocation,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    mesh: MeshId,
    frame: u64,
}

const NO_SUN: DirectionalLight = DirectionalLight {
    direction: Vec3::new(-0.3, -1.0, -0.2),
    color: Vec3::ZERO,
    ambient: Vec3::ZERO,
};

impl Bed {
    fn new() -> Option<Self> {
        let (instance, device) = gpu()?;
        println!("adapter: {}", device.adapter().summary());
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let mut renderer =
            ForwardRenderer::new(&device, &bindless, HDR, DEPTH, ShadowSettings::default())
                .unwrap();
        renderer.set_reflections(&device, &bindless, true).unwrap();
        let mut geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
        let textures = TexturePool::new(&device, 1 << 20, 1);
        let materials = MaterialTable::new(1);
        let ring = UploadRing::new(&device, &mut allocator, 1, 8 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let environment =
            Environment::new(&device, &mut allocator, &bindless, Sky::uniform(Vec3::ZERO)).unwrap();
        let bytes = u64::from(SIZE * SIZE * 8);
        let (readback, readback_memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes)
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
            environment,
            readback,
            readback_memory,
            pool,
            cmd,
            mesh,
            frame: 0,
        })
    }

    fn material(&mut self, desc: MaterialDesc) -> MaterialId {
        self.materials.add(desc)
    }

    /// A box from `min` to `max` of `material`.
    fn block(&self, material: MaterialId, min: Vec3, max: Vec3) -> DrawItem {
        DrawItem {
            mesh: self.mesh,
            material,
            world_from_object: Mat4::from_scale_rotation_translation(
                max - min,
                Quat::IDENTITY,
                (min + max) * 0.5,
            ),
        }
    }

    fn probes(&mut self, capacity: u32, faces_per_frame: u32) -> ReflectionProbes {
        ReflectionProbes::new(
            &self.device,
            &mut self.allocator,
            &self.bindless,
            ProbeSettings {
                capacity,
                faces_per_frame,
                ..ProbeSettings::default()
            },
        )
        .unwrap()
    }

    fn ssr(&mut self, settings: SsrSettings) -> ScreenSpaceReflections {
        ScreenSpaceReflections::new(
            &self.device,
            &mut self.allocator,
            &self.bindless,
            SIZE,
            SIZE,
            settings,
        )
        .unwrap()
    }

    /// Renders a frame and returns its color, rows top to bottom.
    fn render(
        &mut self,
        camera: Camera,
        draws: &[DrawItem],
        reflections: Reflections<'_>,
    ) -> Vec<Vec3> {
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
        self.ring.begin_slot(0);
        let bytes = u64::from(SIZE * SIZE * 8);
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { self.graph.begin_slot(0, cmd) };
            self.textures
                .begin_frame(&mut fg, &mut self.allocator, &self.bindless, frame)
                .unwrap();
            let mats = self
                .materials
                .begin_frame(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &self.textures,
                    frame,
                )
                .unwrap();
            let geo = self
                .geometry
                .begin_frame(&mut fg, &self.device, &mut self.allocator, frame)
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(HDR, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, SIZE, SIZE));
            self.renderer
                .add_passes(
                    &mut fg,
                    &mut self.ring,
                    &self.bindless,
                    &geo,
                    &mats,
                    draws,
                    &ForwardView {
                        camera,
                        light: NO_SUN,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
                        lights: &[],
                        environment: Some(&self.environment),
                        reflections,
                    },
                )
                .unwrap();
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: self.readback,
                    size: bytes,
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
        let ptr = self.readback_memory.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), bytes as usize) };
        raw.chunks_exact(8)
            .map(|p| {
                let h = |k: usize| half::to_f32(u16::from_le_bytes([p[2 * k], p[2 * k + 1]]));
                Vec3::new(h(0), h(1), h(2))
            })
            .collect()
    }

    /// Renders a frame of a persistent GPU scene (no readback).
    fn render_scene(&mut self, camera: Camera, scene: &mut GpuScene, reflections: Reflections<'_>) {
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
        self.ring.begin_slot(0);
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { self.graph.begin_slot(0, cmd) };
            self.textures
                .begin_frame(&mut fg, &mut self.allocator, &self.bindless, frame)
                .unwrap();
            let mats = self
                .materials
                .begin_frame(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &self.textures,
                    frame,
                )
                .unwrap();
            let geo = self
                .geometry
                .begin_frame(&mut fg, &self.device, &mut self.allocator, frame)
                .unwrap();
            let scene_frame = scene
                .add_passes(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &mut self.ring,
                    &self.bindless,
                    &geo,
                    &mats,
                    frame,
                )
                .unwrap()
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(HDR, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, SIZE, SIZE));
            self.renderer
                .add_scene_passes(
                    &mut fg,
                    &mut self.ring,
                    &self.bindless,
                    &geo,
                    &mats,
                    &scene_frame,
                    &ForwardView {
                        camera,
                        light: NO_SUN,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
                        lights: &[],
                        environment: Some(&self.environment),
                        reflections,
                    },
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
    }

    /// Frees everything and checks the validation layer saw no errors.
    fn finish(mut self, mut probes: Vec<ReflectionProbes>, mut ssr: Vec<ScreenSpaceReflections>) {
        let d = self.device.raw().clone();
        // SAFETY: the device is idle.
        unsafe {
            for p in &mut probes {
                p.destroy(&mut self.allocator, &self.bindless);
            }
            for s in &mut ssr {
                s.destroy(&mut self.allocator, &self.bindless);
            }
            self.environment
                .destroy(&mut self.allocator, &self.bindless);
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.ring.destroy(&mut self.allocator);
            self.geometry.destroy(&mut self.allocator);
            self.materials.destroy(&mut self.allocator);
            self.textures.destroy(&mut self.allocator, &self.bindless);
            self.allocator
                .destroy_buffer(self.readback, self.readback_memory);
            d.destroy_command_pool(self.pool, None);
        }
        drop((probes, ssr, self.environment, self.renderer, self.bindless));
        drop(self.allocator);
        drop(self.device);
        let report = self.instance.validation_report();
        println!(
            "validation (with synchronization validation): errors={} warnings={}",
            report.errors, report.warnings
        );
        assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
    }
}

/// A perspective camera at `eye` looking at `target` (square, FOV).
fn camera(eye: Vec3, target: Vec3) -> Camera {
    Camera {
        clip_from_world: vulkan_perspective(FOV, 1.0, 0.05, 100.0) * look_at(eye, target, Vec3::Y),
        position: eye,
    }
}

/// The world-space ray through the centre of pixel `(x, y)`.
fn pixel_ray(camera: &Camera, x: u32, y: u32) -> Vec3 {
    let inverse = camera.clip_from_world.inverse();
    let ndc = Vec3::new(
        (x as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0,
        (y as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0,
        0.5,
    );
    let p = inverse * Vec4::new(ndc.x, ndc.y, ndc.z, 1.0);
    (p.truncate() / p.w - camera.position).normalize()
}

/// The room of the probe tests: inside `-4..4 × 0..4 × -4..4`, walls 0.2 thick.
const ROOM_MIN: Vec3 = Vec3::new(-4.0, 0.0, -4.0);
const ROOM_MAX: Vec3 = Vec3::new(4.0, 4.0, 4.0);

/// The walls of the room (−X, +X, −Z, +Z, ceiling) as boxes of `materials` (in that
/// order), and the floor (top at y = 0) of `floor`.
fn room(bed: &Bed, walls: &[MaterialId; 5], floor: MaterialId) -> Vec<DrawItem> {
    let (lo, hi, t) = (ROOM_MIN, ROOM_MAX, 0.2);
    vec![
        bed.block(
            walls[0],
            Vec3::new(lo.x - t, lo.y - t, lo.z - t),
            Vec3::new(lo.x, hi.y + t, hi.z + t),
        ),
        bed.block(
            walls[1],
            Vec3::new(hi.x, lo.y - t, lo.z - t),
            Vec3::new(hi.x + t, hi.y + t, hi.z + t),
        ),
        bed.block(
            walls[2],
            Vec3::new(lo.x, lo.y - t, lo.z - t),
            Vec3::new(hi.x, hi.y + t, lo.z),
        ),
        bed.block(
            walls[3],
            Vec3::new(lo.x, lo.y - t, hi.z),
            Vec3::new(hi.x, hi.y + t, hi.z + t),
        ),
        bed.block(
            walls[4],
            Vec3::new(lo.x, hi.y, lo.z),
            Vec3::new(hi.x, hi.y + t, hi.z),
        ),
        bed.block(
            floor,
            Vec3::new(lo.x, lo.y - t, lo.z),
            Vec3::new(hi.x, lo.y, hi.z),
        ),
    ]
}

/// Which face of the room box a ray from inside meets (0 −X, 1 +X, 2 −Z, 3 +Z, 4 ceiling,
/// 5 floor) and how far from that face's edges, in world units.
fn room_face(origin: Vec3, dir: Vec3) -> (usize, f32) {
    let b = ProbeBox::new((ROOM_MIN + ROOM_MAX) * 0.5, (ROOM_MAX - ROOM_MIN) * 0.5);
    let t = b.exit_distance(origin, dir).unwrap();
    let p = origin + dir * t;
    let near = |v: f32, lo: f32, hi: f32| (v - lo).min(hi - v);
    let candidates = [
        (0, (p.x - ROOM_MIN.x).abs()),
        (1, (p.x - ROOM_MAX.x).abs()),
        (2, (p.z - ROOM_MIN.z).abs()),
        (3, (p.z - ROOM_MAX.z).abs()),
        (4, (p.y - ROOM_MAX.y).abs()),
        (5, (p.y - ROOM_MIN.y).abs()),
    ];
    let face = candidates
        .iter()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
        .0;
    let margin = match face {
        0 | 1 => near(p.y, ROOM_MIN.y, ROOM_MAX.y).min(near(p.z, ROOM_MIN.z, ROOM_MAX.z)),
        2 | 3 => near(p.y, ROOM_MIN.y, ROOM_MAX.y).min(near(p.x, ROOM_MIN.x, ROOM_MAX.x)),
        _ => near(p.x, ROOM_MIN.x, ROOM_MAX.x).min(near(p.z, ROOM_MIN.z, ROOM_MAX.z)),
    };
    (face, margin)
}

/// Renders frames until every probe is captured.
fn capture_all(bed: &mut Bed, camera: Camera, draws: &[DrawItem], probes: &ReflectionProbes) {
    for _ in 0..32 {
        bed.render(
            camera,
            draws,
            Reflections {
                probes: Some(probes),
                ssr: None,
            },
        );
        if probes.stats().pending == 0 {
            return;
        }
    }
    panic!("probes not captured: {:?}", probes.stats());
}

#[test]
fn probes_correct_parallax_and_keep_energy() {
    let Some(mut bed) = Bed::new() else {
        return;
    };
    let colors = [
        Vec3::new(2.0, 0.1, 0.1),
        Vec3::new(0.1, 2.0, 0.1),
        Vec3::new(0.1, 0.1, 2.0),
        Vec3::new(2.0, 2.0, 0.1),
        Vec3::new(0.1, 2.0, 2.0),
    ];
    let glow = |c: Vec3| MaterialDesc {
        emissive: c.to_array(),
        ..MaterialDesc::solid([0.0; 3], 0.0, 1.0)
    };
    let walls = colors.map(|c| bed.material(glow(c)));
    let mirror = bed.material(MaterialDesc::solid([1.0; 3], 1.0, 0.0));
    let draws = room(&bed, &walls, mirror);
    // Captured off the room's centre: the plain reflected direction from there would read
    // the wrong wall for many floor pixels.
    let probe_at = Vec3::new(2.2, 0.8, -2.0);
    let room_box = ProbeBox::new((ROOM_MIN + ROOM_MAX) * 0.5, (ROOM_MAX - ROOM_MIN) * 0.5);
    let mut probes = bed.probes(4, 6);
    probes
        .add(ReflectionProbe {
            position: probe_at,
            influence: ProbeBox::new(room_box.center, room_box.half_extents + 0.5),
            parallax: room_box,
            blend_distance: 0.1,
            intensity: 1.0,
            far: 20.0,
            refresh: ProbeRefresh::OnChange,
        })
        .unwrap();
    let view = camera(Vec3::new(-3.0, 1.4, 3.2), Vec3::new(1.0, 0.0, -1.5));
    capture_all(&mut bed, view, &draws, &probes);
    let image = bed.render(
        view,
        &draws,
        Reflections {
            probes: Some(&probes),
            ssr: None,
        },
    );
    if let Ok(dir) = std::env::var("REFL_DUMP") {
        let mut expected_img = vec![Vec3::ZERO; image.len()];
        for y in 0..SIZE {
            for x in 0..SIZE {
                let ray = pixel_ray(&view, x, y);
                if ray.y < -0.01 {
                    let t = -view.position.y / ray.y;
                    let p = view.position + ray * t;
                    if p.x.abs() < 4.0 && p.z.abs() < 4.0 {
                        let r = Vec3::new(ray.x, -ray.y, ray.z);
                        expected_img[(y * SIZE + x) as usize] =
                            colors[room_face(p + Vec3::Y * 1e-3, r).0.min(4)];
                    }
                }
            }
        }
        for (name, img) in [("got", &image), ("expected", &expected_img)] {
            let bytes: Vec<u8> = img
                .iter()
                .flat_map(|c| {
                    (*c * 0.5)
                        .to_array()
                        .map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8)
                })
                .collect();
            let png = engine_render::png::encode_png(
                SIZE,
                SIZE,
                engine_render::png::PngFormat::Rgb,
                &bytes,
            )
            .unwrap();
            std::fs::write(format!("{dir}/parallax_{name}.png"), png).unwrap();
        }
    }
    let (mut tested, mut right, mut naive_wrong) = (0, 0, 0);
    let mut worst = (0.0f32, 0, 0, Vec3::ZERO, Vec3::ZERO);
    let mut confusion = [[0u32; 5]; 6];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let ray = pixel_ray(&view, x, y);
            if ray.y >= -0.05 {
                continue;
            }
            // The floor point this pixel shows, and the reflected ray.
            let t = -view.position.y / ray.y;
            let p = view.position + ray * t;
            if p.x.abs() > 3.8 || p.z.abs() > 3.8 {
                continue;
            }
            let r = Vec3::new(ray.x, -ray.y, ray.z);
            let (face, margin) = room_face(p + Vec3::Y * 1e-3, r);
            // Away from the walls' edges (the probe's 128² texels, filtering).
            if margin < 0.35 {
                continue;
            }
            tested += 1;
            let expected = colors[face];
            let got = image[(y * SIZE + x) as usize];
            let error = (got - expected).abs().max_element() / expected.max_element();
            if error < 0.06 {
                right += 1;
            } else {
                let seen = (0..5)
                    .min_by(|&a, &b| {
                        (got - colors[a])
                            .length()
                            .total_cmp(&(got - colors[b]).length())
                    })
                    .unwrap();
                confusion[face][seen] += 1;
                if error > worst.0 {
                    worst = (error, x, y, got, expected);
                }
            }
            if room_face(probe_at, r).0 != face {
                naive_wrong += 1;
            }
        }
    }
    println!(
        "parallax: {right} of {tested} floor pixels reflect the right wall; the plain \
         direction would read the wrong one for {naive_wrong}; worst {worst:?};          wrong ones (expected face → seen): {confusion:?}"
    );
    assert!(tested > 2000, "{tested}");
    assert!(naive_wrong * 5 > tested, "the setup does not test parallax");
    assert!(
        right as f32 >= tested as f32 * 0.99,
        "{right} of {tested}; worst {worst:?}"
    );

    // 2. Energy: the room glowing evenly (radiance 2), white metals of graded roughness
    // standing on the floor, not in the capture (a manual probe captured before they were
    // placed): each reflects 2 (f0 = 1: the split sum's weight is 1).
    let even = bed.material(glow(Vec3::splat(2.0)));
    let room_draws = room(&bed, &[even; 5], even);
    let mut probes2 = bed.probes(1, 6);
    probes2
        .add(ReflectionProbe {
            refresh: ProbeRefresh::Manual,
            ..ReflectionProbe::room(Vec3::new(0.0, 2.0, 0.0), Vec3::new(4.0, 2.0, 4.0))
        })
        .unwrap();
    let view = camera(Vec3::new(0.0, 1.0, 3.5), Vec3::new(0.0, 0.6, 0.0));
    capture_all(&mut bed, view, &room_draws, &probes2);
    let roughness = [0.0, 0.2, 0.4, 0.6, 0.8, 1.0];
    let mut draws = room_draws.clone();
    for (k, &r) in roughness.iter().enumerate() {
        let m = bed.material(MaterialDesc::solid([1.0; 3], 1.0, r));
        let x = -2.5 + k as f32;
        draws.push(bed.block(
            m,
            Vec3::new(x - 0.3, 0.3, -0.3),
            Vec3::new(x + 0.3, 0.9, 0.3),
        ));
    }
    let image = bed.render(
        view,
        &draws,
        Reflections {
            probes: Some(&probes2),
            ssr: None,
        },
    );
    assert_eq!(probes2.stats().faces, 0, "a manual probe captured again");
    for (k, &r) in roughness.iter().enumerate() {
        // The block's front face, at its centre.
        let world = Vec3::new(-2.5 + k as f32, 0.6, 0.3);
        let clip = view.clip_from_world * world.extend(1.0);
        let (px, py) = (
            ((clip.x / clip.w * 0.5 + 0.5) * SIZE as f32) as u32,
            ((clip.y / clip.w * 0.5 + 0.5) * SIZE as f32) as u32,
        );
        let got = image[(py * SIZE + px) as usize];
        println!("white metal, roughness {r}: {got}");
        assert!(
            (got - Vec3::splat(2.0)).abs().max_element() < 0.04,
            "roughness {r}: {got}"
        );
    }
    bed.finish(vec![probes, probes2], Vec::new());
}

#[test]
fn probe_captures_are_cached_and_budgeted() {
    let Some(mut bed) = Bed::new() else {
        return;
    };
    let grey = bed.material(MaterialDesc::solid([0.5; 3], 0.0, 0.5));
    let draws = room(&bed, &[grey; 5], grey);
    // Two probes, two faces per frame: each takes three frames.
    let mut probes = bed.probes(2, 2);
    let a = probes
        .add(ReflectionProbe::room(
            Vec3::new(-2.0, 2.0, 0.0),
            Vec3::new(2.0, 2.0, 4.0),
        ))
        .unwrap();
    probes
        .add(ReflectionProbe::room(
            Vec3::new(2.0, 2.0, 0.0),
            Vec3::new(2.0, 2.0, 4.0),
        ))
        .unwrap();
    let view = camera(Vec3::new(0.0, 2.0, 3.0), Vec3::new(0.0, 1.0, 0.0));
    let reflections = Reflections {
        probes: Some(&probes),
        ssr: None,
    };
    let mut faces = Vec::new();
    for _ in 0..10 {
        bed.render(view, &draws, reflections);
        faces.push(probes.stats().faces);
    }
    println!("faces per frame: {faces:?}");
    assert_eq!(faces, [2, 2, 2, 2, 2, 2, 0, 0, 0, 0]);
    // A refresh captures that probe only.
    probes.refresh(a);
    let mut faces = Vec::new();
    for _ in 0..5 {
        bed.render(view, &draws, reflections);
        faces.push(probes.stats().faces);
    }
    assert_eq!(faces, [2, 2, 2, 0, 0]);
    // A change far from both probes captures nothing; one inside the first, that one.
    probes.invalidate_region(Vec3::splat(50.0), Vec3::splat(51.0));
    bed.render(view, &draws, reflections);
    assert_eq!(probes.stats().faces, 0);
    probes.invalidate_region(Vec3::new(-3.0, 0.0, 0.0), Vec3::new(-2.5, 1.0, 1.0));
    let mut total = 0;
    for _ in 0..5 {
        bed.render(view, &draws, reflections);
        total += probes.stats().faces;
    }
    assert_eq!(total, 6);

    // The persistent GPU scene: its changes reach the probes by themselves.
    let mut scene = GpuScene::new(&bed.device, &bed.bindless, 64, 1).unwrap();
    let ids: Vec<InstanceId> = draws
        .iter()
        .map(|d| {
            scene.insert(SceneInstance {
                mesh: d.mesh,
                material: d.material,
                world_from_object: d.world_from_object,
            })
        })
        .collect();
    let ball = scene.insert(SceneInstance {
        mesh: bed.mesh,
        material: grey,
        world_from_object: Mat4::from_translation(Vec3::new(-2.0, 1.0, 1.0)),
    });
    let scene_faces = |bed: &mut Bed, scene: &mut GpuScene, frames: usize| {
        let mut total = 0;
        for _ in 0..frames {
            bed.render_scene(view, scene, reflections);
            total += probes.stats().faces;
        }
        total
    };
    // The first frames: the instances appear inside both probes.
    assert_eq!(scene_faces(&mut bed, &mut scene, 8), 12);
    // Nothing changes: nothing is captured.
    assert_eq!(scene_faces(&mut bed, &mut scene, 5), 0);
    // The block moves inside the first probe's box: that probe again.
    scene.set_transform(ball, &Mat4::from_translation(Vec3::new(-2.5, 1.0, 1.0)));
    assert_eq!(scene_faces(&mut bed, &mut scene, 5), 6);
    // Something far from both moves: nothing.
    scene.set_transform(ids[0], &Mat4::from_translation(Vec3::splat(100.0)));
    let far = scene_faces(&mut bed, &mut scene, 5);
    // (The wall left the first probe's box: its old place was inside it.)
    assert_eq!(far, 6);
    scene.set_transform(ids[0], &Mat4::from_translation(Vec3::splat(120.0)));
    assert_eq!(scene_faces(&mut bed, &mut scene, 5), 0);
    // SAFETY: the device is idle.
    unsafe { scene.destroy(&mut bed.allocator) };
    drop(scene);
    bed.finish(vec![probes], Vec::new());
}

#[test]
fn screen_space_reflections_hit_an_analytic_mirror() {
    let Some(mut bed) = Bed::new() else {
        return;
    };
    let mirror = bed.material(MaterialDesc::solid([1.0; 3], 1.0, 0.0));
    let glow = bed.material(MaterialDesc {
        emissive: [3.0, 1.5, 0.5],
        ..MaterialDesc::solid([0.0; 3], 0.0, 1.0)
    });
    let block_min = Vec3::new(-1.0, 0.0, -2.0);
    let block_max = Vec3::new(1.0, 1.5, -1.0);
    let draws = vec![
        bed.block(
            mirror,
            Vec3::new(-10.0, -0.2, -10.0),
            Vec3::new(10.0, 0.0, 10.0),
        ),
        bed.block(glow, block_min, block_max),
    ];
    let view = camera(Vec3::new(0.3, 1.6, 3.0), Vec3::new(0.0, 0.3, -1.0));
    let block = ProbeBox::new((block_min + block_max) * 0.5, (block_max - block_min) * 0.5);
    let mut states = Vec::new();
    for half_resolution in [false, true] {
        let ssr = bed.ssr(SsrSettings {
            half_resolution,
            ..SsrSettings::default()
        });
        let reflections = Reflections {
            probes: None,
            ssr: Some(&ssr),
        };
        let mut image = Vec::new();
        for _ in 0..4 {
            image = bed.render(view, &draws, reflections);
        }
        let expected_hit = Vec3::new(3.0, 1.5, 0.5);
        let mut class = vec![0u8; image.len() * 3];
        let (mut hits, mut hits_right, mut misses, mut misses_right) = (0, 0, 0, 0);
        for y in 0..SIZE {
            for x in 0..SIZE {
                let ray = pixel_ray(&view, x, y);
                if ray.y >= -0.02 {
                    continue;
                }
                let t = -view.position.y / ray.y;
                let p = view.position + ray * t;
                // The floor, not the block in front of it (with a margin at its outline).
                let outline = ProbeBox::new(block.center, block.half_extents + 0.05);
                if slab_hit(&outline, view.position, ray) {
                    continue;
                }
                if p.x > block_min.x - 0.05
                    && p.x < block_max.x + 0.05
                    && p.z > block_min.z - 0.05
                    && p.z < block_max.z + 0.05
                {
                    continue;
                }
                let r = Vec3::new(ray.x, -ray.y, ray.z);
                // The reflected ray against the block: entering through its front face
                // (+Z, which the camera sees), well inside it; or missing it by a margin.
                let tz = (block_max.z - p.z) / r.z;
                let at = p + r * tz;
                let inside_front = r.z < 0.0
                    && tz > 0.0
                    && at.x > block_min.x + 0.15
                    && at.x < block_max.x - 0.15
                    && at.y > block_min.y + 0.15
                    && at.y < block_max.y - 0.15;
                let grown = ProbeBox::new(block.center, block.half_extents + 0.25);
                let near = slab_hit(&grown, p, r);
                // Where the hit is on screen: not near the edges (the confidence fades).
                let on_screen = {
                    let c = view.clip_from_world * at.extend(1.0);
                    let uv = Vec3::new(c.x / c.w, c.y / c.w, 0.0) * 0.5 + 0.5;
                    uv.x > 0.08 && uv.x < 0.92 && uv.y > 0.08 && uv.y < 0.92
                };
                let got = image[(y * SIZE + x) as usize];
                let i = (y * SIZE + x) as usize * 3;
                class[i..i + 3].copy_from_slice(&if inside_front && on_screen {
                    [0, 255, 0]
                } else if !near {
                    if got.max_element() < 0.02 {
                        [0, 0, 120]
                    } else {
                        [255, 0, 0]
                    }
                } else {
                    [80, 80, 80]
                });
                if inside_front && on_screen {
                    hits += 1;
                    if (got - expected_hit).abs().max_element() < 0.1 * 3.0 {
                        hits_right += 1;
                    }
                } else if !near {
                    misses += 1;
                    if got.max_element() < 0.02 {
                        misses_right += 1;
                    }
                }
            }
        }
        if let Ok(dir) = std::env::var("REFL_DUMP") {
            let bytes: Vec<u8> = image
                .iter()
                .flat_map(|c| {
                    (*c * 0.3)
                        .to_array()
                        .map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8)
                })
                .collect();
            for (name, b) in [("img", &bytes), ("class", &class)] {
                let png = engine_render::png::encode_png(
                    SIZE,
                    SIZE,
                    engine_render::png::PngFormat::Rgb,
                    b,
                )
                .unwrap();
                std::fs::write(format!("{dir}/ssr_{half_resolution}_{name}.png"), png).unwrap();
            }
        }
        println!(
            "ssr (half resolution {half_resolution}): {hits_right} of {hits} hits, \
             {misses_right} of {misses} misses right"
        );
        assert!(hits > 800 && misses > 2000, "{hits} hits, {misses} misses");
        let need = if half_resolution { 0.97 } else { 0.99 };
        assert!(
            hits_right as f32 >= hits as f32 * need,
            "{hits_right} of {hits}"
        );
        assert!(
            misses_right as f32 >= misses as f32 * need,
            "{misses_right} of {misses}"
        );
        states.push(ssr);
    }
    bed.finish(Vec::new(), states);
}

/// Whether the ray from `o` along `d` meets box `b` at all (slab test).
fn slab_hit(b: &ProbeBox, o: Vec3, d: Vec3) -> bool {
    let (lo, hi) = b.bounds();
    let (mut t0, mut t1) = (0.0f32, f32::MAX);
    for k in 0..3 {
        if d[k].abs() < 1e-8 {
            if o[k] < lo[k] || o[k] > hi[k] {
                return false;
            }
            continue;
        }
        let (a, c) = ((lo[k] - o[k]) / d[k], (hi[k] - o[k]) / d[k]);
        t0 = t0.max(a.min(c));
        t1 = t1.min(a.max(c));
    }
    t0 <= t1
}
