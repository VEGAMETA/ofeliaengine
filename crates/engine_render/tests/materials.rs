//! Materials and textures (ADR 0040) through the forward renderer, under the validation
//! layer with synchronization validation: texture sampling (sRGB, wrap modes), alpha masks
//! and their shadows, double-sided and mirrored geometry, normal maps in glTF's tangent
//! convention, textures streaming in within the upload budget, and edits and removals of
//! materials.
//!
//! Most checks use emission only (no light), so a pixel is exactly the texel it shows: an
//! 8 × 8 texture made of 2 × 2 blocks is sampled at block centers, where bilinear filtering
//! blends identical texels, and an sRGB target encodes back what the sRGB texture decoded.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3, Vec4, look_at, vulkan_orthographic};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable, TextureRef};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, OwnedMeshData};
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation, Wrap,
};

const SIZE: u32 = 64;
const FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render materials test"),
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

/// A unit quad in the XY plane facing +Z, texture coordinates from (0, 0) at the top left
/// to (`u`, 1) at the bottom right; tangent along +X (glTF: the bitangent points up the
/// image, towards decreasing v).
fn quad(u: f32) -> OwnedMeshData {
    OwnedMeshData {
        positions: vec![
            [-0.5, -0.5, 0.0],
            [0.5, -0.5, 0.0],
            [0.5, 0.5, 0.0],
            [-0.5, 0.5, 0.0],
        ],
        normals: vec![[0.0, 0.0, 1.0]; 4],
        tangents: vec![[1.0, 0.0, 0.0, 1.0]; 4],
        uvs: vec![[0.0, 1.0], [u, 1.0], [u, 0.0], [0.0, 0.0]],
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

/// An 8 × 8 RGBA texture of 4 × 4 blocks, one level.
fn blocks(srgb: bool, color: impl Fn(u32, u32) -> [u8; 4]) -> TextureLevels {
    let mut texels = Vec::with_capacity(8 * 8 * 4);
    for y in 0..8 {
        for x in 0..8 {
            texels.extend_from_slice(&color(x / 2, y / 2));
        }
    }
    TextureLevels {
        width: 8,
        height: 8,
        format: if srgb {
            TextureFormat::Rgba8Srgb
        } else {
            TextureFormat::Rgba8Unorm
        },
        levels: vec![texels],
    }
}

/// `levels` block-compressed for `usage` by the asset pipeline (BC7 or BC5).
fn compressed(levels: &TextureLevels, usage: engine_assets::TextureUsage) -> TextureLevels {
    let image = engine_assets::TextureImage {
        width: levels.width,
        height: levels.height,
        srgb: levels.format == TextureFormat::Rgba8Srgb,
        levels: levels.levels.clone(),
    };
    let encoded = engine_assets::compress(&image, usage, None);
    TextureLevels {
        width: encoded.width,
        height: encoded.height,
        format: match encoded.format {
            engine_assets::TextureFormat::Bc7Srgb => TextureFormat::Bc7Srgb,
            engine_assets::TextureFormat::Bc7Unorm => TextureFormat::Bc7Unorm,
            engine_assets::TextureFormat::Bc5Unorm => TextureFormat::Bc5Unorm,
            other => panic!("not compressed: {other:?}"),
        },
        levels: encoded.levels,
    }
}

/// Distinct colors for block (bx, by).
fn block_color(bx: u32, by: u32) -> [u8; 4] {
    [
        (40 + bx * 60) as u8,
        (200 - by * 50) as u8,
        (30 + (bx + by) * 25) as u8,
        255,
    ]
}

/// A camera looking down -Z at the unit square around the origin: pixel (x, y) sees
/// (x + 0.5, y + 0.5) / SIZE of the quad, rows from the top.
fn front_camera() -> Camera {
    let eye = Vec3::new(0.0, 0.0, 5.0);
    Camera {
        clip_from_world: vulkan_orthographic(-0.5, 0.5, -0.5, 0.5, 0.1, 20.0)
            * look_at(eye, Vec3::ZERO, Vec3::Y),
        position: eye,
    }
}

const DARK: DirectionalLight = DirectionalLight {
    direction: Vec3::NEG_Y,
    color: Vec3::ZERO,
    ambient: Vec3::ZERO,
};

/// Emission only: the pixel is the texture (times `color`).
fn emissive(texture: Option<TextureRef>, color: [f32; 3]) -> MaterialDesc {
    MaterialDesc {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: color,
        emissive_texture: texture,
        ..MaterialDesc::default()
    }
}

struct Scene {
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
    command_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
    frame_number: u64,
}

impl Scene {
    fn new(instance: Instance, device: Device, upload_budget: u64) -> Self {
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let renderer = ForwardRenderer::new(
            &device,
            &bindless,
            FORMAT,
            DEPTH,
            ShadowSettings {
                resolution: 1024,
                ..ShadowSettings::default()
            },
        )
        .unwrap();
        let geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let textures = TexturePool::new(&device, upload_budget, 1);
        let ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let (readback, readback_memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(u64::from(SIZE * SIZE * 4))
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();
        let d = device.raw();
        // SAFETY: plain object creation.
        let (command_pool, cmd) = unsafe {
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
        Self {
            instance,
            device,
            allocator,
            bindless,
            renderer,
            geometry,
            textures,
            materials: MaterialTable::new(1),
            ring,
            graph,
            command_pool,
            cmd,
            readback,
            readback_memory: Some(readback_memory),
            frame_number: 0,
        }
    }

    fn mesh(&mut self, data: &OwnedMeshData) -> MeshId {
        self.geometry.add_mesh(&data.as_data()).unwrap()
    }

    fn texture(&mut self, levels: TextureLevels) -> TextureId {
        self.textures
            .add(&self.device, &mut self.allocator, levels)
            .unwrap()
    }

    /// Renders one frame and returns its pixels (RGBA, sRGB-encoded, rows from the top).
    fn render(&mut self, draws: &[DrawItem], camera: Camera, light: DirectionalLight) -> Vec<u8> {
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
            cmd,
            readback,
            readback_memory,
            frame_number,
            ..
        } = self;
        let (d, cmd) = (device.raw(), *cmd);
        let size = u64::from(SIZE * SIZE * 4);
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
        ring.begin_slot(0);
        *frame_number += 1;
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            textures
                .begin_frame(&mut fg, allocator, bindless, *frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, device, allocator, textures, *frame_number)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, device, allocator, *frame_number)
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(FORMAT, SIZE, SIZE));
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
                        light,
                        color,
                        depth,
                        clear: Some([0.0, 0.0, 0.0, 1.0]),
                        occlusion: None,
                        temporal: None,
                        lights: &[],
                    },
                )
                .unwrap();
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: *readback,
                    size,
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
            fg.execute_with_bindless(allocator, Some(bindless)).unwrap();
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
        let ptr = readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        unsafe { core::slice::from_raw_parts(ptr.as_ptr(), size as usize) }.to_vec()
    }

    fn finish(mut self) {
        // SAFETY: the device is idle.
        unsafe {
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.ring.destroy(&mut self.allocator);
            self.geometry.destroy(&mut self.allocator);
            self.materials.destroy(&mut self.allocator);
            self.textures.destroy(&mut self.allocator, &self.bindless);
            if let Some(memory) = self.readback_memory.take() {
                self.allocator.destroy_buffer(self.readback, memory);
            }
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
            ..
        } = self;
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

/// The pixel at (x, y).
fn pixel(image: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * SIZE + x) * 4) as usize;
    [image[i], image[i + 1], image[i + 2], image[i + 3]]
}

/// The pixel where block (bx, by) of an 8 × 8 texture spread over the view is centred.
fn block_pixel(image: &[u8], bx: u32, by: u32) -> [u8; 4] {
    pixel(image, bx * SIZE / 4 + SIZE / 8, by * SIZE / 4 + SIZE / 8)
}

/// Whether two colors differ by at most `tolerance` per channel.
fn near(a: [u8; 4], b: [u8; 4], tolerance: u8) -> bool {
    a.iter().zip(b).all(|(&x, y)| x.abs_diff(y) <= tolerance)
}

/// The brightness of a pixel (sum of RGB).
fn luma(p: [u8; 4]) -> u32 {
    u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])
}

#[test]
fn materials_and_textures() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let mut scene = Scene::new(instance, device, 64 << 20);
    let quad_mesh = scene.mesh(&quad(1.0));
    let wide = scene.mesh(&quad(2.0));
    let picture = scene.texture(blocks(true, block_color));
    let camera = front_camera();
    let draw = |mesh, material, world_from_object| DrawItem {
        mesh,
        material,
        world_from_object,
    };

    // 1. The texture as it is: every block, sRGB round trip within one step.
    let shown = scene
        .materials
        .add(emissive(Some(TextureRef::new(picture)), [1.0; 3]));
    let image = scene.render(&[draw(quad_mesh, shown, Mat4::IDENTITY)], camera, DARK);
    for by in 0..4 {
        for bx in 0..4 {
            let got = block_pixel(&image, bx, by);
            assert!(
                near(got, block_color(bx, by), 1),
                "block ({bx}, {by}): {got:?}, texel {:?}",
                block_color(bx, by)
            );
        }
    }

    // 2. Wrap modes: the right half of a quad with u from 0 to 2 lies beyond the texture.
    // Pixel x = 35 + 8b shows u = 1 + b/4 + 0.11: block b repeated, the edge block
    // clamped, block 3 - b mirrored.
    for (wrap, expected) in [
        (Wrap::Repeat, [0, 1, 2, 3]),
        (Wrap::ClampToEdge, [3, 3, 3, 3]),
        (Wrap::MirroredRepeat, [3, 2, 1, 0]),
    ] {
        let material = scene.materials.add(emissive(
            Some(TextureRef {
                texture: picture,
                wrap: [wrap, Wrap::Repeat],
            }),
            [1.0; 3],
        ));
        let image = scene.render(&[draw(wide, material, Mat4::IDENTITY)], camera, DARK);
        for (b, &block) in expected.iter().enumerate() {
            let got = pixel(&image, 35 + 8 * b as u32, SIZE / 8);
            assert!(
                near(got, block_color(block, 0), 1),
                "{wrap:?} at u = {}: {got:?}, expected block {block}",
                1.0 + b as f32 / 4.0 + 0.11
            );
        }
    }

    // 3. Alpha mask: the left half of the texture is transparent. A green quad behind shows
    // through the masked half, and only there.
    let mask = scene.texture(blocks(true, |bx, _| {
        [255, 0, 0, if bx < 2 { 0 } else { 255 }]
    }));
    let masked = scene.materials.add(MaterialDesc {
        base_color_texture: Some(TextureRef::new(mask)),
        alpha: AlphaMode::Mask { cutoff: 0.5 },
        ..emissive(None, [1.0, 0.0, 0.0])
    });
    let behind = scene.materials.add(emissive(None, [0.0, 1.0, 0.0]));
    let back = Mat4::from_translation(Vec3::new(0.0, 0.0, -1.0));
    let image = scene.render(
        &[
            draw(quad_mesh, masked, Mat4::IDENTITY),
            draw(quad_mesh, behind, back),
        ],
        camera,
        DARK,
    );
    assert_eq!(block_pixel(&image, 0, 1), [0, 255, 0, 255], "cut out");
    assert_eq!(block_pixel(&image, 3, 1), [255, 0, 0, 255], "kept");

    // 4. Its shadow: sunlight straight down through a masked plane onto the ground. Under
    // the cut-out half the ground is lit; under the kept half, and under the same plane
    // opaque, it is in shadow.
    let lit_ground = scene
        .materials
        .add(MaterialDesc::solid([0.8, 0.8, 0.8], 0.0, 0.9));
    let masked_lit = scene.materials.add(MaterialDesc {
        base_color_texture: Some(TextureRef::new(mask)),
        alpha: AlphaMode::Mask { cutoff: 0.5 },
        double_sided: true,
        ..MaterialDesc::solid([1.0, 0.0, 0.0], 0.0, 0.9)
    });
    let opaque_lit = scene.materials.add(MaterialDesc {
        double_sided: true,
        ..MaterialDesc::solid([1.0, 0.0, 0.0], 0.0, 0.9)
    });
    // Planes facing up: the quad rotated from +Z to +Y.
    let flat = Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2);
    let ground = Mat4::from_scale_rotation_translation(Vec3::splat(4.0), flat, Vec3::ZERO);
    let roof = Mat4::from_scale_rotation_translation(
        Vec3::new(2.0, 0.6, 1.0),
        flat,
        Vec3::new(0.0, 1.0, 0.0),
    );
    let eye = Vec3::new(0.0, 5.0, 3.0);
    let clip_from_world = engine_core::math::vulkan_perspective(0.9, 1.0, 0.5, 20.0)
        * look_at(eye, Vec3::ZERO, Vec3::Y);
    let sun = DirectionalLight {
        direction: Vec3::NEG_Y,
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.02),
    };
    let at = |image: &[u8], world: Vec3| {
        let clip = clip_from_world * Vec4::new(world.x, world.y, world.z, 1.0);
        let ndc = clip.truncate() / clip.w;
        let x = ((ndc.x * 0.5 + 0.5) * SIZE as f32) as u32;
        let y = ((ndc.y * 0.5 + 0.5) * SIZE as f32) as u32;
        pixel(image, x.min(SIZE - 1), y.min(SIZE - 1))
    };
    let (under_cut, under_kept) = (Vec3::new(-0.5, 0.0, 0.0), Vec3::new(0.5, 0.0, 0.0));
    let mut shadows = Vec::new();
    for roof_material in [masked_lit, opaque_lit] {
        let image = scene.render(
            &[
                draw(quad_mesh, lit_ground, ground),
                draw(quad_mesh, roof_material, roof),
            ],
            Camera {
                clip_from_world,
                position: eye,
            },
            sun,
        );
        shadows.push((at(&image, under_cut), at(&image, under_kept)));
    }
    println!("ground under the masked plane (cut, kept), then under the opaque one: {shadows:?}");
    let ((cut, kept), (opaque_cut, opaque_kept)) = (shadows[0], shadows[1]);
    assert!(
        luma(cut) > 4 * luma(kept).max(1),
        "light passes the cut-out half"
    );
    assert!(
        luma(opaque_cut) * 4 < luma(cut),
        "the opaque plane shadows both halves"
    );
    assert!(
        near(kept, opaque_kept, 2),
        "the kept half shadows like an opaque plane"
    );

    // 5. Faces: a quad turned away from the camera is culled when single-sided and drawn
    // when double-sided; mirrored (negative determinant) it keeps its front face, and the
    // texture shows mirrored.
    let turned = Mat4::from_rotation_y(core::f32::consts::PI);
    let two_sided = scene.materials.add(MaterialDesc {
        double_sided: true,
        ..emissive(Some(TextureRef::new(picture)), [1.0; 3])
    });
    let image = scene.render(&[draw(quad_mesh, shown, turned)], camera, DARK);
    assert_eq!(
        block_pixel(&image, 1, 1),
        [0, 0, 0, 255],
        "back face culled"
    );
    let image = scene.render(&[draw(quad_mesh, two_sided, turned)], camera, DARK);
    assert!(
        near(block_pixel(&image, 0, 0), block_color(3, 0), 1),
        "back face drawn, seen from behind"
    );
    let mirrored = Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0));
    let image = scene.render(&[draw(quad_mesh, shown, mirrored)], camera, DARK);
    for bx in 0..4 {
        assert!(
            near(block_pixel(&image, bx, 2), block_color(3 - bx, 2), 1),
            "mirrored block {bx}: {:?}",
            block_pixel(&image, bx, 2)
        );
    }

    // 6. Normal maps: a map tilting every normal by 45° towards +u (red), -u, or up the
    // image (green). Lit from 60° off the normal, towards +X or +Y, the surface brightens
    // only when the tilt leans towards the light. Mirrored, +u points along -X.
    let tilt = |x: f32, y: f32| {
        let z = (1.0 - x * x - y * y).sqrt();
        let byte = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() as u8;
        scene_texture_color([byte(x), byte(y), byte(z)])
    };
    let s = core::f32::consts::FRAC_1_SQRT_2;
    let maps = [
        ("+u", tilt(s, 0.0)),
        ("-u", tilt(-s, 0.0)),
        ("+v up", tilt(0.0, s)),
    ];
    let white = |normal: Option<TextureId>| MaterialDesc {
        normal_texture: normal.map(TextureRef::new),
        ..MaterialDesc::solid([1.0; 3], 0.0, 1.0)
    };
    let (sin, cos) = (60f32.to_radians().sin(), 60f32.to_radians().cos());
    let from = |to_light: Vec3| DirectionalLight {
        direction: -to_light,
        color: Vec3::splat(3.0),
        ambient: Vec3::ZERO,
    };
    let flat_material = scene.materials.add(white(None));
    let shade = |scene: &mut Scene, material, transform, light| {
        let image = scene.render(&[draw(quad_mesh, material, transform)], camera, light);
        luma(pixel(&image, SIZE / 2, SIZE / 2))
    };
    let from_x = from(Vec3::new(sin, 0.0, cos));
    let from_y = from(Vec3::new(0.0, sin, cos));
    let from_neg_x = from(Vec3::new(-sin, 0.0, cos));
    let flat_x = shade(&mut scene, flat_material, Mat4::IDENTITY, from_x);
    let flat_y = shade(&mut scene, flat_material, Mat4::IDENTITY, from_y);
    let mut lit = Vec::new();
    for (name, levels) in maps {
        let texture = scene.texture(levels);
        let material = scene.materials.add(white(Some(texture)));
        lit.push((
            name,
            shade(&mut scene, material, Mat4::IDENTITY, from_x),
            shade(&mut scene, material, Mat4::IDENTITY, from_y),
            shade(&mut scene, material, mirrored, from_neg_x),
        ));
    }
    println!(
        "flat: {flat_x} (light +X), {flat_y} (light +Y); tilted (+X, +Y, mirrored -X): {lit:?}"
    );
    let [(_, px, py, pm), (_, nx, _, nm), (_, ux, uy, _)] = [lit[0], lit[1], lit[2]];
    assert!(px > flat_x + 60 && nx + 60 < flat_x, "red tilts along +X");
    assert!(uy > flat_y + 60 && ux < px, "green tilts up the image (+Y)");
    assert!(pm > nm + 60, "mirrored: +u along -X");
    assert!(py < uy, "red does not tilt along +Y");

    // 7. Streaming: with a budget below one texture, textures upload one per frame. Until
    // its texture is uploaded a material shows its factor alone.
    scene.finish();
    let Some((instance, device)) = gpu() else {
        return;
    };
    let mut scene = Scene::new(instance, device, 1);
    let quad_mesh = scene.mesh(&quad(1.0));
    let first = scene.texture(blocks(true, |_, _| [255, 0, 0, 255]));
    let second = scene.texture(blocks(true, |_, _| [0, 0, 255, 255]));
    let late = scene
        .materials
        .add(emissive(Some(TextureRef::new(second)), [1.0, 1.0, 0.0]));
    let generation = scene.textures.generation();
    let mut seen = Vec::new();
    for _ in 0..3 {
        let image = scene.render(&[draw(quad_mesh, late, Mat4::IDENTITY)], camera, DARK);
        seen.push((
            block_pixel(&image, 1, 1),
            scene.textures.index(first).is_some(),
            scene.textures.index(second).is_some(),
        ));
    }
    println!("streaming (pixel, first ready, second ready): {seen:?}");
    assert_eq!(
        seen,
        [
            ([255, 255, 0, 255], true, false),
            ([0, 0, 0, 255], true, true),
            ([0, 0, 0, 255], true, true),
        ],
        "the yellow factor alone, then blue × yellow = black"
    );
    assert!(scene.textures.generation() > generation);
    // A removed texture: its materials shade without it from the next frame on.
    let frame = scene.frame_number;
    scene.textures.remove(second, frame, &scene.bindless);
    assert!(scene.textures.index(second).is_none());
    let image = scene.render(&[draw(quad_mesh, late, Mat4::IDENTITY)], camera, DARK);
    assert_eq!(
        block_pixel(&image, 1, 1),
        [255, 255, 0, 255],
        "texture removed"
    );

    // 8. Edits apply next frame; removed materials draw as the default material.
    let reference = scene.render(
        &[draw(
            quad_mesh,
            scene.materials.default_material(),
            Mat4::IDENTITY,
        )],
        camera,
        from_x,
    );
    let edited = scene.materials.add(emissive(None, [0.0, 1.0, 0.0]));
    let image = scene.render(&[draw(quad_mesh, edited, Mat4::IDENTITY)], camera, DARK);
    assert_eq!(block_pixel(&image, 1, 1), [0, 255, 0, 255]);
    scene.materials.set(edited, emissive(None, [0.0, 0.0, 1.0]));
    let image = scene.render(&[draw(quad_mesh, edited, Mat4::IDENTITY)], camera, DARK);
    assert_eq!(block_pixel(&image, 1, 1), [0, 0, 255, 255], "edited");
    scene.materials.remove(edited);
    let image = scene.render(&[draw(quad_mesh, edited, Mat4::IDENTITY)], camera, from_x);
    assert_eq!(
        block_pixel(&image, 1, 1),
        block_pixel(&reference, 1, 1),
        "stale id: default material"
    );

    // 9. Block-compressed textures (ADR 0041): BC7 color within a few steps of its
    // texels, and a BC5 normal map shading like its uncompressed original.
    let bc7 = scene.texture(compressed(
        &blocks(true, block_color),
        engine_assets::TextureUsage::Color,
    ));
    let shown_bc7 = scene
        .materials
        .add(emissive(Some(TextureRef::new(bc7)), [1.0; 3]));
    let image = scene.render(&[draw(quad_mesh, shown_bc7, Mat4::IDENTITY)], camera, DARK);
    for by in 0..4 {
        for bx in 0..4 {
            let got = block_pixel(&image, bx, by);
            assert!(
                near(got, block_color(bx, by), 4),
                "BC7 block ({bx}, {by}): {got:?}, texel {:?}",
                block_color(bx, by)
            );
        }
    }
    let s = core::f32::consts::FRAC_1_SQRT_2;
    let tilted = |z: f32| {
        let byte = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() as u8;
        scene_texture_color([byte(s), byte(0.0), byte(z)])
    };
    let plain = scene.texture(tilted(s));
    let bc5 = scene.texture(compressed(&tilted(s), engine_assets::TextureUsage::Normal));
    let mut lit = Vec::new();
    for texture in [plain, bc5] {
        let material = scene.materials.add(MaterialDesc {
            normal_texture: Some(TextureRef::new(texture)),
            ..MaterialDesc::solid([1.0; 3], 0.0, 1.0)
        });
        let image = scene.render(&[draw(quad_mesh, material, Mat4::IDENTITY)], camera, from_x);
        lit.push(luma(pixel(&image, SIZE / 2, SIZE / 2)));
    }
    println!("normal map lit from +X, RGBA8 vs BC5: {lit:?}");
    assert!(lit[0].abs_diff(lit[1]) <= 6, "BC5 normals: {lit:?}");

    // 10. The depth prepass (ADR 0042) changes no pixel: masked, double-sided and
    // mirrored geometry in sunlight, shaded with and without it.
    let mask = scene.texture(blocks(true, |bx, by| {
        [200, 60, 30, if (bx + by) % 2 == 0 { 0 } else { 255 }]
    }));
    let leaves = scene.materials.add(MaterialDesc {
        base_color_texture: Some(TextureRef::new(mask)),
        alpha: AlphaMode::Mask { cutoff: 0.5 },
        double_sided: true,
        ..MaterialDesc::solid([1.0; 3], 0.0, 0.7)
    });
    let stone = scene
        .materials
        .add(MaterialDesc::solid([0.7, 0.7, 0.65], 0.0, 0.8));
    let standing = Mat4::from_scale_rotation_translation(
        Vec3::new(-1.2, 1.2, 1.0),
        Quat::from_rotation_y(0.6),
        Vec3::new(0.8, 0.6, 0.5),
    );
    let draws = [
        draw(quad_mesh, stone, ground),
        draw(quad_mesh, leaves, roof),
        draw(quad_mesh, stone, standing),
    ];
    let view = Camera {
        clip_from_world,
        position: eye,
    };
    let mut images = Vec::new();
    for prepass in [true, false] {
        scene.renderer.set_depth_prepass(prepass);
        images.push(scene.render(&draws, view, sun));
    }
    scene.renderer.set_depth_prepass(true);
    let lit = images[0].chunks_exact(4).filter(|p| p[0] > 40).count();
    assert!(lit > 1000, "the scene covers the view ({lit} lit pixels)");
    let differing = images[0]
        .chunks_exact(4)
        .zip(images[1].chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differing, 0, "the depth prepass changed {differing} pixels");
    scene.finish();
}

/// A uniform linear 8 × 8 texture (normal maps are data, not sRGB).
fn scene_texture_color(rgb: [u8; 3]) -> TextureLevels {
    blocks(false, |_, _| [rgb[0], rgb[1], rgb[2], 255])
}
