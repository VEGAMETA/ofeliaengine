//! Skinning (ADR 0068), under the validation layer with synchronization validation:
//! skinned positions and normals against a CPU reference (4 and 8 influences), last
//! frame's positions kept for motion vectors, and the motion vectors of a bending skinned
//! bar against the exact motion of its surface, through the forward pass and the skinned
//! motion pass, with the depth prepass on and off.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::too_many_lines)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec2, Vec3, look_at, vulkan_orthographic};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::MaterialTable;
use engine_render::mesh::{
    GeometryCapacity, GeometryPool, MeshData, OwnedMeshData, decode_octahedral,
};
use engine_render::skinning::{SkinnedMeshData, Skinning};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const SIZE: u32 = 64;
/// The orthographic view spans 8 world units: 8 pixels per unit.
const SPAN: f32 = 8.0;
/// The bar bends about this point (its second joint).
const PIVOT: Vec3 = Vec3::new(0.0, 2.0, 0.0);

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render skinning test"),
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

/// A square bar from y = 0 to 4, 1 wide, with a ring of vertices every 0.25: the lower
/// part follows joint 0, the upper part joint 1, blended between y = 1.5 and 2.5. With
/// `eight`, every vertex's weights are spread over two sets of four.
fn bar(eight: bool) -> (OwnedMeshData, Vec<[u16; 4]>, Vec<[f32; 4]>) {
    let mut mesh = OwnedMeshData::default();
    let mut joints = Vec::new();
    let mut weights = Vec::new();
    let sides: [(Vec3, Vec3); 4] = [
        (Vec3::Z, Vec3::X),
        (Vec3::X, Vec3::NEG_Z),
        (Vec3::NEG_Z, Vec3::NEG_X),
        (Vec3::NEG_X, Vec3::Z),
    ];
    let rings = 16;
    for (normal, across) in sides {
        let base = mesh.positions.len() as u32;
        for r in 0..=rings {
            let y = 4.0 * r as f32 / rings as f32;
            for s in [-0.5f32, 0.5] {
                let p = normal * 0.5 + across * s + Vec3::Y * y;
                mesh.positions.push(p.to_array());
                mesh.normals.push(normal.to_array());
                mesh.tangents.push(across.extend(1.0).to_array());
                mesh.uvs.push([s + 0.5, y / 4.0]);
                let t = ((y - 1.5) / 1.0).clamp(0.0, 1.0);
                if eight {
                    // The same weights, in two sets with repeated joints.
                    joints.push([0, 1, 0, 1]);
                    weights.push([(1.0 - t) * 0.5, t * 0.5, 0.0, 0.0]);
                    joints.push([1, 0, 0, 0]);
                    weights.push([t * 0.5, (1.0 - t) * 0.5, 0.0, 0.0]);
                } else {
                    joints.push([0, 1, 0, 0]);
                    weights.push([1.0 - t, t, 0.0, 0.0]);
                }
            }
        }
        for r in 0..rings {
            let a = base + 2 * r;
            // Counter-clockwise seen from outside (normal × across = ... up the bar).
            mesh.indices.extend([a, a + 1, a + 3, a, a + 3, a + 2]);
        }
    }
    (mesh, joints, weights)
}

/// Skinning matrices of the bar bent by `angle` about +Z at the pivot: joint 0 still,
/// joint 1 rotated (model · inverse bind = T(pivot) R T(−pivot)).
fn palette(angle: f32) -> [[f32; 12]; 2] {
    let bend = Mat4::from_translation(PIVOT)
        * Mat4::from_quat(Quat::from_rotation_z(angle))
        * Mat4::from_translation(-PIVOT);
    let rows = |m: Mat4| {
        let r = |i: usize| m.row(i).to_array();
        let (a, b, c) = (r(0), r(1), r(2));
        [
            a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], c[0], c[1], c[2], c[3],
        ]
    };
    [rows(Mat4::IDENTITY), rows(bend)]
}

/// Linear blend skinning on the CPU, with the GPU's 16-bit weights.
fn reference(
    mesh: &OwnedMeshData,
    joints: &[[u16; 4]],
    weights: &[[f32; 4]],
    palette: &[[f32; 12]],
) -> Vec<(Vec3, Vec3)> {
    let sets = joints.len() / mesh.positions.len();
    (0..mesh.positions.len())
        .map(|v| {
            let total: f32 = weights[v * sets..][..sets].iter().flatten().sum();
            let mut m = [0.0f32; 12];
            let mut sum = 0.0;
            for k in 0..sets {
                for i in 0..4 {
                    let w = ((weights[v * sets + k][i] / total) * 65535.0).round() / 65535.0;
                    let j = joints[v * sets + k][i] as usize;
                    for e in 0..12 {
                        m[e] += palette[j][e] * w;
                    }
                    sum += w;
                }
            }
            let m = m.map(|e| e / sum);
            let apply = |p: Vec3, t: f32| {
                Vec3::new(
                    m[0] * p.x + m[1] * p.y + m[2] * p.z + m[3] * t,
                    m[4] * p.x + m[5] * p.y + m[6] * p.z + m[7] * t,
                    m[8] * p.x + m[9] * p.y + m[10] * p.z + m[11] * t,
                )
            };
            (
                apply(Vec3::from(mesh.positions[v]), 1.0),
                apply(Vec3::from(mesh.normals[v]), 0.0).normalize(),
            )
        })
        .collect()
}

fn camera() -> Camera {
    // Looking down −Z at (0, 2): the bar fills the middle of the view.
    let eye = Vec3::new(0.0, 2.0, 10.0);
    Camera {
        clip_from_world: vulkan_orthographic(
            -SPAN / 2.0,
            SPAN / 2.0,
            -SPAN / 2.0,
            SPAN / 2.0,
            0.1,
            50.0,
        ) * look_at(eye, Vec3::new(0.0, 2.0, 0.0), Vec3::Y),
        position: eye,
    }
}

/// Pixel coordinates (x right, y down) of a world point.
fn to_pixels(p: Vec3) -> Vec2 {
    let clip = camera().clip_from_world * p.extend(1.0);
    let ndc = Vec2::new(clip.x, clip.y) / clip.w;
    (ndc * 0.5 + 0.5) * SIZE as f32
}

/// The world point (on the plane z = 0.5, the bar's front face) at pixel coordinates.
fn from_pixels(px: Vec2) -> Vec3 {
    let ndc = px / SIZE as f32 * 2.0 - 1.0;
    let inverse = camera().clip_from_world.inverse();
    let p = inverse * ndc.extend(0.0).extend(1.0);
    let p = p.truncate() / p.w;
    Vec3::new(p.x, p.y, 0.5)
}

struct Frame {
    positions: Vec<Vec3>,
    normals: Vec<Vec3>,
    previous: Vec<Vec3>,
    motion: Vec<[f32; 2]>,
    jitter: [f32; 2],
}

#[test]
fn skinning() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let depth_format = vk::Format::D32_SFLOAT;
    let mut renderer = ForwardRenderer::new(
        &device,
        &bindless,
        vk::Format::R16G16B16A16_SFLOAT,
        depth_format,
        ShadowSettings {
            resolution: 512,
            ..ShadowSettings::default()
        },
    )
    .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mut skinning =
        Skinning::new(&device, &mut allocator, &bindless, depth_format, 64, 1).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let temporal = Temporal::new(&device, &mut allocator, 16).unwrap();

    let mut skins = Vec::new();
    let mut meshes = Vec::new();
    for eight in [false, true] {
        let (mesh, joints, weights) = bar(eight);
        let skin = skinning
            .add_skin(&SkinnedMeshData {
                mesh: MeshData {
                    positions: &mesh.positions,
                    normals: &mesh.normals,
                    tangents: &mesh.tangents,
                    uvs: &mesh.uvs,
                    indices: &mesh.indices,
                },
                joints: &joints,
                weights: &weights,
                joint_count: 2,
            })
            .unwrap();
        let (id, out) = skinning.add_instance(skin, &mut geometry).unwrap();
        skins.push((mesh, joints, weights));
        meshes.push((id, out));
    }
    let vertex_count = skins[0].0.positions.len() as u64;
    let motion_bytes = u64::from(SIZE * SIZE * 4);
    let geometry_bytes = vertex_count * 12;
    let readback_size = motion_bytes + 2 * (2 * geometry_bytes + vertex_count * 16);
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(readback_size)
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
    let light = DirectionalLight {
        direction: Vec3::new(-0.3, -1.0, -0.4),
        color: Vec3::splat(2.0),
        ambient: Vec3::splat(0.1),
    };
    let mut frame_number = 0u64;
    // Only the four-influence bar is drawn (the eight-influence one is skinned, read back,
    // and drawn nowhere).
    let world = Mat4::IDENTITY;

    let default_material = materials.default_material();
    // The same bar in a persistent scene (ADR 0064).
    let mut scene = GpuScene::new(&device, &bindless, 16, 1).unwrap();
    let scene_id = scene.insert(SceneInstance {
        mesh: meshes[0].1,
        material: default_material,
        world_from_object: world,
    });
    skinning.set_scene_instance(meshes[0].0, Some(scene_id));
    let mut render = |angle: f32,
                      renderer: &ForwardRenderer,
                      skinning: &mut Skinning,
                      scene: Option<&mut GpuScene>|
     -> [Frame; 2] {
        for &(id, _) in &meshes {
            skinning.set_pose(id, &palette(angle), &world);
        }
        let mut scene = scene;
        skinning.update_bounds(&mut geometry, scene.as_deref_mut());
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
        frame_number += 1;
        let jitter;
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                .unwrap();
            let skins_frame = skinning
                .begin_frame(
                    &mut fg,
                    &mut ring,
                    &device,
                    &mut allocator,
                    &bindless,
                    &geo,
                    frame_number,
                )
                .unwrap();
            assert_eq!(skins_frame.instances(), 2);
            let color = fg.create_texture(
                "color",
                TextureDesc::new_2d(vk::Format::R16G16B16A16_SFLOAT, SIZE, SIZE),
            );
            let depth = fg.create_texture("depth", TextureDesc::new_2d(depth_format, SIZE, SIZE));
            let draws = [DrawItem {
                mesh: meshes[0].1,
                material: default_material,
                world_from_object: world,
            }];
            let view = ForwardView {
                camera: camera(),
                light,
                color,
                depth,
                clear: Some([0.0; 4]),
                occlusion: None,
                temporal: Some(&temporal),
                lights: &[],
                decals: &[],
                environment: None,
                reflections: Default::default(),
            };
            let passes = match scene {
                None => renderer
                    .add_passes(&mut fg, &mut ring, &bindless, &geo, &mats, &draws, &view)
                    .unwrap(),
                Some(scene) => {
                    let frame = scene
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
                        .unwrap()
                        .unwrap();
                    renderer
                        .add_scene_passes(&mut fg, &mut ring, &bindless, &geo, &mats, &frame, &view)
                        .unwrap()
                }
            };
            let motion = passes.motion.unwrap();
            jitter = motion.jitter;
            skins_frame
                .add_motion_pass(&mut fg, &mut ring, &bindless, &geo, depth, &motion)
                .unwrap();
            let velocity = motion.velocity;
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
            let firsts: Vec<(u64, u64, u64)> = meshes
                .iter()
                .map(|&(id, mesh)| {
                    let info = geo.mesh(mesh).unwrap();
                    let (offset, _) = skins_frame.previous_range(id).unwrap();
                    (u64::from(info.first_vertex), offset, 0)
                })
                .collect();
            let (positions, attributes, previous) =
                (geo.positions, geo.attributes, skins_frame.previous_buffer());
            fg.add_pass("read back")
                .read(velocity, Access::TransferRead)
                .read(positions, Access::TransferRead)
                .read(attributes, Access::TransferRead)
                .read(previous, Access::TransferRead)
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
                    // SAFETY: recording; layouts arranged by the graph; every region lies
                    // in its buffer.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(velocity),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[region],
                        );
                        let mut at = motion_bytes;
                        for &(first, prev, _) in &firsts {
                            for (source, offset, size) in [
                                (positions, first * 12, geometry_bytes),
                                (attributes, first * 16, vertex_count * 16),
                                (previous, prev, geometry_bytes),
                            ] {
                                ctx.device.cmd_copy_buffer(
                                    ctx.cmd,
                                    ctx.buffer(source),
                                    ctx.buffer(out),
                                    &[vk::BufferCopy {
                                        src_offset: offset,
                                        dst_offset: at,
                                        size,
                                    }],
                                );
                                at += size;
                            }
                        }
                    }
                });
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
        let ptr = readback_memory.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
        let motion: Vec<[f32; 2]> = raw[..motion_bytes as usize]
            .chunks_exact(4)
            .map(|p| {
                let h = |a: u8, b: u8| half::to_f32(u16::from_le_bytes([a, b])) * SIZE as f32;
                [h(p[0], p[1]), h(p[2], p[3])]
            })
            .collect();
        let floats = |bytes: &[u8]| -> Vec<Vec3> {
            bytes
                .chunks_exact(12)
                .map(|c| {
                    let f = |i: usize| f32::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
                    Vec3::new(f(0), f(4), f(8))
                })
                .collect()
        };
        let mut at = motion_bytes as usize;
        let mut take = |n: u64| {
            let s = &raw[at..at + n as usize];
            at += n as usize;
            s
        };
        core::array::from_fn(|_| {
            let positions = floats(take(geometry_bytes));
            let normals = take(vertex_count * 16)
                .chunks_exact(16)
                .map(|c| {
                    Vec3::from(decode_octahedral(u32::from_le_bytes([
                        c[0], c[1], c[2], c[3],
                    ])))
                })
                .collect();
            let previous = floats(take(geometry_bytes));
            Frame {
                positions,
                normals,
                previous,
                motion: motion.clone(),
                jitter,
            }
        })
    };

    let mut list_motion = Vec::new();
    for (prepass, use_scene) in [(false, false), (true, false), (false, true), (true, true)] {
        renderer.set_depth_prepass(prepass);
        temporal.reset();
        temporal.set_jitter_phases(2);
        for &(id, _) in &meshes {
            skinning.teleport(id);
        }
        let first = render(
            0.3,
            &renderer,
            &mut skinning,
            use_scene.then_some(&mut scene),
        );
        let second = render(
            0.6,
            &renderer,
            &mut skinning,
            use_scene.then_some(&mut scene),
        );
        for (k, (mesh, joints, weights)) in skins.iter().enumerate() {
            let label = format!(
                "{} influences, prepass {prepass}, scene {use_scene}",
                if k == 0 { 4 } else { 8 }
            );
            for (frame, angle) in [(&first[k], 0.3), (&second[k], 0.6)] {
                let expected = reference(mesh, joints, weights, &palette(angle));
                for (v, (p, n)) in expected.iter().enumerate() {
                    assert!(
                        (frame.positions[v] - *p).length() < 1e-5,
                        "{label}: vertex {v}: {} vs {p}",
                        frame.positions[v]
                    );
                    assert!(
                        frame.normals[v].angle_between(*n) < 1e-3,
                        "{label}: normal {v}: {} vs {n}",
                        frame.normals[v]
                    );
                }
            }
            // After a teleport, the previous positions are the current ones; a frame later
            // they are the first frame's.
            assert_eq!(first[k].previous, first[k].positions, "{label}");
            assert_eq!(second[k].previous, first[k].positions, "{label}");
            // The bounds contain every skinned vertex.
            let (lo, hi) = skinning.bounds(meshes[k].0).unwrap();
            for p in &second[k].positions {
                assert!(
                    p.cmpge(lo - 1e-5).all() && p.cmple(hi + 1e-5).all(),
                    "{label}: {p} outside"
                );
            }
        }
        // Motion: on the bent tip each pixel shows its surface point's rotation about the
        // pivot by 0.3 radians since last frame; below the blend, nothing moves.
        let frame = &second[0];
        let jitter = Vec2::from(frame.jitter);
        let mut checked = 0;
        for bind in [
            Vec3::new(0.0, 3.0, 0.5),
            Vec3::new(0.2, 3.5, 0.5),
            Vec3::new(-0.2, 3.25, 0.5),
            Vec3::new(0.1, 3.8, 0.5),
        ] {
            let now = PIVOT + Quat::from_rotation_z(0.6) * (bind - PIVOT);
            let p = to_pixels(now) + jitter;
            let (x, y) = (p.x.floor() as u32, p.y.floor() as u32);
            let pixel = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
            let unjittered = pixel - jitter;
            let surface = from_pixels(unjittered);
            // Only pixels on the rigid tip: their bind-pose point above y = 2.5.
            let bind = PIVOT + Quat::from_rotation_z(-0.6) * (surface - PIVOT);
            if bind.y < 2.6 || bind.x.abs() > 0.45 {
                continue;
            }
            let before = PIVOT + Quat::from_rotation_z(-0.3) * (surface - PIVOT);
            let expected = unjittered - to_pixels(before);
            let got = frame.motion[(y * SIZE + x) as usize];
            println!("prepass {prepass}: pixel ({x}, {y}): motion {got:?}, expected {expected}");
            assert!(
                (Vec2::from(got) - expected).length() < 0.05,
                "prepass {prepass}: pixel ({x}, {y}): {got:?} vs {expected}"
            );
            checked += 1;
        }
        assert!(checked >= 2, "too few pixels on the tip");
        // The lower part (joint 0 only) is still.
        let still = frame.motion[(44 * SIZE + 32) as usize];
        assert!(Vec2::from(still).length() < 0.01, "{still:?}");
        // The scene path draws the same pixels: identical motion to the draw list's at the
        // same jitter.
        if use_scene {
            let (list_jitter, list) = &list_motion[usize::from(prepass)];
            assert_eq!(
                *list_jitter, frame.jitter,
                "two jitter phases, two frames a run"
            );
            assert!(*list == frame.motion, "scene vs list motion");
        } else {
            list_motion.push((frame.jitter, frame.motion.clone()));
        }
    }

    // SAFETY: the device is idle.
    unsafe {
        let mut temporal = temporal;
        scene.destroy(&mut allocator);
        skinning.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        ring.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        temporal.destroy(&mut allocator);
        allocator.destroy_buffer(readback, readback_memory);
        d.destroy_command_pool(pool, None);
    }
    drop((renderer, skinning, bindless, allocator));
    let report = instance.validation_report();
    assert_eq!(
        report.errors, 0,
        "validation errors: {:#?}",
        report.first_errors
    );
}
