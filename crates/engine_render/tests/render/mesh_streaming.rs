//! Meshes streamed by detail level (ADR 0076) under the validation layer with
//! synchronization validation: a mesh starts with its coarsest level; finer levels arrive
//! one at a time (each a larger vertex prefix, the old one copied on the GPU, the new
//! vertices and the level's indices uploaded) and are evicted again; after every frame the
//! pool holds exactly the resident vertices and each level's indices and meshlets (ADR
//! 0085), and levels not resident draw the finest resident one.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_assets::meshlet::build_meshlets;
use engine_assets::{
    CookedMesh, LodSettings, generate_lods, order_for_streaming, streamed_vertex_counts,
};
use engine_core::math::Vec3;
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph};
use engine_render::mesh::{
    GeometryCapacity, GeometryPool, LodIndices, MeshData, MeshError, MeshId, Meshlet,
    StreamedLevel, encode_octahedral,
};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc, MemoryLocation};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render mesh streaming test"),
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

/// A bumpy 48 × 48 grid with its detail levels and their meshlets, vertices ordered for
/// streaming (the cook's order).
fn mesh() -> (CookedMesh, Vec<u32>) {
    let n = 49u32;
    let mut positions = Vec::new();
    for z in 0..n {
        for x in 0..n {
            let (fx, fz) = (x as f32 / 48.0, z as f32 / 48.0);
            positions.push([fx, 0.1 * (fx * 9.0).sin() * (fz * 7.0).cos(), fz]);
        }
    }
    let mut indices = Vec::new();
    for z in 0..n - 1 {
        for x in 0..n - 1 {
            let a = z * n + x;
            indices.extend_from_slice(&[a, a + n, a + 1, a + 1, a + n, a + n + 1]);
        }
    }
    let lods = generate_lods(&positions, &indices, &LodSettings::default());
    let mut mesh = CookedMesh {
        uvs: positions.iter().map(|p| [p[0], p[2]]).collect(),
        normals: vec![[0.0, 1.0, 0.0]; positions.len()],
        positions,
        indices,
        lods,
        max: [1.0, 0.1, 1.0],
        min: [0.0, -0.1, 0.0],
        ..CookedMesh::default()
    };
    mesh.meshlets = build_meshlets(&mesh.positions, &mut mesh.indices);
    for l in &mut mesh.lods {
        l.meshlets = build_meshlets(&mesh.positions, &mut l.indices);
    }
    assert!(mesh.meshlets.len() > 4 && mesh.lods.iter().all(|l| !l.meshlets.is_empty()));
    order_for_streaming(&mut mesh);
    let counts = streamed_vertex_counts(&mesh).unwrap();
    (mesh, counts)
}

fn level_indices(mesh: &CookedMesh, k: usize) -> &[u32] {
    if k == 0 {
        &mesh.indices
    } else {
        &mesh.lods[k - 1].indices
    }
}

/// Level `k`'s meshlets, as the pool takes them (the cook's are the same 32 bytes).
fn level_meshlets(mesh: &CookedMesh, k: usize) -> &[Meshlet] {
    bytemuck::cast_slice(if k == 0 {
        &mesh.meshlets
    } else {
        &mesh.lods[k - 1].meshlets
    })
}

/// Index buffer words of a level's range: its indices, then its meshlets from the next
/// 32-byte boundary.
fn range_words(indices: usize, meshlets: usize) -> u64 {
    if meshlets == 0 {
        indices as u64
    } else {
        (indices.next_multiple_of(8) + meshlets * 8) as u64
    }
}

/// The new vertices and indices of level `k`.
fn level_data<'a>(mesh: &'a CookedMesh, counts: &[u32], k: usize) -> MeshData<'a> {
    let (first, end) = (counts[k + 1] as usize, counts[k] as usize);
    MeshData {
        positions: &mesh.positions[first..end],
        normals: &mesh.normals[first..end],
        tangents: &[],
        uvs: &mesh.uvs[first..end],
        indices: level_indices(mesh, k),
        meshlets: level_meshlets(mesh, k),
    }
}

struct Readback {
    positions: Vec<[f32; 3]>,
    normals: Vec<u32>,
    indices: Vec<u32>,
}

#[test]
fn levels_stream_in_and_out_and_the_pool_holds_what_is_resident() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let mut pool = GeometryPool::new(
        &device,
        &mut allocator,
        GeometryCapacity {
            vertices: 64,
            indices: 64,
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
    let frame_number = core::cell::Cell::new(0u64);
    let mut run_frame = |pool: &mut GeometryPool, allocator: &mut Allocator| -> Readback {
        frame_number.set(frame_number.get() + 1);
        let current = frame_number.get();
        let (_, vertex_capacity, _, index_capacity) = pool.usage();
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
                .begin_frame(&mut fg, &device, allocator, current)
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
        let words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let (p, rest) = words.split_at((sizes[0] / 4) as usize);
        let (a, i) = rest.split_at((sizes[1] / 4) as usize);
        // SAFETY: the GPU is idle.
        unsafe { allocator.destroy_buffer(readback, readback_mem) };
        Readback {
            positions: p
                .chunks_exact(3)
                .map(|c| {
                    [
                        f32::from_bits(c[0]),
                        f32::from_bits(c[1]),
                        f32::from_bits(c[2]),
                    ]
                })
                .collect(),
            normals: a.chunks_exact(4).map(|c| c[0]).collect(),
            indices: i.to_vec(),
        }
    };

    let (mesh, counts) = mesh();
    let levels: Vec<StreamedLevel> = (0..counts.len())
        .map(|k| StreamedLevel {
            index_count: level_indices(&mesh, k).len() as u32,
            error: if k == 0 { 0.0 } else { mesh.lods[k - 1].error },
            vertices: counts[k],
        })
        .collect();
    let coarsest = counts.len() - 1;
    println!("levels: {levels:?}");
    let coarse = MeshData {
        positions: &mesh.positions[..counts[coarsest] as usize],
        normals: &mesh.normals[..counts[coarsest] as usize],
        tangents: &[],
        uvs: &mesh.uvs[..counts[coarsest] as usize],
        indices: level_indices(&mesh, coarsest),
        meshlets: level_meshlets(&mesh, coarsest),
    };
    let id = pool
        .add_streamed_mesh(&coarse, &levels, Vec3::from(mesh.min), Vec3::from(mesh.max))
        .unwrap();
    // Not on the GPU yet: no level can follow.
    assert!(matches!(
        pool.upload_mesh_level(
            id,
            coarsest as u32 - 1,
            &level_data(&mesh, &counts, coarsest - 1)
        ),
        Err(MeshError::Busy)
    ));

    // What the pool must hold for a mesh resident from `resident`.
    let check = |pool: &GeometryPool, got: &Readback, id: MeshId, resident: usize| {
        let info = pool.mesh(id).unwrap();
        assert_eq!(
            pool.mesh_residency(id),
            Some((resident as u32, counts.len() as u32))
        );
        let (v, n) = (info.first_vertex as usize, counts[resident] as usize);
        assert_eq!(info.vertex_count as usize, n);
        assert_eq!(&got.positions[v..v + n], &mesh.positions[..n], "positions");
        let normal = encode_octahedral([0.0, 1.0, 0.0]);
        assert!(
            got.normals[v..v + n].iter().all(|&a| a == normal),
            "normals"
        );
        for k in 0..counts.len() {
            let lod = info.lods[k];
            // Levels not resident draw the finest resident one.
            let shown = k.max(resident);
            let expected = level_indices(&mesh, shown);
            assert_eq!(lod.index_count as usize, expected.len(), "level {k}");
            let i = lod.first_index as usize;
            assert_eq!(
                &got.indices[i..i + expected.len()],
                expected,
                "level {k} indices"
            );
            // Its meshlets after them, 32-byte aligned.
            let meshlets = level_meshlets(&mesh, shown);
            assert_eq!(
                lod.meshlet_count as usize,
                meshlets.len(),
                "level {k} meshlets"
            );
            let m = lod.first_meshlet as usize;
            assert_eq!(m % 8, 0, "level {k} meshlets aligned");
            assert!(
                m >= i + expected.len(),
                "level {k} meshlets after its indices"
            );
            let words: &[u32] = bytemuck::cast_slice(meshlets);
            assert_eq!(
                &got.indices[m..m + words.len()],
                words,
                "level {k} meshlets"
            );
        }
        let bytes = n as u64 * 28
            + (resident..counts.len())
                .map(|k| {
                    range_words(
                        level_indices(&mesh, k).len(),
                        level_meshlets(&mesh, k).len(),
                    ) * 4
                })
                .sum::<u64>();
        assert_eq!(pool.streamed_bytes(), bytes);
    };
    let got = run_frame(&mut pool, &mut allocator);
    check(&pool, &got, id, coarsest);

    // Finer levels one at a time, each in a frame of its own; a second level in the same
    // frame must wait.
    for k in (0..coarsest).rev() {
        pool.upload_mesh_level(id, k as u32, &level_data(&mesh, &counts, k))
            .unwrap();
        if k > 0 {
            assert!(matches!(
                pool.upload_mesh_level(id, k as u32 - 1, &level_data(&mesh, &counts, k - 1)),
                Err(MeshError::Busy)
            ));
        }
        let got = run_frame(&mut pool, &mut allocator);
        check(&pool, &got, id, k);
    }
    // A level that does not follow is refused.
    assert!(matches!(
        pool.upload_mesh_level(id, 0, &level_data(&mesh, &counts, 0)),
        Err(MeshError::BadLevel)
    ));

    // Evict down to the coarsest but one: the full mesh's vertices and the finer levels'
    // indices and meshlets go; what stays is moved and intact.
    let kept = coarsest - 1;
    pool.evict_mesh_levels(id, kept as u32, frame_number.get())
        .unwrap();
    let got = run_frame(&mut pool, &mut allocator);
    check(&pool, &got, id, kept);
    // Meshlets that do not cover their level are refused.
    let mut bad: Vec<Meshlet> = level_meshlets(&mesh, kept - 1).to_vec();
    bad[0].triangles += 1;
    assert!(matches!(
        pool.upload_mesh_level(
            id,
            kept as u32 - 1,
            &MeshData {
                meshlets: &bad,
                ..level_data(&mesh, &counts, kept - 1)
            }
        ),
        Err(MeshError::BadMeshlets)
    ));
    // And back up.
    for k in (0..kept).rev() {
        pool.upload_mesh_level(id, k as u32, &level_data(&mesh, &counts, k))
            .unwrap();
        let got = run_frame(&mut pool, &mut allocator);
        check(&pool, &got, id, k);
    }

    // The same mesh whole, with its levels: their indices one after the other, then every
    // level's meshlets.
    let lods: Vec<LodIndices<'_>> = (1..counts.len())
        .map(|k| LodIndices {
            indices: level_indices(&mesh, k),
            error: mesh.lods[k - 1].error,
            meshlets: level_meshlets(&mesh, k),
        })
        .collect();
    let whole = pool
        .add_mesh_with_lods(
            &MeshData {
                positions: &mesh.positions,
                normals: &mesh.normals,
                tangents: &[],
                uvs: &mesh.uvs,
                indices: &mesh.indices,
                meshlets: level_meshlets(&mesh, 0),
            },
            &lods,
        )
        .unwrap();
    let got = run_frame(&mut pool, &mut allocator);
    let info = *pool.mesh(whole).unwrap();
    for k in 0..counts.len() {
        let lod = info.lods[k];
        let (indices, meshlets) = (level_indices(&mesh, k), level_meshlets(&mesh, k));
        let i = lod.first_index as usize;
        assert_eq!(
            &got.indices[i..i + indices.len()],
            indices,
            "whole level {k}"
        );
        let m = lod.first_meshlet as usize;
        assert_eq!((m % 8, lod.meshlet_count as usize), (0, meshlets.len()));
        let words: &[u32] = bytemuck::cast_slice(meshlets);
        assert_eq!(
            &got.indices[m..m + words.len()],
            words,
            "whole level {k} meshlets"
        );
    }
    pool.remove_mesh(whole, frame_number.get());
    // Meshlets that do not cover their level are refused here too.
    assert!(matches!(
        pool.add_mesh(&MeshData {
            positions: &mesh.positions,
            normals: &mesh.normals,
            tangents: &[],
            uvs: &mesh.uvs,
            indices: &mesh.indices[3..],
            meshlets: level_meshlets(&mesh, 0),
        }),
        Err(MeshError::BadMeshlets)
    ));
    pool.remove_mesh(id, frame_number.get());
    run_frame(&mut pool, &mut allocator);
    run_frame(&mut pool, &mut allocator);
    assert_eq!(pool.streamed_bytes(), 0);
    let (vertices, _, indices, _) = pool.usage();
    assert_eq!((vertices, indices), (0, 0), "every range is free again");

    // SAFETY: the device is idle.
    unsafe {
        pool.destroy(&mut allocator);
        graph.destroy(&mut allocator, None);
        d.destroy_command_pool(command_pool, None);
    }
    drop(allocator);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
