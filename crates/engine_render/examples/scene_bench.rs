//! Real content through the forward renderer, headless (ADR 0039, ADR 0040): a glTF scene
//! with its materials and textures, drawn into a 1920×1080 HDR target with a shadow map
//! from a fixed camera.
//!
//! Reports the import (glTF, then every texture decoded, given its mip chain and
//! block-compressed in parallel, or loaded from the derived data cache, then the meshes
//! into the geometry pool), the memory of geometry and
//! textures, and the median GPU time of the shadow and forward passes, with the materials
//! as imported and with their factors only (no textures). `--screenshot out.png` also
//! writes the textured frame, tonemapped.
//!
//! `cargo run -p engine_render --release --example scene_bench -- test-assets/Sponza/Sponza.gltf`
//! (scenes: `tools/fetch-test-assets.ps1`; options: `--screenshot <png>`,
//! `--view <eye x,y,z,target x,y,z>`, `--uncompressed` for RGBA8 textures, `--ddc <dir>`
//! for the derived data cache, `test-assets/.ddc` by default)

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_assets::{
    AlphaMode as GltfAlpha, CacheOutcome, DerivedDataCache, ImportedScene, TextureFormat as Stored,
    TextureUsage, TextureWrap, build_mips, decode_image, import_gltf, import_texture, read_texture,
};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Quat, Vec3, look_at, vulkan_perspective};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialId, MaterialTable, TextureRef};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData, MeshId};
use engine_render::png::{PngFormat, encode_png};
use engine_render::post::{HDR_FORMAT, Tonemap};
use engine_render::texture_pool::{TextureFormat, TextureId, TextureLevels, TexturePool};
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation, Wrap,
};
use std::sync::OnceLock;
use std::time::Instant;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WARMUP: usize = 30;
const FRAMES: usize = 200;
/// Texture bytes uploaded per frame: the whole scene within a few frames.
const UPLOAD_BUDGET: u64 = 256 << 20;

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// World transforms of every node that has a mesh: `(mesh index, world from object)`.
fn instances(scene: &ImportedScene) -> Vec<(usize, Mat4)> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, Mat4)> = scene.roots.iter().map(|&r| (r, Mat4::IDENTITY)).collect();
    while let Some((index, parent)) = stack.pop() {
        let node = &scene.nodes[index];
        let local = Mat4::from_scale_rotation_translation(
            Vec3::from(node.scale),
            Quat::from_array(node.rotation),
            Vec3::from(node.translation),
        );
        let world = parent * local;
        if let Some(mesh) = node.mesh {
            out.push((mesh, world));
        }
        stack.extend(node.children.iter().map(|&c| (c, world)));
    }
    out
}

/// Every distinct use of a texture by the materials: `(texture, usage)`. The usage decides
/// how its mip levels are filtered.
fn texture_uses(scene: &ImportedScene) -> Vec<(usize, TextureUsage)> {
    let mut uses = Vec::new();
    let mut add = |texture: Option<usize>, usage| {
        if let Some(texture) = texture
            && !uses.contains(&(texture, usage))
        {
            uses.push((texture, usage));
        }
    };
    for m in &scene.materials {
        add(m.base_color_texture, color_usage(m.alpha));
        add(m.emissive_texture, TextureUsage::Color);
        add(m.metallic_roughness_texture, TextureUsage::Data);
        add(m.occlusion_texture, TextureUsage::Data);
        add(m.normal_texture, TextureUsage::Normal);
    }
    uses
}

fn color_usage(alpha: GltfAlpha) -> TextureUsage {
    match alpha {
        GltfAlpha::Mask(bits) => TextureUsage::ColorMasked {
            cutoff: f32::from_bits(bits),
        },
        GltfAlpha::Opaque | GltfAlpha::Blend => TextureUsage::Color,
    }
}

fn wrap(w: TextureWrap) -> Wrap {
    match w {
        TextureWrap::Repeat => Wrap::Repeat,
        TextureWrap::MirroredRepeat => Wrap::MirroredRepeat,
        TextureWrap::ClampToEdge => Wrap::ClampToEdge,
    }
}

/// The scene's materials; `textured: false` keeps the factors only.
fn material_descs(
    scene: &ImportedScene,
    uses: &[(usize, TextureUsage)],
    ids: &[Option<TextureId>],
    textured: bool,
) -> Vec<MaterialDesc> {
    let find = |texture: Option<usize>, usage: TextureUsage| -> Option<TextureRef> {
        let texture = texture.filter(|_| textured)?;
        let k = uses.iter().position(|&u| u == (texture, usage))?;
        Some(TextureRef {
            texture: ids[k]?,
            wrap: scene.textures[texture].wrap.map(wrap),
        })
    };
    scene
        .materials
        .iter()
        .map(|m| MaterialDesc {
            base_color: m.base_color,
            base_color_texture: find(m.base_color_texture, color_usage(m.alpha)),
            metallic: m.metallic,
            roughness: m.roughness,
            metallic_roughness_texture: find(m.metallic_roughness_texture, TextureUsage::Data),
            normal_texture: find(m.normal_texture, TextureUsage::Normal),
            normal_scale: m.normal_scale,
            occlusion_texture: find(m.occlusion_texture, TextureUsage::Data),
            occlusion_strength: m.occlusion_strength,
            emissive: m.emissive,
            emissive_texture: find(m.emissive_texture, TextureUsage::Color),
            // Blending comes with the transparency pass; until then blended materials
            // draw opaque.
            alpha: match m.alpha {
                GltfAlpha::Mask(bits) => AlphaMode::Mask {
                    cutoff: f32::from_bits(bits),
                },
                GltfAlpha::Opaque | GltfAlpha::Blend => AlphaMode::Opaque,
            },
            double_sided: m.double_sided,
        })
        .collect()
}

/// A camera from `--view ex,ey,ez,tx,ty,tz`, or an interior view down the longest
/// horizontal axis from a quarter of its length, a little above the floor, or an object
/// view from the front.
fn view(min: Vec3, max: Vec3, requested: Option<[f32; 6]>) -> (Vec3, Vec3) {
    if let Some(v) = requested {
        return (Vec3::new(v[0], v[1], v[2]), Vec3::new(v[3], v[4], v[5]));
    }
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

fn main() {
    let mut path = String::from("test-assets/Sponza/Sponza.gltf");
    let (mut screenshot, mut requested_view) = (None, None);
    let mut uncompressed = false;
    let mut ddc = String::from("test-assets/.ddc");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--screenshot" => screenshot = args.next(),
            "--uncompressed" => uncompressed = true,
            "--ddc" => ddc = args.next().unwrap(),
            "--view" => {
                let v: Vec<f32> = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .map(|x| x.trim().parse().unwrap())
                    .collect();
                requested_view = Some(<[f32; 6]>::try_from(v).unwrap());
            }
            _ => path = arg,
        }
    }
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!("{path}: {e} (download test scenes with tools/fetch-test-assets.ps1)")
    });
    let dir = std::path::Path::new(&path).parent();
    let start = Instant::now();
    let scene = import_gltf(&bytes, dir).unwrap();
    let imported = start.elapsed();
    let placed = instances(&scene);

    // Textures: read, decode, filter and compress every use in parallel (or load them
    // from the derived data cache).
    let jobs = ThreadPool::builder().build().unwrap();
    let cache = DerivedDataCache::new(&ddc).unwrap();
    let uses = texture_uses(&scene);
    let start = Instant::now();
    let images: Vec<OnceLock<(TextureLevels, bool)>> =
        uses.iter().map(|_| OnceLock::new()).collect();
    jobs.parallel_for(uses.len(), 1, |_, range| {
        for k in range {
            let (texture, usage) = uses[k];
            let encoded = read_texture(&scene.textures[texture].source, dir).unwrap();
            let levels = if uncompressed {
                let mips = build_mips(&decode_image(&encoded).unwrap(), usage);
                let format = if mips.srgb {
                    TextureFormat::Rgba8Srgb
                } else {
                    TextureFormat::Rgba8Unorm
                };
                let levels = TextureLevels {
                    width: mips.width,
                    height: mips.height,
                    format,
                    levels: mips.levels,
                };
                (levels, false)
            } else {
                let (t, outcome) =
                    import_texture(&encoded, usage, Some(&cache), Some(&jobs)).unwrap();
                let format = match t.format {
                    Stored::Rgba8Srgb => TextureFormat::Rgba8Srgb,
                    Stored::Rgba8Unorm => TextureFormat::Rgba8Unorm,
                    Stored::Bc7Srgb => TextureFormat::Bc7Srgb,
                    Stored::Bc7Unorm => TextureFormat::Bc7Unorm,
                    Stored::Bc5Unorm => TextureFormat::Bc5Unorm,
                };
                let levels = TextureLevels {
                    width: t.width,
                    height: t.height,
                    format,
                    levels: t.levels,
                };
                (levels, outcome == CacheOutcome::Hit)
            };
            let _ = images[k].set(levels);
        }
    });
    let images: Vec<(TextureLevels, bool)> = images
        .into_iter()
        .map(|i| i.into_inner().unwrap())
        .collect();
    let decoded = start.elapsed();
    let hits = images.iter().filter(|(_, hit)| *hit).count();
    let texels: u64 = images
        .iter()
        .map(|(i, _)| u64::from(i.width) * u64::from(i.height))
        .sum();

    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("scene_bench"),
        validation: false,
        debug_utils: true,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .unwrap();
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    let device = Device::new(&instance, &adapter).unwrap();
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let depth_format = vk::Format::D32_SFLOAT;
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        HDR_FORMAT,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let screenshot_format = vk::Format::R8G8B8A8_SRGB;
    let tonemap = Tonemap::new(&device, &bindless, screenshot_format).unwrap();
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 64 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let readback_size = u64::from(WIDTH * HEIGHT * 4);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(readback_size)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
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

    // Meshes, one per primitive.
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let start = Instant::now();
    let meshes: Vec<Vec<MeshId>> = scene
        .meshes
        .iter()
        .map(|mesh| {
            mesh.primitives
                .iter()
                .map(|p| {
                    geometry
                        .add_mesh(&MeshData {
                            positions: &p.positions,
                            normals: &p.normals,
                            tangents: &p.tangents,
                            uvs: &p.uvs,
                            indices: &p.indices,
                        })
                        .unwrap()
                })
                .collect()
        })
        .collect();
    let meshes_added = start.elapsed();
    let (vertices, _, indices, _) = geometry.usage();

    // Textures and materials.
    let mut textures = TexturePool::new(&device, UPLOAD_BUDGET, 1);
    let texture_ids: Vec<Option<TextureId>> = images
        .into_iter()
        .map(|(levels, _)| textures.add(&device, &mut allocator, levels).ok())
        .collect();
    let (_, texture_bytes) = textures.pending();
    let mut materials = MaterialTable::new(1);
    let textured = material_descs(&scene, &uses, &texture_ids, true);
    let untextured = material_descs(&scene, &uses, &texture_ids, false);
    let material_ids: Vec<MaterialId> = textured.iter().map(|&m| materials.add(m)).collect();
    let fallback = materials.default_material();

    // Draws sorted by mesh, so instances of a primitive share a batch.
    let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let mut draws: Vec<DrawItem> = Vec::new();
    for &(mesh, world) in &placed {
        for (p, primitive) in scene.meshes[mesh].primitives.iter().enumerate() {
            draws.push(DrawItem {
                mesh: meshes[mesh][p],
                material: primitive.material.map_or(fallback, |i| material_ids[i]),
                world_from_object: world,
            });
            for corner in 0..8 {
                let pick = |bit: usize, axis: usize| {
                    if corner & bit != 0 {
                        primitive.max[axis]
                    } else {
                        primitive.min[axis]
                    }
                };
                let c = world.transform_point3(Vec3::new(pick(1, 0), pick(2, 1), pick(4, 2)));
                min = min.min(c);
                max = max.max(c);
            }
        }
    }
    draws.sort_by_key(|d| format!("{:?}", d.mesh));
    let (eye, target) = view(min, max, requested_view);
    let size = max - min;
    let camera = Camera {
        clip_from_world: vulkan_perspective(
            60f32.to_radians(),
            WIDTH as f32 / HEIGHT as f32,
            size.length() * 0.001,
            size.length() * 2.0,
        ) * look_at(eye, target, Vec3::Y),
        position: eye,
    };
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.2),
        color: Vec3::splat(3.0),
        ambient: Vec3::splat(0.08),
    };

    let masked = scene
        .materials
        .iter()
        .filter(|m| matches!(m.alpha, GltfAlpha::Mask(_)))
        .count();
    let blended = scene
        .materials
        .iter()
        .filter(|m| m.alpha == GltfAlpha::Blend)
        .count();
    let double_sided = scene.materials.iter().filter(|m| m.double_sided).count();
    println!("### Scene through the forward renderer ({WIDTH}×{HEIGHT} HDR, shadow map 2048²)");
    println!();
    println!("Device: {}", device.adapter().summary());
    println!(
        "Scene: {path}: {} triangles in {} primitives, {} draws; {} materials ({masked} masked, \
         {blended} blended drawn opaque, {double_sided} double-sided)",
        scene.triangle_count(),
        scene
            .meshes
            .iter()
            .map(|m| m.primitives.len())
            .sum::<usize>(),
        draws.len(),
        scene.materials.len(),
    );
    println!(
        "Import: glTF {:.0} ms; {} textures ({:.1} Mtexels) {} in {:.0} ms on {} threads \
         ({hits} from the cache); meshes into the pool {:.0} ms",
        imported.as_secs_f64() * 1e3,
        uses.len(),
        texels as f64 / 1e6,
        if uncompressed {
            "read, decoded and mipmapped"
        } else {
            "read, decoded, mipmapped and compressed"
        },
        decoded.as_secs_f64() * 1e3,
        jobs.num_workers() + 1,
        meshes_added.as_secs_f64() * 1e3,
    );
    println!(
        "Memory: geometry {:.1} MB ({vertices} vertices), textures {:.1} MB ({} with mips)",
        (vertices * 28 + indices * 4) as f64 / (1 << 20) as f64,
        texture_bytes as f64 / (1 << 20) as f64,
        if uncompressed { "RGBA8" } else { "BC7 and BC5" },
    );
    println!();
    println!("| materials | upload frames | GPU shadow µs | GPU forward µs |");
    println!("|---|---:|---:|---:|");

    let mut frame_number = 0u64;
    for (mode, descs) in [("textured", &textured), ("factors only", &untextured)] {
        for (&id, &desc) in material_ids.iter().zip(descs.iter()) {
            materials.set(id, desc);
        }
        let (mut shadow, mut forward) = (Vec::new(), Vec::new());
        let mut upload_frames = 0;
        let last = mode == "textured" && screenshot.is_some();
        let mut frame = 0;
        while frame < WARMUP + FRAMES + usize::from(last) {
            let uploading = textures.pending().0 > 0;
            upload_frames += usize::from(uploading);
            let capture = last && frame == WARMUP + FRAMES;
            // SAFETY: the previous frame finished (waited below).
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
            {
                // SAFETY: nothing is in flight.
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                frame_number += 1;
                textures
                    .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                    .unwrap();
                let mats = materials
                    .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                    .unwrap();
                let geo = geometry
                    .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                    .unwrap();
                let hdr = fg.create_texture("hdr", TextureDesc::new_2d(HDR_FORMAT, WIDTH, HEIGHT));
                let depth =
                    fg.create_texture("depth", TextureDesc::new_2d(depth_format, WIDTH, HEIGHT));
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
                            light,
                            color: hdr,
                            depth,
                            clear: Some([0.02, 0.02, 0.05, 1.0]),
                            occlusion: None,
                        },
                    )
                    .unwrap();
                if capture {
                    let ldr = fg.create_texture(
                        "screenshot",
                        TextureDesc::new_2d(screenshot_format, WIDTH, HEIGHT),
                    );
                    tonemap.add_pass(&mut fg, &bindless, hdr, ldr, 1.0);
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
                                    ctx.image(ldr),
                                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                    ctx.buffer(out),
                                    &[region],
                                );
                            }
                        });
                } else {
                    fg.add_pass("consume")
                        .read(hdr, Access::TransferRead)
                        .side_effects()
                        .execute(|_| {});
                }
                fg.execute_with_bindless(&mut allocator, Some(&bindless))
                    .unwrap();
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
            // Measure once every texture is resident.
            if uploading {
                continue;
            }
            if frame > WARMUP && !capture {
                for time in graph.gpu_pass_times() {
                    match time.name {
                        "shadow" => shadow.push(f64::from(time.gpu_us)),
                        "forward" => forward.push(f64::from(time.gpu_us)),
                        _ => {}
                    }
                }
            }
            if capture && let Some(file) = &screenshot {
                let ptr = readback_mem.mapped_ptr().unwrap();
                // SAFETY: the GPU is idle; the mapping covers the image.
                let pixels =
                    unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
                let png = encode_png(WIDTH, HEIGHT, PngFormat::Rgba, pixels).unwrap();
                std::fs::write(file, png).unwrap();
            }
            frame += 1;
        }
        println!(
            "| {mode} | {upload_frames} | {:.0} | {:.0} |",
            median(&mut shadow),
            median(&mut forward),
        );
    }
    println!();
    if let Some(file) = &screenshot {
        println!("Screenshot: {file} (eye {eye}, target {target})");
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(command_pool, None);
    }
}
