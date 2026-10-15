//! The frame's layout ([`ForwardRenderer::layout_draws`], [`ForwardRenderer::layout_scene`]):
//! a draw list or a persistent scene turned into batches by pipeline state, their draw
//! commands and slots, written into the upload ring, as the passes of `declare` read them.

use super::*;

impl ForwardRenderer {
    /// Lays out a draw list: instances, batches (runs of consecutive draws) and draw
    /// commands, all written into `ring`.
    #[allow(clippy::too_many_lines)]
    pub(super) fn layout_draws(
        &self,
        ring: &mut UploadRing,
        geometry: &GeometryFrame<'_>,
        materials: &MaterialFrame<'_>,
        draws: &[DrawItem],
        view: &ForwardView<'_>,
        setup: &Setup,
    ) -> Option<Layout> {
        let Setup {
            phases,
            cascade_count,
            shadow_draws,
            ..
        } = *setup;
        let lists = phases + cascade_count + shadow_draws;
        let instance_count = draws.len() as u32;
        let mut scratch = self.scratch.borrow_mut();
        let Scratch {
            slots,
            runs,
            keys,
            commands: command_scratch,
            ..
        } = &mut *scratch;
        // Batches in draw order, counted per pipeline state, then ordered by state and,
        // within a state, front to back (by the first instance's box), so the depth test
        // rejects hidden fragments before shading.
        let mut counts = [0u32; STATES];
        // Subsurface materials get a pipeline state of their own while scattering is on
        // and there are profiles; otherwise they shade as ordinary ones.
        let subsurface_on = self.subsurface_for(view).is_some();
        batches(draws, materials, slots, runs, &mut counts, subsurface_on);
        let subsurface_draws = counts[SUBSURFACE..SUBSURFACE + 8].iter().any(|&c| c > 0);
        let mut starts = [0u32; STATES];
        let mut next = 0;
        for (start, count) in starts.iter_mut().zip(counts) {
            *start = next;
            next += count;
        }
        keys.clear();
        keys.extend(runs.iter().enumerate().map(|(k, run)| {
            let draw = &draws[run.first as usize];
            let (center, extents) = local_box(geometry, draw.mesh);
            let (center, _) = crate::cull::transform_box(&draw.world_from_object, center, extents);
            let distance = center.distance(view.camera.position).max(0.0).to_bits();
            // Opaque batches front to back (the depth test rejects hidden fragments early),
            // blended ones back to front (each blends over what is behind it).
            let distance = if run.state & BLENDED != 0 {
                !distance
            } else {
                distance
            };
            (sort_key(run, distance), k as u32)
        }));
        keys.sort_unstable();
        for (order, &(_, k)) in keys.iter().enumerate() {
            runs[k as usize].order = order as u32;
        }
        let batch_count = runs.len() as u32;
        // Draw commands: one per detail level of each batch's mesh, a batch's levels
        // together and the batches in state order (so each state stays one indirect draw).
        // Each command has its own range of instance slots, in draw order (without LODs a
        // draw's slot is its index, as before LODs).
        counts = [0; STATES];
        let mut command_count = 0u32;
        for &(_, k) in keys.iter() {
            let run = &mut runs[k as usize];
            run.lods = geometry
                .mesh(draws[run.first as usize].mesh)
                .map_or(1, |m| m.lod_count.clamp(1, crate::mesh::MAX_LODS as u32));
            run.command = command_count;
            command_count += run.lods;
            counts[run.state] += run.lods;
        }
        let typed = typed_ranges(keys, runs);
        let mut slot_count = 0u32;
        for run in runs.iter_mut() {
            run.slot = slot_count;
            slot_count += run.lods * run.len;
        }
        let mut next = 0;
        for (start, count) in starts.iter_mut().zip(counts) {
            *start = next;
            next += count;
        }
        // Instances in draw order, and the scene's bounding box for the light on the way.
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        let instances = {
            let (out, staged) = ring.alloc::<InstanceData>(draws.len())?;
            for run in runs.iter() {
                let (center, extents) = local_box(geometry, draws[run.first as usize].mesh);
                let range = run.first as usize..(run.first + run.len) as usize;
                let items = draws[range.clone()].iter().zip(&slots[range.clone()]);
                for ((item, &material), slot) in items.zip(&mut out[range]) {
                    let (world_center, world_extents) =
                        crate::cull::transform_box(&item.world_from_object, center, extents);
                    min = min.min(world_center - world_extents);
                    max = max.max(world_center + world_extents);
                    let rows = item.world_from_object.transpose();
                    slot.write(InstanceData {
                        rows: [
                            rows.x_axis.to_array(),
                            rows.y_axis.to_array(),
                            rows.z_axis.to_array(),
                        ],
                        material,
                        batch: run.order,
                        _pad: [0; 2],
                    });
                }
            }
            staged
        };
        let instances_staged = instances;
        let instances = instances.address;
        // The ring's memory takes writes best in order (it may be write-combined device
        // memory): the batch table and the command lists are written front to back.
        let batch_table = {
            let (out, staged) = ring.alloc::<BatchData>(batch_count as usize)?;
            for &(_, k) in keys.iter() {
                let run = &runs[k as usize];
                let mesh = draws[run.first as usize].mesh;
                let (center, extents) = local_box(geometry, mesh);
                let lods = geometry.mesh(mesh).map(|m| m.lods).unwrap_or_default();
                out[run.order as usize].write(BatchData {
                    command: run.command,
                    slot: run.slot,
                    count: run.len,
                    lods: run.lods | u32::from(run.state & BLENDED != 0) << 8,
                    center: center.extend(0.0).to_array(),
                    extents: extents.extend(0.0).to_array(),
                    errors: lods.map(|l| l.error),
                });
            }
            staged.address
        };
        // The meshlet lists (ADR 0085), with the batch table's order.
        let meshlets = if self.meshlet_shaders().is_some() && !setup.capture {
            meshlet_layout(
                ring,
                geometry,
                batch_count as usize,
                instance_count,
                keys.iter().map(|&(_, k)| {
                    let run = &runs[k as usize];
                    (run.order as usize, run, draws[run.first as usize].mesh)
                }),
            )
        } else {
            None
        };
        // The virtual shadow pages' lists (ADR 0093 step 3b).
        let vsm = if setup.vsm {
            vsm_layout(
                ring,
                geometry,
                batch_count as usize,
                instance_count,
                keys.iter().map(|&(_, k)| {
                    let run = &runs[k as usize];
                    (run.order as usize, run, draws[run.first as usize].mesh)
                }),
            )
        } else {
            None
        };
        // Draw commands, in state order: the forward pass's for each phase, then the shadow
        // pass's for each cascade, with no instances until culling counts them (each list's
        // slots follow the previous list's).
        let commands = {
            let per_list = command_count as usize;
            let (out, staged) =
                ring.alloc::<vk::DrawIndexedIndirectCommand>(lists as usize * per_list)?;
            // One list's commands, in command order (the sorted batches' levels)...
            command_scratch.clear();
            for &(_, k) in keys.iter() {
                let run = &runs[k as usize];
                // A removed mesh draws nothing.
                let mesh = geometry.mesh(draws[run.first as usize].mesh);
                for lod in 0..run.lods {
                    let level = mesh.map(|m| m.lods[lod as usize]).unwrap_or_default();
                    command_scratch.push(vk::DrawIndexedIndirectCommand {
                        index_count: level.index_count,
                        instance_count: 0,
                        first_index: level.first_index,
                        vertex_offset: mesh.map_or(0, |m| m.first_vertex as i32),
                        first_instance: run.slot + lod * run.len,
                    });
                }
            }
            // ... then copied into each list, which has its own slot region.
            for list in 0..lists {
                let slots = list_offset(list, phases, cascade_count) * slot_count;
                let out = &mut out[list as usize * per_list..(list as usize + 1) * per_list];
                for (out, command) in out.iter_mut().zip(command_scratch.iter()) {
                    out.write(vk::DrawIndexedIndirectCommand {
                        first_instance: command.first_instance + slots,
                        ..*command
                    });
                }
            }
            staged
        };
        drop(scratch);
        Some(Layout {
            instance_count,
            instances,
            source: Source::Ring(instances_staged),
            batch_table,
            commands,
            command_count,
            drawn_commands: command_count,
            slot_count,
            batch_count,
            states: States {
                starts,
                counts,
                typed,
            },
            min,
            max,
            subsurface_draws,
            meshlets,
            vsm,
        })
    }

    /// Lays out a persistent scene: its batches and their draw commands, written into
    /// `ring`, with the sink batch's command and slots after them.
    #[allow(clippy::too_many_lines)]
    pub(super) fn layout_scene(
        &self,
        ring: &mut UploadRing,
        geometry: &GeometryFrame<'_>,
        scene: &SceneFrame<'_>,
        view: &ForwardView<'_>,
        setup: &Setup,
    ) -> Option<Layout> {
        let Setup {
            phases,
            cascade_count,
            shadow_draws,
            ..
        } = *setup;
        let lists = phases + cascade_count + shadow_draws;
        let mut scratch = self.scratch.borrow_mut();
        let Scratch {
            runs,
            keys,
            commands: command_scratch,
            ..
        } = &mut *scratch;
        let subsurface_on = self.subsurface_for(view).is_some();
        let batches = scene.batches();
        // One run per live batch (`first` holds its id), counted per pipeline state.
        let mut counts = [0u32; STATES];
        runs.clear();
        for (id, batch) in batches.iter().enumerate().skip(1) {
            if !batch.alive || batch.count == 0 {
                continue;
            }
            let mut state = batch.state as usize;
            if !subsurface_on {
                state &= !SUBSURFACE;
            }
            runs.push(Run {
                first: id as u32,
                len: batch.count,
                state,
                ty: batch.ty,
                order: 0,
                command: 0,
                slot: 0,
                lods: 1,
            });
            counts[state] += 1;
        }
        let subsurface_draws = counts[SUBSURFACE..SUBSURFACE + 8].iter().any(|&c| c > 0);
        // Ordered by state, then front to back (blended: back to front) by the batch's
        // center, as the draw-list path orders its runs.
        keys.clear();
        keys.extend(runs.iter().enumerate().map(|(k, run)| {
            let center = scene.batch_center(run.first);
            let distance = center.distance(view.camera.position).max(0.0).to_bits();
            let distance = if run.state & BLENDED != 0 {
                !distance
            } else {
                distance
            };
            (sort_key(run, distance), k as u32)
        }));
        keys.sort_unstable();
        for (order, &(_, k)) in keys.iter().enumerate() {
            runs[k as usize].order = order as u32;
        }
        let batch_count = runs.len() as u32;
        counts = [0; STATES];
        let mut command_count = 0u32;
        for &(_, k) in keys.iter() {
            let run = &mut runs[k as usize];
            run.lods = geometry
                .mesh(batches[run.first as usize].mesh)
                .map_or(1, |m| m.lod_count.clamp(1, crate::mesh::MAX_LODS as u32));
            run.command = command_count;
            command_count += run.lods;
            counts[run.state] += run.lods;
        }
        let mut slot_count = 0u32;
        for run in runs.iter_mut() {
            run.slot = slot_count;
            slot_count += run.lods * run.len;
        }
        let typed = typed_ranges(keys, runs);
        let drawn_commands = command_count;
        // The sink: its command follows the drawn ones in every list (no state draws it),
        // its slots follow theirs; culling appends holes there when it keeps them.
        let holes = scene.holes();
        let sink_command = command_count;
        let sink_slot = slot_count;
        command_count += 1;
        slot_count += holes;
        let mut starts = [0u32; STATES];
        let mut next = 0;
        for (start, count) in starts.iter_mut().zip(counts) {
            *start = next;
            next += count;
        }
        // The batch table, indexed by batch id: what the instances' `batch` fields name.
        let batch_table = {
            let (out, staged) = ring.alloc::<BatchData>(batches.len())?;
            // The sink's entry (id 0, named by the holes), and dead batches' (named by
            // no instance): blended (no shadows, nothing early), one level, and an
            // inside-out box (negative extents) that fails every frustum plane, so holes
            // are culled without touching the sink's counter. With culling off they are
            // appended to the sink's command, which no state draws.
            let empty = BatchData {
                command: sink_command,
                slot: sink_slot,
                count: holes,
                lods: 1 | 1 << 8,
                center: [0.0; 4],
                extents: [-1e30, -1e30, -1e30, 0.0],
                errors: [0.0; 4],
            };
            for out in out.iter_mut() {
                out.write(empty);
            }
            for run in runs.iter() {
                let mesh = batches[run.first as usize].mesh;
                let (center, extents) = local_box(geometry, mesh);
                let lods = geometry.mesh(mesh).map(|m| m.lods).unwrap_or_default();
                out[run.first as usize].write(BatchData {
                    command: run.command,
                    slot: run.slot,
                    count: run.len,
                    lods: run.lods | u32::from(run.state & BLENDED != 0) << 8,
                    center: center.extend(0.0).to_array(),
                    extents: extents.extend(0.0).to_array(),
                    errors: lods.map(|l| l.error),
                });
            }
            staged.address
        };
        // The meshlet lists (ADR 0085), indexed by batch id as the batch table.
        let meshlets = if self.meshlet_shaders().is_some() && !setup.capture {
            meshlet_layout(
                ring,
                geometry,
                batches.len(),
                scene.slot_count(),
                runs.iter()
                    .map(|run| (run.first as usize, run, batches[run.first as usize].mesh)),
            )
        } else {
            None
        };
        // The virtual shadow pages' lists (ADR 0093 step 3b).
        let vsm = if setup.vsm {
            vsm_layout(
                ring,
                geometry,
                batches.len(),
                scene.slot_count(),
                runs.iter()
                    .map(|run| (run.first as usize, run, batches[run.first as usize].mesh)),
            )
        } else {
            None
        };
        let commands = {
            let per_list = command_count as usize;
            let (out, staged) =
                ring.alloc::<vk::DrawIndexedIndirectCommand>(lists as usize * per_list)?;
            command_scratch.clear();
            for &(_, k) in keys.iter() {
                let run = &runs[k as usize];
                // A removed mesh draws nothing.
                let mesh = geometry.mesh(batches[run.first as usize].mesh);
                for lod in 0..run.lods {
                    let level = mesh.map(|m| m.lods[lod as usize]).unwrap_or_default();
                    command_scratch.push(vk::DrawIndexedIndirectCommand {
                        index_count: level.index_count,
                        instance_count: 0,
                        first_index: level.first_index,
                        vertex_offset: mesh.map_or(0, |m| m.first_vertex as i32),
                        first_instance: run.slot + lod * run.len,
                    });
                }
            }
            command_scratch.push(vk::DrawIndexedIndirectCommand {
                index_count: 0,
                instance_count: 0,
                first_index: 0,
                vertex_offset: 0,
                first_instance: sink_slot,
            });
            for list in 0..lists {
                let slots = list_offset(list, phases, cascade_count) * slot_count;
                let out = &mut out[list as usize * per_list..(list as usize + 1) * per_list];
                for (out, command) in out.iter_mut().zip(command_scratch.iter()) {
                    out.write(vk::DrawIndexedIndirectCommand {
                        first_instance: command.first_instance + slots,
                        ..*command
                    });
                }
            }
            staged
        };
        drop(scratch);
        let (min, max) = scene.bounds();
        Some(Layout {
            instance_count: scene.slot_count(),
            instances: scene.current_address,
            source: Source::Scene {
                current: scene.current,
                previous: scene.previous,
                previous_address: scene.previous_address,
            },
            batch_table,
            commands,
            command_count,
            drawn_commands,
            slot_count,
            batch_count,
            states: States {
                starts,
                counts,
                typed,
            },
            min,
            max,
            subsurface_draws,
            meshlets,
            vsm,
        })
    }
}

/// Room for meshlet draws in each forward phase's lists (ADR 0085); instances beyond it
/// keep their instanced draw. 2^17 draws: 2.6 MB of commands and 0.5 MB of slots a phase.
const MAX_MESHLET_DRAWS: u64 = 1 << 17;

/// Bytes of a phase's meshlet lists' header (`MeshletHeader` in `forward.slang`).
pub(super) const MESHLET_HEADER_BYTES: u64 = 528;

/// Where the dispatch arguments of the meshlet cull are in a phase's lists.
pub(super) const MESHLET_ARGS: u64 = 512;

/// Meshlets per work item of the meshlet cull (`MESHLET_CHUNK` in `forward.slang`).
const MESHLET_CHUNK: u32 = 64;

/// A frame's meshlet lists (ADR 0085), the same in every forward phase: the batches'
/// meshlets and, per pipeline state, room for the draws of every instance that may take
/// the path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MeshletLayout {
    /// The batches' meshlets (`BatchMeshletData`), indexed as the batch table.
    pub(super) table: vk::DeviceAddress,
    /// The lists' header with its counters zero, in the ring: copied into every phase's
    /// lists before culling.
    pub(super) header: crate::upload::Staged,
    /// Per state: its first draw and its draws at most.
    pub(super) offsets: [u32; STATES],
    pub(super) caps: [u32; STATES],
    /// Draws in a phase's lists (the sum of `caps`).
    pub(super) draws: u32,
    /// Work items a phase's lists have room for: a chunk of up to [`MESHLET_CHUNK`]
    /// meshlets each. The instances that fit fill at most one more each than their draws.
    pub(super) work: u32,
}

impl MeshletLayout {
    /// Where the draws start in a phase's lists, after the work items.
    pub(super) fn draws_offset(&self) -> u64 {
        MESHLET_HEADER_BYTES + 8 * u64::from(self.work)
    }

    /// Bytes of a phase's lists.
    pub(super) fn list_bytes(&self) -> u64 {
        (self.draws_offset() + COMMAND_BYTES * u64::from(self.draws)).next_multiple_of(16)
    }
}

/// Lays out the meshlet lists (ADR 0085) of `runs` (`(index in the batch table, run,
/// mesh)`) for a batch table of `len` and `instances` culled: each eligible run's levels'
/// meshlets, and per pipeline state room for the draws of all its instances, scaled down
/// to [`MAX_MESHLET_DRAWS`]. Blended runs (drawn back to front) and those of material
/// types (their own pipelines) are not eligible. `None` when no run is, or the ring is
/// full.
fn meshlet_layout<'r>(
    ring: &mut UploadRing,
    geometry: &GeometryFrame<'_>,
    len: usize,
    instances: u32,
    runs: impl Iterator<Item = (usize, &'r Run, MeshId)> + Clone,
) -> Option<MeshletLayout> {
    let eligible = |run: &Run, mesh: MeshId| {
        let info = geometry.mesh(mesh)?;
        let levels = run.lods as usize;
        (run.state & BLENDED == 0
            && run.ty == crate::material::MaterialTypeId::GLTF
            && info.lods[..levels].iter().any(|l| l.meshlet_count > 0))
        .then_some(info)
    };
    let mut wanted = [0u64; STATES];
    for (_, run, mesh) in runs.clone() {
        if let Some(info) = eligible(run, mesh) {
            let most = info.lods[..run.lods as usize]
                .iter()
                .map(|l| l.meshlet_count)
                .max()
                .unwrap_or(0);
            wanted[run.state] += u64::from(run.len) * u64::from(most);
        }
    }
    let total: u64 = wanted.iter().sum();
    if total == 0 {
        return None;
    }
    let mut caps = [0u32; STATES];
    let mut offsets = [0u32; STATES];
    let mut draws = 0u32;
    for state in 0..STATES {
        caps[state] = if total > MAX_MESHLET_DRAWS {
            (wanted[state] * MAX_MESHLET_DRAWS / total) as u32
        } else {
            wanted[state] as u32
        };
        offsets[state] = draws;
        draws += caps[state];
    }
    let table = {
        let (out, staged) = ring.alloc::<BatchMeshletData>(len.max(1))?;
        for out in out.iter_mut() {
            out.write(BatchMeshletData::default());
        }
        for (index, run, mesh) in runs {
            let Some(info) = eligible(run, mesh) else {
                continue;
            };
            let mut entry = BatchMeshletData {
                state: run.state as u32,
                cone: u32::from(run.state & DOUBLE_SIDED == 0),
                ..BatchMeshletData::default()
            };
            for (k, level) in info.lods[..run.lods as usize].iter().enumerate() {
                // The pool's meshlets are 8 words, 32-byte aligned.
                entry.first[k] = level.first_meshlet / 8;
                entry.count[k] = level.meshlet_count;
            }
            if let Some(slot) = out.get_mut(index) {
                slot.write(entry);
            }
        }
        staged.address
    };
    let work = instances + draws.div_ceil(MESHLET_CHUNK);
    // The header: counters zero, then each state's offset and room, then the meshlet
    // cull's dispatch (no work items yet; one group high and deep) and the room for work
    // items.
    let mut header = [0u32; (MESHLET_HEADER_BYTES / 4) as usize];
    header[64..96].copy_from_slice(&offsets);
    header[96..128].copy_from_slice(&caps);
    header[128..132].copy_from_slice(&[0, 1, 1, work]);
    let header = ring.stage(&header)?;
    Some(MeshletLayout {
        table,
        header,
        offsets,
        caps,
        draws,
        work,
    })
}

/// Room in the virtual shadow pages' lists (ADR 0093 step 3b) for each caster: an instance
/// is drawn into a few of their levels at once.
const VSM_LEVELS_PER_INSTANCE: u64 = 4;
/// The pages' draws at most: 2^19 (10 MB of commands, 2 MB of slots).
const MAX_VSM_DRAWS: u64 = 1 << 19;
/// Levels of the pages (`MAX_LEVELS` in vsm.rs): an instance takes the meshlet path for
/// each at most once.
const VSM_LEVELS: u32 = crate::vsm::MAX_LEVELS;

/// Lays out the virtual shadow pages' lists (ADR 0093 step 3b) as a phase's meshlet lists
/// (`runs` and `len` as for [`meshlet_layout`]): their own batch table (every batch's
/// state, and its levels' meshlets where it may take the meshlet path) and per pipeline
/// state room for the meshlets of its instances that may take it and a whole draw of the
/// others, [`VSM_LEVELS_PER_INSTANCE`] levels each, scaled down to [`MAX_VSM_DRAWS`].
/// Blended runs cast no shadows. `None` when nothing casts, or the ring is full.
fn vsm_layout<'r>(
    ring: &mut UploadRing,
    geometry: &GeometryFrame<'_>,
    len: usize,
    instances: u32,
    runs: impl Iterator<Item = (usize, &'r Run, MeshId)> + Clone,
) -> Option<MeshletLayout> {
    let meshlets = |run: &Run, mesh: MeshId| {
        let info = geometry.mesh(mesh)?;
        let levels = run.lods as usize;
        (run.ty == crate::material::MaterialTypeId::GLTF
            && info.lods[..levels].iter().any(|l| l.meshlet_count > 0))
        .then_some(info)
    };
    let mut wanted = [0u64; STATES];
    for (_, run, mesh) in runs.clone() {
        if run.state & BLENDED != 0 {
            continue;
        }
        let each = meshlets(run, mesh).map_or(1, |info| {
            info.lods[..run.lods as usize]
                .iter()
                .map(|l| l.meshlet_count)
                .max()
                .unwrap_or(1)
        });
        wanted[run.state] += u64::from(run.len) * u64::from(each) * VSM_LEVELS_PER_INSTANCE;
    }
    let total: u64 = wanted.iter().sum();
    if total == 0 {
        return None;
    }
    let mut caps = [0u32; STATES];
    let mut offsets = [0u32; STATES];
    let mut draws = 0u32;
    for state in 0..STATES {
        caps[state] = if total > MAX_VSM_DRAWS {
            (wanted[state] * MAX_VSM_DRAWS / total) as u32
        } else {
            wanted[state] as u32
        };
        offsets[state] = draws;
        draws += caps[state];
    }
    let table = {
        let (out, staged) = ring.alloc::<BatchMeshletData>(len.max(1))?;
        for out in out.iter_mut() {
            out.write(BatchMeshletData::default());
        }
        for (index, run, mesh) in runs {
            let mut entry = BatchMeshletData {
                state: run.state as u32,
                cone: u32::from(run.state & DOUBLE_SIDED == 0),
                ..BatchMeshletData::default()
            };
            if let Some(info) = meshlets(run, mesh) {
                for (k, level) in info.lods[..run.lods as usize].iter().enumerate() {
                    // The pool's meshlets are 8 words, 32-byte aligned.
                    entry.first[k] = level.first_meshlet / 8;
                    entry.count[k] = level.meshlet_count;
                }
            }
            if let Some(slot) = out.get_mut(index) {
                slot.write(entry);
            }
        }
        staged.address
    };
    // Each instance may take the meshlet path once per level: a chunk more each time
    // than its draws fill.
    let work = instances * VSM_LEVELS + draws.div_ceil(MESHLET_CHUNK);
    let mut header = [0u32; (MESHLET_HEADER_BYTES / 4) as usize];
    header[64..96].copy_from_slice(&offsets);
    header[96..128].copy_from_slice(&caps);
    header[128..132].copy_from_slice(&[0, 1, 1, work]);
    let header = ring.stage(&header)?;
    Some(MeshletLayout {
        table,
        header,
        offsets,
        caps,
        draws,
        work,
    })
}

/// Where a frame's instance changes come from, for the local shadows' cache.
#[derive(Clone, Copy)]
pub(super) enum Changes<'a, 'g> {
    Draws(&'a [DrawItem]),
    Scene(&'a SceneFrame<'g>),
}

/// Where the instances come from.
#[derive(Debug, Clone, Copy)]
pub(super) enum Source {
    /// Written into the ring this frame, in draw order (the draw-list path).
    Ring(crate::upload::Staged),
    /// A persistent scene's buffers.
    Scene {
        current: Buffer,
        previous: Buffer,
        previous_address: vk::DeviceAddress,
    },
}

/// One frame's instances, batches and draw commands.
#[derive(Debug, Clone, Copy)]
pub(super) struct Layout {
    /// Instances the culling pass visits (one thread each).
    pub(super) instance_count: u32,
    pub(super) instances: vk::DeviceAddress,
    pub(super) source: Source,
    pub(super) batch_table: vk::DeviceAddress,
    /// The command lists, in the ring.
    pub(super) commands: crate::upload::Staged,
    /// Commands per list.
    pub(super) command_count: u32,
    /// Commands that draw (the scene path's sink follows them).
    pub(super) drawn_commands: u32,
    /// Visible-list slots per list.
    pub(super) slot_count: u32,
    pub(super) batch_count: u32,
    pub(super) states: States,
    /// World-space bounds of the instances (the shadow cascades' fit).
    pub(super) min: Vec3,
    pub(super) max: Vec3,
    pub(super) subsurface_draws: bool,
    /// The meshlet lists, when the view draws by meshlet (ADR 0085).
    pub(super) meshlets: Option<MeshletLayout>,
    /// The virtual shadow pages' lists, when the view has them (ADR 0093 step 3b): their
    /// own batch table in `table`.
    pub(super) vsm: Option<MeshletLayout>,
}

/// Whether a transform mirrors (negative determinant of its linear part).
pub(crate) fn mirrored(m: &Mat4) -> bool {
    m.x_axis
        .truncate()
        .cross(m.y_axis.truncate())
        .dot(m.z_axis.truncate())
        < 0.0
}

/// A batch's pipeline state; `subsurface`: whether subsurface materials get theirs.
pub(crate) fn state(kind: MaterialKind, mirrored: bool, subsurface: bool) -> usize {
    // Masked and blended exclude each other (both come from the alpha mode), blended and
    // subsurface too (the material table never sets both).
    (usize::from(kind.subsurface && subsurface) * SUBSURFACE)
        | (usize::from(kind.blended) * BLENDED)
        | (usize::from(kind.masked) * MASKED)
        | (usize::from(kind.double_sided) * DOUBLE_SIDED)
        | (usize::from(mirrored) * MIRRORED)
}

/// Fills `slots` with every draw's material slot and `runs` with the runs of consecutive
/// draws that share a mesh and a pipeline state; counts the runs per state. `subsurface`:
/// whether subsurface materials get their own states.
pub(super) fn batches(
    draws: &[DrawItem],
    materials: &MaterialFrame<'_>,
    slots: &mut Vec<u32>,
    runs: &mut Vec<Run>,
    counts: &mut [u32; STATES],
    subsurface: bool,
) {
    slots.clear();
    runs.clear();
    let mut mesh = None;
    for (i, draw) in draws.iter().enumerate() {
        let (slot, kind) = materials.resolve(draw.material);
        let state = state(kind, mirrored(&draw.world_from_object), subsurface);
        slots.push(slot);
        match runs.last_mut() {
            Some(run) if run.state == state && run.ty == kind.ty && mesh == Some(draw.mesh) => {
                run.len += 1;
            }
            _ => {
                runs.push(Run {
                    first: i as u32,
                    len: 1,
                    state,
                    ty: kind.ty,
                    order: 0,
                    command: 0,
                    slot: 0,
                    lods: 1,
                });
                counts[state] += 1;
                // A blended draw is a batch of its own (no mesh to continue): one command
                // per instance keeps them in back-to-front order through culling.
                mesh = (state & BLENDED == 0).then_some(draw.mesh);
            }
        }
    }
}

/// The most runs of commands of material types other than glTF's a frame's layout keeps
/// apart (ADR 0092); further ones draw with the built-in pipelines, as their fallback.
pub(crate) const MAX_TYPE_RANGES: usize = 128;

/// Consecutive commands of one pipeline state and one material type other than glTF's.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct TypeRange {
    pub(crate) ty: crate::material::MaterialTypeId,
    pub(crate) state: u8,
    pub(crate) start: u32,
    pub(crate) count: u32,
}

/// The typed ranges of a layout, in command order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TypedRanges {
    pub(crate) ranges: [TypeRange; MAX_TYPE_RANGES],
    pub(crate) len: u32,
}

impl TypedRanges {
    pub(crate) fn as_slice(&self) -> &[TypeRange] {
        &self.ranges[..self.len as usize]
    }
}

/// The sort key of a run: its pipeline state, then (opaque and masked states) its material
/// type, so a type's batches of a state are consecutive commands, then its distance (the
/// argument: front to back, or `!distance` for blended runs, back to front across types).
fn sort_key(run: &Run, distance: u32) -> u64 {
    let ty = if run.state & BLENDED == 0 {
        u64::from(run.ty.index())
    } else {
        0
    };
    ((run.state as u64) << 48) | (ty << 32) | u64::from(distance)
}

/// The runs of consecutive commands of material types other than glTF's, from the runs in
/// command order (`keys`); at most [`MAX_TYPE_RANGES`].
fn typed_ranges(keys: &[(u64, u32)], runs: &[Run]) -> TypedRanges {
    let mut typed = TypedRanges {
        ranges: [TypeRange::default(); MAX_TYPE_RANGES],
        len: 0,
    };
    for &(_, k) in keys {
        let run = &runs[k as usize];
        if run.ty == crate::material::MaterialTypeId::GLTF {
            continue;
        }
        let n = typed.len as usize;
        if n > 0 {
            let last = &mut typed.ranges[n - 1];
            if last.ty == run.ty
                && usize::from(last.state) == run.state
                && last.start + last.count == run.command
            {
                last.count += run.lods;
                continue;
            }
        }
        if n == MAX_TYPE_RANGES {
            continue;
        }
        typed.ranges[n] = TypeRange {
            ty: run.ty,
            state: run.state as u8,
            start: run.command,
            count: run.lods,
        };
        typed.len += 1;
    }
    typed
}

/// Each typed range's pipelines for one pass (`[opaque, masked]`): its type's, or `None`
/// for the built-in ones (the fallback, until the type's are made).
#[derive(Clone, Copy)]
pub(crate) struct TypedPipelines<'a> {
    pub(crate) ranges: [Option<[&'a Pipeline; 2]>; MAX_TYPE_RANGES],
}

impl TypedPipelines<'_> {
    /// Every range with the built-in pipelines.
    pub(crate) const BUILTIN: Self = Self {
        ranges: [None; MAX_TYPE_RANGES],
    };
}

/// Where each pipeline state's batches are among the commands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct States {
    pub(super) starts: [u32; STATES],
    pub(super) counts: [u32; STATES],
    /// The commands of material types other than glTF's (ADR 0092).
    pub(super) typed: TypedRanges,
}

impl States {
    /// Records one indirect draw per pipeline state in use, from the commands at `offset`
    /// in `buffer` (one per batch, in state order), into `area` of the target (a rectangle,
    /// or a viewport with a scissor of its own): `pipelines`
    /// are the opaque and the masked one. Only the states whose bits `mask` sets are drawn
    /// (`*_STATES`). The shadow pass renders back faces of single-sided
    /// geometry (fewer self-shadowing artifacts on closed meshes), except for subsurface
    /// materials (their front faces, where light enters, for thick transmission); the
    /// forward passes cull them.
    ///
    /// # Safety
    ///
    /// `ctx` records inside a rendering scope of a pass that reads the geometry pool and
    /// the material table; `buffer` holds the commands and has indirect-buffer usage.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn draw(
        &self,
        ctx: &crate::graph::PassContext<'_>,
        bindless: &Bindless,
        geometry: &GeometryFrame<'_>,
        pipelines: [&Pipeline; 2],
        typed: &TypedPipelines<'_>,
        push: &Push,
        shadow: bool,
        mask: u32,
        buffer: vk::Buffer,
        offset: u64,
        area: impl Into<super::DrawArea>,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.draw_slice(
                ctx,
                bindless,
                geometry,
                pipelines,
                typed,
                push,
                shadow,
                mask,
                buffer,
                offset,
                area,
                (0, 1),
            );
        }
    }

    /// [`Self::draw`] for one slice of the commands: with `(k, n)`, every `n`-th command of
    /// each state from its `k`-th (texture feedback spreads its draws over `n` frames).
    ///
    /// # Safety
    ///
    /// As [`Self::draw`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn draw_slice(
        &self,
        ctx: &crate::graph::PassContext<'_>,
        bindless: &Bindless,
        geometry: &GeometryFrame<'_>,
        pipelines: [&Pipeline; 2],
        typed: &TypedPipelines<'_>,
        push: &Push,
        shadow: bool,
        mask: u32,
        buffer: vk::Buffer,
        offset: u64,
        area: impl Into<super::DrawArea>,
        (slice, slices): (u32, u32),
    ) {
        let area = area.into();
        let slices = slices.max(1);
        let mut bound: Option<vk::Pipeline> = None;
        let ranges = self.typed.as_slice();
        let mut next_range = 0;
        for state in 0..STATES {
            let count = self.counts[state].saturating_sub(slice).div_ceil(slices);
            if count == 0 || mask & (1 << state) == 0 {
                // Skip its typed ranges.
                while next_range < ranges.len() && usize::from(ranges[next_range].state) == state {
                    next_range += 1;
                }
                continue;
            }
            let masked = state & MASKED != 0;
            let (cull, front) = faces(state, shadow);
            let start = self.starts[state];
            let end = start + self.counts[state];
            // The commands from `a` to `b` of this state with `pipeline`: its every
            // `slices`-th from its `slice`-th (counted from the state's first).
            let mut draw = |a: u32, b: u32, pipeline: &Pipeline| {
                let first = a + (slice + slices - (a - start) % slices) % slices;
                if first >= b {
                    return;
                }
                let count = (b - first).div_ceil(slices);
                // SAFETY: guaranteed by the caller.
                unsafe {
                    if bound != Some(pipeline.raw()) {
                        pipeline.bind(ctx.cmd);
                        if bound.is_none() {
                            bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                            super::set_draw_area(ctx.device, ctx.cmd, area);
                            bindless.push_constants(ctx.cmd, push_bytes(push));
                            geometry.bind_indices(ctx);
                        }
                        bound = Some(pipeline.raw());
                    }
                    ctx.device.cmd_set_cull_mode(ctx.cmd, cull);
                    ctx.device.cmd_set_front_face(ctx.cmd, front);
                    ctx.device.cmd_draw_indexed_indirect(
                        ctx.cmd,
                        buffer,
                        offset + u64::from(first) * COMMAND_BYTES,
                        count,
                        COMMAND_BYTES as u32 * slices,
                    );
                }
            };
            let builtin = pipelines[usize::from(masked)];
            let mut cursor = start;
            while next_range < ranges.len() && usize::from(ranges[next_range].state) == state {
                let range = ranges[next_range];
                let own = typed.ranges[next_range].map_or(builtin, |p| p[usize::from(masked)]);
                draw(cursor, range.start, builtin);
                draw(range.start, range.start + range.count, own);
                cursor = range.start + range.count;
                next_range += 1;
            }
            draw(cursor, end, builtin);
        }
    }

    /// Records each pipeline state's meshlet draws (ADR 0085) from one phase's (or shadow
    /// cascade's: `shadow`) lists at `lists` in `buffer`, as many as its meshlet cull wrote
    /// (their count is in the lists' header), with the built-in `pipelines` (opaque,
    /// masked), for the states `mask` sets.
    ///
    /// # Safety
    ///
    /// As [`Self::draw`]; `buffer` holds the lists and has indirect-buffer usage.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn draw_meshlets(
        &self,
        ctx: &crate::graph::PassContext<'_>,
        bindless: &Bindless,
        geometry: &GeometryFrame<'_>,
        pipelines: [&Pipeline; 2],
        push: &Push,
        shadow: bool,
        mask: u32,
        buffer: vk::Buffer,
        lists: u64,
        meshlets: &super::MeshletDraws,
        area: impl Into<super::DrawAreas>,
    ) {
        let area = area.into();
        let layout = &meshlets.layout;
        let draws = lists + layout.draws_offset();
        let mut bound: Option<vk::Pipeline> = None;
        for state in 0..STATES {
            let room = layout.caps[state];
            if room == 0 || mask & (1 << state) == 0 {
                continue;
            }
            let pipeline = pipelines[usize::from(state & MASKED != 0)];
            let (cull, front) = faces(state, shadow);
            // SAFETY: guaranteed by the caller; the count is at the state's word of the
            // header and never above `room` (the cull reserves before it writes).
            unsafe {
                if bound != Some(pipeline.raw()) {
                    pipeline.bind(ctx.cmd);
                    if bound.is_none() {
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        if let super::DrawAreas::One(area) = area {
                            super::set_draw_area(ctx.device, ctx.cmd, area);
                        }
                        bindless.push_constants(ctx.cmd, push_bytes(push));
                        geometry.bind_indices(ctx);
                    }
                    bound = Some(pipeline.raw());
                }
                ctx.device.cmd_set_cull_mode(ctx.cmd, cull);
                ctx.device.cmd_set_front_face(ctx.cmd, front);
                ctx.device.cmd_draw_indexed_indirect_count(
                    ctx.cmd,
                    buffer,
                    draws + u64::from(layout.offsets[state]) * COMMAND_BYTES,
                    buffer,
                    lists + state as u64 * 4,
                    room,
                    COMMAND_BYTES as u32,
                );
            }
        }
    }
}

/// The faces a pipeline state culls and its front face. The shadow passes render back
/// faces of single-sided geometry (fewer self-shadowing artifacts on closed meshes),
/// except for subsurface materials (their front faces, where light enters, for thick
/// transmission); the forward passes cull them.
fn faces(state: usize, shadow: bool) -> (vk::CullModeFlags, vk::FrontFace) {
    let cull = match (state & DOUBLE_SIDED != 0, shadow) {
        (true, _) => vk::CullModeFlags::NONE,
        // Subsurface materials cast from the faces towards the light: the shadow map then
        // holds where light enters them, for thick transmission.
        (false, true) if state & SUBSURFACE != 0 => vk::CullModeFlags::BACK,
        (false, true) => vk::CullModeFlags::FRONT,
        (false, false) => vk::CullModeFlags::BACK,
    };
    let front = if state & MIRRORED != 0 {
        vk::FrontFace::CLOCKWISE
    } else {
        vk::FrontFace::COUNTER_CLOCKWISE
    };
    (cull, front)
}

/// A mesh's object-space bounding box as center and half extents (empty for removed
/// meshes).
pub(super) fn local_box(geometry: &GeometryFrame<'_>, mesh: MeshId) -> (Vec3, Vec3) {
    geometry.mesh(mesh).map_or((Vec3::ZERO, Vec3::ZERO), |m| {
        ((m.min + m.max) * 0.5, (m.max - m.min) * 0.5)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::material::MaterialTypeId;

    fn run(state: usize, ty: u16, command: u32, lods: u32) -> Run {
        Run {
            first: 0,
            len: 1,
            state,
            ty: MaterialTypeId(ty),
            order: 0,
            command,
            slot: 0,
            lods,
        }
    }

    /// Opaque states order their runs by type, then distance; blended ones by distance
    /// only (back to front across types).
    #[test]
    fn types_sort_within_opaque_states_only() {
        let near_typed = sort_key(&run(0, 1, 0, 1), 1);
        let far_gltf = sort_key(&run(0, 0, 0, 1), 9);
        assert!(far_gltf < near_typed, "glTF first within a state");
        let blended_near_typed = sort_key(&run(BLENDED, 1, 0, 1), !1);
        let blended_far_gltf = sort_key(&run(BLENDED, 0, 0, 1), !9);
        assert!(
            blended_far_gltf < blended_near_typed,
            "farther first, whatever the type"
        );
        assert!(sort_key(&run(1, 0, 0, 1), 0) > sort_key(&run(0, 7, 0, 1), u32::MAX));
    }

    /// Consecutive commands of one type and state make one range; glTF runs make none.
    #[test]
    fn typed_ranges_merge_consecutive_commands() {
        let runs = [
            run(0, 0, 0, 2),
            run(0, 1, 2, 3),
            run(0, 1, 5, 1),
            run(0, 2, 6, 1),
            run(BLENDED, 1, 7, 1),
            run(BLENDED, 0, 8, 1),
            run(BLENDED, 1, 9, 1),
        ];
        let keys: Vec<(u64, u32)> = (0..runs.len() as u32).map(|k| (0, k)).collect();
        let typed = typed_ranges(&keys, &runs);
        let got: Vec<(u16, u8, u32, u32)> = typed
            .as_slice()
            .iter()
            .map(|r| (r.ty.0, r.state, r.start, r.count))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, 0, 2, 4),
                (2, 0, 6, 1),
                (1, BLENDED as u8, 7, 1),
                (1, BLENDED as u8, 9, 1),
            ]
        );
    }
}
