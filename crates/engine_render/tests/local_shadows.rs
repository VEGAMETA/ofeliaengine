//! Shadows of point and spot lights (ADR 0071) against analytic shadows, and their cache,
//! under the validation layer with synchronization validation.
//!
//! A ground plane, boxes and one light at a time, seen from above. For every pixel showing
//! the ground, a ray towards the light tells exactly whether it is in shadow; the frame
//! drawn with the light's shadow, divided by the frame without, must agree (lit: about 1,
//! shadowed: about 0). Then the cache: a static scene draws each shadow once, a moving
//! caster redraws only the light it touches, a moving light only its own views; the same
//! through the persistent GPU scene's change list.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::too_many_lines)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, Vec4, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, LocalLight,
    LocalShadowSettings, ShadowSettings, Spot,
};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const WIDTH: u32 = 384;
const HEIGHT: u32 = 216;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render local shadow test"),
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

/// An axis-aligned box caster.
#[derive(Clone, Copy)]
struct Block {
    center: Vec3,
    size: Vec3,
}

impl Block {
    /// Distance along the ray to the box, if it hits it (slab test).
    fn hit(&self, origin: Vec3, direction: Vec3) -> Option<f32> {
        let (lo, hi) = (self.center - self.size * 0.5, self.center + self.size * 0.5);
        let (mut near, mut far) = (0.0f32, f32::MAX);
        for axis in 0..3 {
            let (o, d) = (origin[axis], direction[axis]);
            if d.abs() < 1e-9 {
                if o < lo[axis] || o > hi[axis] {
                    return None;
                }
                continue;
            }
            let (a, b) = ((lo[axis] - o) / d, (hi[axis] - o) / d);
            near = near.max(a.min(b));
            far = far.min(a.max(b));
        }
        (near <= far).then_some(near)
    }

    fn draw(
        &self,
        mesh: engine_render::mesh::MeshId,
        material: engine_render::material::MaterialId,
    ) -> DrawItem {
        DrawItem {
            mesh,
            material,
            world_from_object: Mat4::from_scale_rotation_translation(
                self.size,
                Quat::IDENTITY,
                self.center,
            ),
        }
    }
}

/// The test bed: one device, one renderer, frames rendered and read back.
struct Bed {
    instance: Instance,
    device: Device,
    allocator: Allocator,
    bindless: Bindless,
    geometry: GeometryPool,
    textures: TexturePool,
    materials: MaterialTable,
    ring: UploadRing,
    graph: RenderGraph,
    readback: vk::Buffer,
    readback_memory: Option<engine_rhi::Allocation>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    frame_number: u64,
}

const BYTES: u64 = (WIDTH * HEIGHT * 8) as u64;

impl Bed {
    fn new() -> Option<Self> {
        let (instance, device) = gpu()?;
        println!("adapter: {}", device.adapter().summary());
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let textures = TexturePool::new(&device, 1 << 20, 1);
        let materials = MaterialTable::new(1);
        let ring = UploadRing::new(&device, &mut allocator, 1, 8 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let (readback, readback_memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(BYTES)
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
            geometry,
            textures,
            materials,
            ring,
            graph,
            readback,
            readback_memory: Some(readback_memory),
            pool,
            cmd,
            frame_number: 0,
        })
    }

    /// Renders one frame of `draws` (or of `scene`, moved by `edit` first) lit by `lights`
    /// only, and returns each pixel's red channel.
    fn render(
        &mut self,
        renderer: &ForwardRenderer,
        camera: Camera,
        draws: &[DrawItem],
        scene: Option<&mut GpuScene>,
        lights: &[LocalLight],
    ) -> Vec<f32> {
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
        self.frame_number += 1;
        let frame_number = self.frame_number;
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { self.graph.begin_slot(0, cmd) };
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
            let geo = self
                .geometry
                .begin_frame(&mut fg, &self.device, &mut self.allocator, frame_number)
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(HDR, WIDTH, HEIGHT));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, WIDTH, HEIGHT));
            let view = ForwardView {
                camera,
                light: DirectionalLight {
                    direction: Vec3::NEG_Y,
                    color: Vec3::ZERO,
                    ambient: Vec3::ZERO,
                },
                color,
                depth,
                clear: Some([0.0; 4]),
                occlusion: None,
                temporal: None,
                lights,
                environment: None,
            };
            match scene {
                Some(scene) => {
                    let scene_frame = scene
                        .add_passes(
                            &mut fg,
                            &self.device,
                            &mut self.allocator,
                            &mut self.ring,
                            &self.bindless,
                            &geo,
                            &mats,
                            frame_number,
                        )
                        .unwrap()
                        .unwrap();
                    renderer
                        .add_scene_passes(
                            &mut fg,
                            &mut self.ring,
                            &self.bindless,
                            &geo,
                            &mats,
                            &scene_frame,
                            &view,
                        )
                        .unwrap();
                }
                None => {
                    renderer
                        .add_passes(
                            &mut fg,
                            &mut self.ring,
                            &self.bindless,
                            &geo,
                            &mats,
                            draws,
                            &view,
                        )
                        .unwrap();
                }
            }
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: self.readback,
                    size: BYTES,
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
        let ptr = self.readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), BYTES as usize) };
        (0..(WIDTH * HEIGHT) as usize)
            .map(|i| half::to_f32(u16::from_le_bytes([raw[i * 8], raw[i * 8 + 1]])))
            .collect()
    }

    fn finish(mut self, mut renderers: Vec<ForwardRenderer>, scenes: Vec<GpuScene>) {
        // SAFETY: the device is idle.
        unsafe {
            for r in renderers.iter_mut() {
                r.destroy(&self.device, &mut self.allocator, &self.bindless);
            }
            for mut s in scenes {
                s.destroy(&mut self.allocator);
            }
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
        drop(renderers);
        let Self {
            instance,
            device,
            allocator,
            bindless,
            ..
        } = self;
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

fn renderer(bed: &mut Bed, settings: LocalShadowSettings) -> ForwardRenderer {
    let mut renderer = ForwardRenderer::new(
        &bed.device,
        &bed.bindless,
        HDR,
        DEPTH,
        ShadowSettings::default(),
    )
    .unwrap();
    renderer
        .set_local_shadows(
            &bed.device,
            &mut bed.allocator,
            &bed.bindless,
            Some(settings),
        )
        .unwrap();
    renderer
}

/// Mistakes over judged ground pixels.
#[derive(Debug, Default)]
struct Score {
    lit: u32,
    lit_wrong: u32,
    shadow: u32,
    shadow_wrong: u32,
}

#[test]
fn local_shadows_match_analytic_shadows_and_cache() {
    let Some(mut bed) = Bed::new() else {
        return;
    };
    let mesh = bed.geometry.add_mesh(&cube().as_data()).unwrap();
    let gray = bed
        .materials
        .add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 1.0));
    let ground = Block {
        center: Vec3::new(0.0, -0.1, 0.0),
        size: Vec3::new(60.0, 0.2, 60.0),
    };
    let blocks = [
        // Under the spot light.
        Block {
            center: Vec3::new(-4.0, 1.5, 0.0),
            size: Vec3::new(1.0, 0.6, 1.4),
        },
        // Around the point light: a thin pole and a box on the other side.
        Block {
            center: Vec3::new(5.6, 1.0, 0.3),
            size: Vec3::new(0.25, 2.0, 0.25),
        },
        Block {
            center: Vec3::new(3.0, 0.5, -1.5),
            size: Vec3::new(0.8, 1.0, 0.8),
        },
    ];
    let mut draws = vec![ground.draw(mesh, gray)];
    draws.extend(blocks.iter().map(|b| b.draw(mesh, gray)));
    let spot = LocalLight {
        position: Vec3::new(-4.0, 4.0, 0.0),
        range: 9.0,
        color: Vec3::splat(20.0),
        spot: Some(Spot {
            direction: Vec3::new(0.1, -1.0, 0.0).normalize(),
            inner: 0.6,
            outer: 0.9,
        }),
        shadow: true,
    };
    let point = LocalLight {
        position: Vec3::new(4.5, 2.2, 0.0),
        range: 7.0,
        color: Vec3::splat(10.0),
        spot: None,
        shadow: true,
    };
    let eye = Vec3::new(0.0, 13.0, 9.0);
    let clip_from_world = vulkan_perspective(1.0, WIDTH as f32 / HEIGHT as f32, 0.1, 100.0)
        * look_at(eye, Vec3::new(0.0, 0.0, 0.5), Vec3::Y);
    let camera = Camera {
        clip_from_world,
        position: eye,
    };
    // A fixed resolution, so the edge band below is known.
    let settings = LocalShadowSettings {
        min_resolution: 512,
        max_resolution: 512,
        ..LocalShadowSettings::default()
    };

    // The ground point of every pixel (None: not the ground).
    let inverse = clip_from_world.inverse();
    let ground_points: Vec<Option<Vec3>> = (0..WIDTH * HEIGHT)
        .map(|i| {
            let (x, y) = (i % WIDTH, i / WIDTH);
            let ndc = [
                (x as f32 + 0.5) / WIDTH as f32 * 2.0 - 1.0,
                (y as f32 + 0.5) / HEIGHT as f32 * 2.0 - 1.0,
            ];
            let unproject = |z: f32| {
                let p = inverse * Vec4::new(ndc[0], ndc[1], z, 1.0);
                p.truncate() / p.w
            };
            let origin = unproject(0.0);
            let direction = (unproject(1.0) - origin).normalize();
            if direction.y >= -1e-4 {
                return None;
            }
            let t = -origin.y / direction.y;
            if blocks
                .iter()
                .any(|b| b.hit(origin, direction).is_some_and(|h| h < t))
            {
                return None;
            }
            Some(origin + direction * t)
        })
        .collect();

    let shadowed = renderer(&mut bed, settings);
    let plain = {
        let mut r = renderer(&mut bed, settings);
        r.set_local_shadows(&bed.device, &mut bed.allocator, &bed.bindless, None)
            .unwrap();
        r
    };
    for light in [spot, point] {
        let lights = [light];
        let with = bed.render(&shadowed, camera, &draws, None, &lights);
        let without = bed.render(&plain, camera, &draws, None, &lights);
        let mut score = Score::default();
        for (i, ground) in ground_points.iter().enumerate() {
            let Some(p) = *ground else {
                continue;
            };
            let base = without[i];
            if base < 1e-3 {
                // Out of the light's reach or cone.
                continue;
            }
            let shadow_of = |q: Vec3| {
                let to = light.position - q;
                let (direction, distance) = (to.normalize(), to.length());
                blocks.iter().any(|b| {
                    b.hit(q + direction * 1e-3, direction)
                        .is_some_and(|h| h < distance)
                })
            };
            let truth = shadow_of(p);
            // Unjudged near an edge: a few texels of the view at that distance.
            let distance = p.distance(light.position);
            let edge = 0.02 + distance * 0.012;
            let ring = [
                Vec3::X,
                Vec3::NEG_X,
                Vec3::Z,
                Vec3::NEG_Z,
                Vec3::new(0.7, 0.0, 0.7),
                Vec3::new(-0.7, 0.0, 0.7),
                Vec3::new(0.7, 0.0, -0.7),
                Vec3::new(-0.7, 0.0, -0.7),
            ];
            if ring.iter().any(|r| shadow_of(p + *r * edge) != truth) {
                continue;
            }
            let ratio = with[i] / base;
            if truth {
                score.shadow += 1;
                score.shadow_wrong += u32::from(ratio > 0.5);
            } else {
                score.lit += 1;
                score.lit_wrong += u32::from(ratio < 0.5);
            }
        }
        let kind = if light.spot.is_some() {
            "spot"
        } else {
            "point"
        };
        println!("{kind}: {score:?}; {:?}", shadowed.local_shadow_stats());
        assert!(score.shadow > 200 && score.lit > 2000, "{kind}: {score:?}");
        assert!(
            score.shadow_wrong * 200 <= score.shadow && score.lit_wrong * 500 <= score.lit,
            "{kind} light's shadow agrees with the analytic one: {score:?}"
        );
    }

    // The cache, with both lights.
    let cached = renderer(
        &mut bed,
        LocalShadowSettings {
            updates_per_frame: 6,
            ..LocalShadowSettings::default()
        },
    );
    let mut lights = [spot, point];
    let mut views = |bed: &mut Bed, draws: &[DrawItem], lights: &[LocalLight]| {
        bed.render(&cached, camera, draws, None, lights);
        cached.local_shadow_stats().views_drawn
    };
    assert_eq!(views(&mut bed, &draws, &lights), 7, "both drawn once");
    let reference = bed.render(&cached, camera, &draws, None, &lights);
    assert_eq!(cached.local_shadow_stats().views_drawn, 0, "a static scene");
    // A caster near the spot light moves: only the spot light redraws.
    draws[1].world_from_object = Mat4::from_translation(Vec3::new(0.01, 0.0, 0.0))
        * blocks[0].draw(mesh, gray).world_from_object;
    assert_eq!(views(&mut bed, &draws, &lights), 1);
    assert_eq!(views(&mut bed, &draws, &lights), 0);
    // And back: the frame is the reference again.
    draws[1] = blocks[0].draw(mesh, gray);
    views(&mut bed, &draws, &lights);
    let again = bed.render(&cached, camera, &draws, None, &lights);
    assert_eq!(again, reference, "a redrawn shadow is the same");
    // The point light moves: its six faces.
    lights[1].position.y += 0.05;
    assert_eq!(views(&mut bed, &draws, &lights), 6);
    assert_eq!(views(&mut bed, &draws, &lights), 0);

    // The same through the persistent scene's change list.
    let mut scene = GpuScene::new(&bed.device, &bed.bindless, 64, 1).unwrap();
    let ids: Vec<_> = draws
        .iter()
        .map(|d| {
            scene.insert(SceneInstance {
                mesh: d.mesh,
                material: d.material,
                world_from_object: d.world_from_object,
            })
        })
        .collect();
    let scene_renderer = renderer(&mut bed, LocalShadowSettings::default());
    let mut scene_views = |bed: &mut Bed, scene: &mut GpuScene| {
        bed.render(&scene_renderer, camera, &[], Some(scene), &lights);
        scene_renderer.local_shadow_stats().views_drawn
    };
    assert_eq!(scene_views(&mut bed, &mut scene), 7);
    assert_eq!(scene_views(&mut bed, &mut scene), 0);
    // The pole next to the point light moves.
    scene.set_transform(
        ids[2],
        &(Mat4::from_translation(Vec3::new(0.0, 0.0, 0.02)) * draws[2].world_from_object),
    );
    assert_eq!(scene_views(&mut bed, &mut scene), 6);
    assert_eq!(scene_views(&mut bed, &mut scene), 0);
    // Something far from both lights moves: nothing.
    let far = scene.insert(SceneInstance {
        mesh,
        material: gray,
        world_from_object: Mat4::from_translation(Vec3::new(25.0, 0.5, 25.0)),
    });
    assert_eq!(scene_views(&mut bed, &mut scene), 0);
    scene.set_transform(far, &Mat4::from_translation(Vec3::new(25.0, 0.5, 24.0)));
    assert_eq!(scene_views(&mut bed, &mut scene), 0);
    // A removal near the spot light.
    scene.remove(ids[1]);
    assert_eq!(scene_views(&mut bed, &mut scene), 1);

    bed.finish(vec![shadowed, plain, cached, scene_renderer], vec![scene]);
}
