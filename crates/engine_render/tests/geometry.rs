//! The geometry pool (ADR 0039) under the validation layer with synchronization
//! validation: meshes added beyond the pool's capacity grow its buffers (the old contents
//! copied on the GPU), removed meshes release their ranges only after their frames, and
//! whatever the pool holds is read back and compared with the encoded input, byte for
//! byte.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::Vec3;
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph};
use engine_render::mesh::{
    GeometryCapacity, GeometryPool, MeshId, OwnedMeshData, VertexAttributes, cube,
    encode_octahedral, encode_tangent,
};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc, MemoryLocation};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render geometry test"),
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

/// A grid of `n × n` quads in the XZ plane, with texture coordinates and no tangents.
fn grid(n: u32, height: f32) -> OwnedMeshData {
    let mut mesh = OwnedMeshData::default();
    for z in 0..=n {
        for x in 0..=n {
            mesh.positions.push([x as f32, height, z as f32]);
            mesh.normals.push([0.0, 1.0, 0.0]);
            mesh.uvs.push([x as f32 / n as f32, z as f32 / n as f32]);
        }
    }
    for z in 0..n {
        for x in 0..n {
            let i = z * (n + 1) + x;
            mesh.indices
                .extend([i, i + n + 1, i + 1, i + 1, i + n + 1, i + n + 2]);
        }
    }
    mesh
}

/// What the pool should hold for a mesh, as the shaders read it.
fn expected_attributes(mesh: &OwnedMeshData) -> Vec<VertexAttributes> {
    (0..mesh.positions.len())
        .map(|v| VertexAttributes {
            normal: encode_octahedral(mesh.normals[v]),
            tangent: mesh.tangents.get(v).map_or(0, |&t| encode_tangent(t)),
            uv: mesh.uvs.get(v).copied().unwrap_or([0.0; 2]),
        })
        .collect()
}

struct Readback {
    positions: Vec<[f32; 3]>,
    attributes: Vec<VertexAttributes>,
    indices: Vec<u32>,
}

#[test]
fn meshes_survive_growth_removal_and_reuse() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    // Tiny on purpose: the first mesh already grows every buffer.
    let mut pool = GeometryPool::new(
        &device,
        &mut allocator,
        GeometryCapacity {
            vertices: 8,
            indices: 12,
        },
        1,
    )
    .unwrap();
    let mut graph = RenderGraph::new(&device, 1);
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

    // Runs one frame: `edit` changes the pool first; then everything it holds is read
    // back.
    let mut run_frame =
        |pool: &mut GeometryPool, allocator: &mut Allocator, frame_number: u64| -> Readback {
            let (vertices, vertex_capacity, _, index_capacity) = pool.usage();
            let _ = vertices;
            let sizes = [
                vertex_capacity * 12,
                vertex_capacity * 16,
                index_capacity * 4,
            ];
            let total: u64 = sizes.iter().sum();
            let (readback, readback_mem) = allocator
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(total)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST),
                    MemoryLocation::Readback,
                )
                .unwrap();
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
            {
                // SAFETY: nothing is in flight.
                let mut fg = unsafe { graph.begin_slot(0, cmd) };
                let geo = pool
                    .begin_frame(&mut fg, &device, allocator, frame_number)
                    .unwrap();
                let out = fg.import_buffer(
                    "readback",
                    ImportedBuffer {
                        buffer: readback,
                        size: total,
                        address: 0,
                        initial: BufferState::idle(),
                        final_state: Some(BufferState::host_read()),
                    },
                );
                let sources = [geo.positions, geo.attributes, geo.indices];
                fg.add_pass("read back")
                    .read(geo.positions, Access::TransferRead)
                    .read(geo.attributes, Access::TransferRead)
                    .read(geo.indices, Access::TransferRead)
                    .write(out, Access::TransferWrite)
                    .execute(move |ctx| {
                        let mut offset = 0;
                        for (source, size) in sources.into_iter().zip(sizes) {
                            // SAFETY: recording; the pool's buffers are at least this large.
                            unsafe {
                                ctx.device.cmd_copy_buffer(
                                    ctx.cmd,
                                    ctx.buffer(source),
                                    ctx.buffer(out),
                                    &[vk::BufferCopy {
                                        src_offset: 0,
                                        dst_offset: offset,
                                        size,
                                    }],
                                );
                            }
                            offset += size;
                        }
                    });
                fg.execute(allocator).unwrap();
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
            let ptr = readback_mem.mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping covers the buffer.
            let bytes: &[u8] = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), total as usize) };
            let (p, rest) = bytes.split_at(sizes[0] as usize);
            let (a, i) = rest.split_at(sizes[1] as usize);
            let floats = |b: &[u8]| -> Vec<f32> {
                b.chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect()
            };
            let words = |b: &[u8]| -> Vec<u32> {
                b.chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect()
            };
            let positions = floats(p)
                .chunks_exact(3)
                .map(|c| [c[0], c[1], c[2]])
                .collect();
            let attributes = words(a)
                .chunks_exact(4)
                .map(|c| VertexAttributes {
                    normal: c[0],
                    tangent: c[1],
                    uv: [f32::from_bits(c[2]), f32::from_bits(c[3])],
                })
                .collect();
            let indices = words(&i[..sizes[2] as usize]);
            // SAFETY: the GPU is idle.
            unsafe { allocator.destroy_buffer(readback, readback_mem) };
            Readback {
                positions,
                attributes,
                indices,
            }
        };

    let check =
        |pool: &GeometryPool, got: &Readback, id: MeshId, mesh: &OwnedMeshData, name: &str| {
            let info = pool.mesh(id).unwrap();
            let (v, n) = (info.first_vertex as usize, mesh.positions.len());
            assert_eq!(
                &got.positions[v..v + n],
                &mesh.positions[..],
                "{name}: positions"
            );
            let expected = expected_attributes(mesh);
            for (k, (a, e)) in got.attributes[v..v + n].iter().zip(&expected).enumerate() {
                assert_eq!(a.normal, e.normal, "{name}: normal {k}");
                assert_eq!(a.uv, e.uv, "{name}: uv {k}");
                if !mesh.tangents.is_empty() {
                    assert_eq!(a.tangent, e.tangent, "{name}: tangent {k}");
                }
            }
            let i = info.first_index as usize;
            assert_eq!(
                &got.indices[i..i + mesh.indices.len()],
                &mesh.indices[..],
                "{name}: indices"
            );
            assert_eq!(info.index_count as usize, mesh.indices.len());
            let lo = mesh
                .positions
                .iter()
                .fold(Vec3::splat(f32::MAX), |m, &p| m.min(Vec3::from(p)));
            assert_eq!(info.min, lo, "{name}: bounds");
        };

    // Frame 1: two meshes, both beyond the initial capacity.
    let a_data = cube();
    let b_data = grid(12, 2.0);
    let a = pool.add_mesh(&a_data.as_data()).unwrap();
    let b = pool.add_mesh(&b_data.as_data()).unwrap();
    let got = run_frame(&mut pool, &mut allocator, 1);
    check(&pool, &got, a, &a_data, "a");
    check(&pool, &got, b, &b_data, "b");
    let a_range = *pool.mesh(a).unwrap();
    println!("after frame 1: {:?}", pool.usage());

    // Frame 2: A removed (its ranges wait for frame 2 to finish); C added, and much more
    // than fits, so the buffers grow again with B inside.
    pool.remove_mesh(a, 2);
    assert!(pool.mesh(a).is_none(), "stale id");
    let c_data = grid(64, -1.0);
    let c = pool.add_mesh(&c_data.as_data()).unwrap();
    let c_info = *pool.mesh(c).unwrap();
    assert!(
        c_info.first_vertex >= a_range.first_vertex + a_range.vertex_count
            || c_info.first_vertex + c_info.vertex_count <= a_range.first_vertex,
        "A's range is not reused before its frame has finished"
    );
    let got = run_frame(&mut pool, &mut allocator, 2);
    check(&pool, &got, b, &b_data, "b after growth");
    check(&pool, &got, c, &c_data, "c");
    println!("after frame 2: {:?}", pool.usage());

    // Frame 3: A's ranges are free again; D (a cube, A's size) fits where A was.
    let got_before = run_frame(&mut pool, &mut allocator, 3);
    let _ = got_before;
    let d_data = cube();
    let d_id = pool.add_mesh(&d_data.as_data()).unwrap();
    let d_info = *pool.mesh(d_id).unwrap();
    assert_eq!(
        (d_info.first_vertex, d_info.first_index),
        (a_range.first_vertex, a_range.first_index),
        "A's ranges are reused"
    );
    let got = run_frame(&mut pool, &mut allocator, 4);
    check(&pool, &got, b, &b_data, "b");
    check(&pool, &got, c, &c_data, "c");
    check(&pool, &got, d_id, &d_data, "d");
    assert_eq!(pool.mesh_count(), 3);
    println!("after frame 4: {:?}", pool.usage());

    // SAFETY: the device is idle.
    unsafe {
        pool.destroy(&mut allocator);
        graph.destroy(&mut allocator, None);
        d.destroy_command_pool(command_pool, None);
    }
    drop(allocator);
    let report = instance.validation_report();
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
