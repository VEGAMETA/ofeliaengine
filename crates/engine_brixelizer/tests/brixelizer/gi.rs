//! Brixelizer GI in the forward renderer (ADR 0090), under the validation layer with
//! synchronization validation:
//!
//! - an open floor under a uniform sky receives irradiance π × the sky's radiance: the
//!   SDK's diffuse light is irradiance;
//! - a white wall standing in the sun lights the floor in front of it, through the radiance
//!   cache, which holds the wall's lit radiance;
//! - the distance field has no holes: built by AMD's compiler, the vendored SDK left whole
//!   triangles of a floor out (`third_party/README.md`);
//! - a view without motion vectors gets the camera's motion made from the depth, and the
//!   same light as with the renderer's.

use engine_brixelizer::{
    Brixelizer, BrixelizerGi, DebugView, GiSettings, InternalResolution, SdfDebugView, SdfSettings,
};
use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
    ViewGi,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation,
};

const SIZE: u32 = 512;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const PI: f32 = core::f32::consts::PI;
/// Two RGBA16F images read back: the GI shading's irradiance, then a [`Second`] image.
const READBACK_BYTES: u64 = (SIZE * SIZE * 8 * 2) as u64;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_brixelizer test"),
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

/// A scene of boxes: each a cube mesh scaled, placed and colored.
struct BoxDesc {
    center: Vec3,
    size: Vec3,
    albedo: [f32; 3],
}

/// The second image a frame reads back.
#[derive(Clone, Copy)]
enum Second {
    /// The lit image.
    Lit,
    /// A view of the distance field.
    Distance(SdfDebugView),
    /// A view of the GI's caches.
    Cache(DebugView),
}

/// The pixels a frame reads back: the GI shading's irradiance (rgb, distance) and the
/// [`Second`] image (rgb).
struct Frame {
    irradiance: Vec<[f32; 4]>,
    second: Vec<[f32; 3]>,
}

impl Frame {
    /// The second image at a pixel.
    fn second_at(&self, (x, y): (u32, u32)) -> Vec3 {
        Vec3::from(self.second[(y * SIZE + x) as usize])
    }
}

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
    temporal: Temporal,
    environment: Environment,
    sdf: Brixelizer,
    gi: BrixelizerGi,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    frame: u64,
    draws: Vec<DrawItem>,
    /// The forward renderer's motion vectors (with jitter); without, the GI makes the
    /// camera's motion itself.
    motion_vectors: bool,
}

impl Harness {
    /// The boxes under a uniform sky of `sky` radiance.
    fn new(boxes: &[BoxDesc], sky: Vec3) -> Option<Self> {
        let (instance, device) = gpu()?;
        println!("adapter: {}", device.adapter().summary());
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let mut renderer =
            ForwardRenderer::new(&device, &bindless, HDR, DEPTH, ShadowSettings::default())
                .unwrap();
        renderer
            .set_global_illumination(&device, &bindless, true)
            .unwrap();
        let mut geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let mesh: MeshId = geometry.add_mesh(&cube().as_data()).unwrap();
        let textures = TexturePool::new(&device, 1 << 20, 1);
        let mut materials = MaterialTable::new(1);
        let draws: Vec<DrawItem> = boxes
            .iter()
            .map(|b| DrawItem {
                mesh,
                material: materials.add(MaterialDesc::solid(b.albedo, 0.0, 1.0)),
                world_from_object: Mat4::from_scale_rotation_translation(
                    b.size,
                    Quat::IDENTITY,
                    b.center,
                ),
            })
            .collect();
        let ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let temporal = Temporal::new(&device, &mut allocator, 64).unwrap();
        let environment =
            Environment::new(&device, &mut allocator, &bindless, Sky::uniform(sky)).unwrap();
        let mut sdf = Brixelizer::new(
            &device,
            &mut allocator,
            SdfSettings {
                cascades: 4,
                voxel_size: 0.1,
                max_references: 1 << 20,
                triangle_swap_size: 16 << 20,
                max_views: 1,
                ..SdfSettings::default()
            },
        )
        .unwrap();
        let gi = BrixelizerGi::new(
            &device,
            &mut allocator,
            &bindless,
            &sdf,
            SIZE,
            SIZE,
            GiSettings {
                internal_resolution: InternalResolution::Percent50,
                ..GiSettings::default()
            },
        )
        .unwrap();
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
        let instances: Vec<(MeshId, Mat4)> = draws
            .iter()
            .map(|d| (d.mesh, d.world_from_object))
            .collect();
        sdf.set_instances(&geometry, &instances).unwrap();
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
            temporal,
            environment,
            sdf,
            gi,
            readback,
            readback_memory: Some(readback_memory),
            pool,
            cmd,
            frame: 0,
            draws,
            motion_vectors: true,
        })
    }

    /// Renders one frame seen from `eye` towards `target` (`up` the camera's up) with the
    /// sun `light`, and reads the irradiance and the `second` image back.
    fn render(
        &mut self,
        (eye, target, up): (Vec3, Vec3, Vec3),
        light: DirectionalLight,
        second: Second,
    ) -> Frame {
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
            temporal,
            environment,
            sdf,
            gi,
            readback,
            draws,
            motion_vectors,
            ..
        } = self;
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
            let camera = Camera {
                clip_from_world: vulkan_perspective(1.0, 1.0, 0.05, 200.0)
                    * look_at(eye, target, up),
                position: eye,
            };
            let (sdf_frame, distance_view) = match second {
                Second::Distance(view) => {
                    sdf.add_update_pass_with_debug(&mut fg, &geo, eye, camera, view, (SIZE, SIZE))
                }
                _ => (sdf.add_update_pass(&mut fg, &geo, eye), None),
            };
            gi.begin_frame(&sdf_frame);
            let color = fg.create_texture("color", TextureDesc::new_2d(HDR, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, SIZE, SIZE));
            let passes = renderer
                .add_passes(
                    &mut fg,
                    ring,
                    bindless,
                    &geo,
                    &mats,
                    draws,
                    &ForwardView {
                        camera,
                        light,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: motion_vectors.then_some(&*temporal),
                        lights: &[],
                        decals: &[],
                        environment: Some(&*environment),
                        reflections: Default::default(),
                        gi: Some(ViewGi::Screen(&*gi)),
                    },
                )
                .unwrap();
            let second = match second {
                Second::Lit => color,
                Second::Distance(_) => distance_view.expect("the distance field's view"),
                Second::Cache(view) => gi.add_debug_pass(&mut fg, view).expect("the cache's view"),
            };
            gi.add_capture_pass(&mut fg, color);
            // None until the distance field is built.
            let irradiance = passes.gi;
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
            let copy = |offset: u64| vk::BufferImageCopy {
                buffer_offset: offset,
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
            let half_bytes = READBACK_BYTES / 2;
            let mut pass = fg
                .add_pass("read back")
                .read(second, Access::TransferRead)
                .write(out, Access::TransferWrite);
            if let Some(irradiance) = irradiance {
                pass = pass.read(irradiance, Access::TransferRead);
            }
            pass.execute(move |ctx| {
                // SAFETY: recording; layouts arranged by the graph.
                unsafe {
                    match irradiance {
                        Some(irradiance) => ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(irradiance),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[copy(0)],
                        ),
                        None => {
                            ctx.device
                                .cmd_fill_buffer(ctx.cmd, ctx.buffer(out), 0, half_bytes, 0)
                        }
                    }
                    ctx.device.cmd_copy_image_to_buffer(
                        ctx.cmd,
                        ctx.image(second),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        ctx.buffer(out),
                        &[copy(half_bytes)],
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
        assert_eq!(self.sdf.failures(), 0, "distance field updates refused");
        assert_eq!(self.gi.failures(), 0, "GI dispatches refused");
        let ptr = self.readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers both images.
        let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), READBACK_BYTES as usize) };
        let texel = |p: &[u8], k: usize| half::to_f32(u16::from_le_bytes([p[2 * k], p[2 * k + 1]]));
        let (first, second) = raw.split_at(raw.len() / 2);
        Frame {
            irradiance: first
                .chunks_exact(8)
                .map(|p| [texel(p, 0), texel(p, 1), texel(p, 2), texel(p, 3)])
                .collect(),
            second: second
                .chunks_exact(8)
                .map(|p| [texel(p, 0), texel(p, 1), texel(p, 2)])
                .collect(),
        }
    }

    /// Frees everything and checks the validation layer saw no error.
    fn finish(mut self) {
        let frame = self.frame + 1;
        // SAFETY: the device is idle.
        unsafe {
            self.gi
                .destroy(&self.device, &mut self.allocator, &self.bindless, frame);
            self.sdf.destroy(&self.device, &mut self.allocator);
            self.environment
                .destroy(&mut self.allocator, &self.bindless);
            self.temporal.destroy(&mut self.allocator);
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
            self.renderer
                .destroy(&self.device, &mut self.allocator, &self.bindless);
        }
        let Self {
            instance,
            device,
            allocator,
            bindless,
            renderer,
            environment,
            sdf,
            gi,
            ..
        } = self;
        drop(gi);
        drop(sdf);
        drop(environment);
        drop(renderer);
        drop(bindless);
        drop(allocator);
        drop(device);
        let report = instance.validation_report();
        assert_eq!(
            report.errors, 0,
            "validation errors: {:#?}",
            report.first_errors
        );
    }
}

/// The mean of the irradiance (rgb) over the pixels of the central half of the image
/// whose surface is within `max_distance`.
fn central_mean(frame: &Frame, max_distance: f32) -> Vec3 {
    let mut sum = Vec3::ZERO;
    let mut n = 0.0;
    for y in SIZE / 4..3 * SIZE / 4 {
        for x in SIZE / 4..3 * SIZE / 4 {
            let p = frame.irradiance[(y * SIZE + x) as usize];
            if p[3] > 0.0 && p[3] < max_distance {
                sum += Vec3::new(p[0], p[1], p[2]);
                n += 1.0;
            }
        }
    }
    assert!(n > 0.0, "no surface in the image's centre");
    sum / n
}

const NO_SUN: DirectionalLight = DirectionalLight {
    direction: Vec3::new(-0.3, -1.0, -0.2),
    color: Vec3::ZERO,
    ambient: Vec3::ZERO,
};

#[test]
fn an_open_floor_under_a_uniform_sky_receives_pi_times_its_radiance() {
    // A floor of 40 × 40 m seen from 3 m above under a sky of radiance 1: every ray of the
    // floor's hemisphere reaches the sky, whose irradiance is π. The SDK's diffuse light
    // gets 3.19 to 3.21 on both GPUs (2.19 at 128 × 128: the screen probes need a view
    // larger than a few tiles).
    let floor = BoxDesc {
        center: Vec3::new(0.0, -0.1, 0.0),
        size: Vec3::new(40.0, 0.2, 40.0),
        albedo: [0.8; 3],
    };
    let Some(mut h) = Harness::new(&[floor], Vec3::ONE) else {
        return;
    };
    let camera = (Vec3::new(0.0, 3.0, 0.0), Vec3::ZERO, Vec3::Z);
    let mut last = None;
    for _ in 0..48 {
        last = Some(h.render(camera, NO_SUN, Second::Lit));
    }
    let mean = central_mean(&last.unwrap(), 10.0);
    println!("irradiance under the sky {mean:?}");
    h.finish();
    assert!((mean.x - PI).abs() < 0.1 * PI, "not π: {mean:?}");
}

/// The pixel `point` falls on for the camera at `eye` looking at `target`, if in view.
fn pixel_of((eye, target, up): (Vec3, Vec3, Vec3), point: Vec3) -> Option<(u32, u32)> {
    let clip = vulkan_perspective(1.0, 1.0, 0.05, 200.0) * look_at(eye, target, up);
    let h = clip * point.extend(1.0);
    let ndc = h.truncate() / h.w;
    let x = (ndc.x * 0.5 + 0.5) * SIZE as f32;
    let y = (ndc.y * 0.5 + 0.5) * SIZE as f32;
    (h.w > 0.0 && (0.0..SIZE as f32).contains(&x) && (0.0..SIZE as f32).contains(&y))
        .then_some((x as u32, y as u32))
}

/// The irradiance (rgb) around a pixel: the mean of its 3 × 3 block.
fn irradiance_at(frame: &Frame, (x, y): (u32, u32)) -> Vec3 {
    let mut sum = Vec3::ZERO;
    for dy in 0..3 {
        for dx in 0..3 {
            let px = (x + dx).saturating_sub(1).min(SIZE - 1);
            let py = (y + dy).saturating_sub(1).min(SIZE - 1);
            let p = frame.irradiance[(py * SIZE + px) as usize];
            sum += Vec3::new(p[0], p[1], p[2]);
        }
    }
    sum / 9.0
}

/// A white wall 8 m wide and 4 m tall facing the sun on a grey floor, under a black sky,
/// and a camera in front of it, a little above.
fn sunlit_wall() -> (Vec<BoxDesc>, DirectionalLight, (Vec3, Vec3, Vec3)) {
    let floor = BoxDesc {
        center: Vec3::new(0.0, -0.1, 0.0),
        size: Vec3::new(40.0, 0.2, 40.0),
        albedo: [0.5; 3],
    };
    let wall = BoxDesc {
        center: Vec3::new(0.0, 2.0, -2.0),
        size: Vec3::new(8.0, 4.0, 0.2),
        albedo: [0.9; 3],
    };
    let sun = DirectionalLight {
        direction: Vec3::new(0.0, -0.6, -0.8).normalize(),
        color: Vec3::splat(3.0),
        ambient: Vec3::ZERO,
    };
    let camera = (Vec3::new(0.0, 3.0, 7.0), Vec3::new(0.0, 0.5, -1.0), Vec3::Y);
    (vec![floor, wall], sun, camera)
}

#[test]
fn a_sunlit_wall_lights_the_floor_in_front_of_it() {
    // The wall's radiance is 0.9 × 3 × 0.8 / π ≈ 0.69 (0.71 in the lit image, with the
    // floor's light), and a floor point 0.4 m in front of it sees the wall over 44 % of its
    // cosine-weighted hemisphere: irradiance ≈ 0.96; 5 m away, ≈ 0.17. Brixelizer GI gives
    // a quarter of that on both GPUs: 0.22 to 0.23 after 64 frames, 0.08 away. Its radiance cache holds the wall's radiance, but a ray reads a hit brick's
    // harmonics through a cosine lobe around the surface's normal, the radiance times the
    // cosine of the hit (0.45 on average here), and the screen probes lose the rest.
    let (boxes, sun, camera) = sunlit_wall();
    let Some(mut h) = Harness::new(&boxes, Vec3::ZERO) else {
        return;
    };
    let mut last = None;
    for _ in 0..64 {
        last = Some(h.render(camera, sun, Second::Lit));
    }
    let lit = last.unwrap();
    let cache = h.render(camera, sun, Second::Cache(DebugView::RadianceCache));
    let near = irradiance_at(&lit, pixel_of(camera, Vec3::new(0.0, 0.0, -1.5)).unwrap());
    let far = irradiance_at(&lit, pixel_of(camera, Vec3::new(0.0, 0.0, 3.0)).unwrap());
    let wall = pixel_of(camera, Vec3::new(0.0, 2.0, -1.9)).unwrap();
    let (wall_lit, wall_cache) = (lit.second_at(wall), cache.second_at(wall));
    println!(
        "irradiance in front of the wall {near:?}, away from it {far:?}; the wall lit \
         {wall_lit:?}, in the radiance cache {wall_cache:?}"
    );
    h.finish();
    assert!(near.x > 0.15, "in front of the wall: {near:?}");
    assert!(
        near.x > 2.5 * far.x,
        "in front {near:?} against away {far:?}"
    );
    assert!(
        (wall_cache.x - wall_lit.x).abs() < 0.1 * wall_lit.x,
        "the radiance cache holds {wall_cache:?} where the wall is lit {wall_lit:?}"
    );
}

#[test]
fn the_distance_field_has_no_holes() {
    // The distance field's normals (n / 2 + 1 / 2) on a grid of floor points in front of
    // the wall, once every cascade is built: all up. Built by AMD's compiler, the vendored
    // SDK left a floor's triangles out of whole bricks (`third_party/README.md`): the
    // camera saw through the floor to the sky.
    let (boxes, sun, camera) = sunlit_wall();
    let Some(mut h) = Harness::new(&boxes, Vec3::ZERO) else {
        return;
    };
    let mut last = None;
    for _ in 0..8 {
        last = Some(h.render(camera, sun, Second::Distance(SdfDebugView::Normals)));
    }
    let frame = last.unwrap();
    let up = Vec3::new(0.5, 1.0, 0.5);
    let mut points = 0;
    let mut holes = Vec::new();
    for z in -6..=20 {
        for x in -12..=12 {
            let point = Vec3::new(x as f32 * 0.25, 0.0, z as f32 * 0.25);
            let Some(pixel) = pixel_of(camera, point) else {
                continue;
            };
            points += 1;
            let normal = frame.second_at(pixel);
            if (normal - up).abs().max_element() > 0.1 {
                holes.push((point, normal));
            }
        }
    }
    h.finish();
    assert!(points > 400, "{points} floor points in view");
    assert!(
        holes.is_empty(),
        "{} of {points} floor points: {holes:?}",
        holes.len()
    );
}

#[test]
fn a_view_without_motion_vectors_gets_the_cameras_motion() {
    // The sunlit wall seen from a camera panning 1 cm a frame, with the forward renderer's
    // motion vectors and without (the GI's own, from the depth): the same light in front
    // of the wall.
    let (boxes, sun, (eye, target, up)) = sunlit_wall();
    let mut near = Vec::new();
    for motion_vectors in [true, false] {
        let Some(mut h) = Harness::new(&boxes, Vec3::ZERO) else {
            return;
        };
        h.motion_vectors = motion_vectors;
        let mut last = None;
        for k in 0..64 {
            let pan = Vec3::new(0.01 * k as f32, 0.0, 0.0);
            last = Some(h.render((eye + pan, target + pan, up), sun, Second::Lit));
        }
        let pan = Vec3::new(0.63, 0.0, 0.0);
        let point = Vec3::new(0.63, 0.0, -1.5);
        let pixel = pixel_of((eye + pan, target + pan, up), point).unwrap();
        near.push(irradiance_at(&last.unwrap(), pixel));
        h.finish();
    }
    println!(
        "in front of the wall, renderer's motion {:?}, own {:?}",
        near[0], near[1]
    );
    assert!(near[1].x > 0.1, "without motion vectors: {:?}", near[1]);
    assert!(
        (near[1].x - near[0].x).abs() < 0.15 * near[0].x,
        "renderer's motion {:?} against own {:?}",
        near[0],
        near[1]
    );
}

#[test]
fn the_sky_does_not_light_the_inside_of_a_closed_box() {
    // A closed room of 0.2 m walls, 6 × 3 × 6 m inside, under a sky of radiance 1 and no
    // sun, seen from inside: no ray from inside reaches the sky and nothing lights the
    // walls, so no light. Had the GI run before the distance field was built, the sky would
    // still light a quarter of it after 64 frames (0.74, decaying by half every 48 frames).
    let t = 0.2;
    let walls = [
        (
            Vec3::new(0.0, -t / 2.0, 0.0),
            Vec3::new(6.0 + 2.0 * t, t, 6.0 + 2.0 * t),
        ),
        (
            Vec3::new(0.0, 3.0 + t / 2.0, 0.0),
            Vec3::new(6.0 + 2.0 * t, t, 6.0 + 2.0 * t),
        ),
        (
            Vec3::new(-3.0 - t / 2.0, 1.5, 0.0),
            Vec3::new(t, 3.0, 6.0 + 2.0 * t),
        ),
        (
            Vec3::new(3.0 + t / 2.0, 1.5, 0.0),
            Vec3::new(t, 3.0, 6.0 + 2.0 * t),
        ),
        (
            Vec3::new(0.0, 1.5, -3.0 - t / 2.0),
            Vec3::new(6.0 + 2.0 * t, 3.0, t),
        ),
        (
            Vec3::new(0.0, 1.5, 3.0 + t / 2.0),
            Vec3::new(6.0 + 2.0 * t, 3.0, t),
        ),
    ];
    let boxes: Vec<BoxDesc> = walls
        .iter()
        .map(|&(center, size)| BoxDesc {
            center,
            size,
            albedo: [0.8; 3],
        })
        .collect();
    let Some(mut h) = Harness::new(&boxes, Vec3::ONE) else {
        return;
    };
    let camera = (Vec3::new(0.0, 1.5, 2.5), Vec3::new(0.0, 0.5, -2.0), Vec3::Y);
    let mut last = None;
    for _ in 0..64 {
        last = Some(h.render(camera, NO_SUN, Second::Lit));
    }
    let mean = central_mean(&last.unwrap(), 20.0);
    println!("irradiance inside the closed box {mean:?}");
    h.finish();
    assert!(mean.x < 0.01 * PI, "the sky leaks in: {mean:?}");
}
