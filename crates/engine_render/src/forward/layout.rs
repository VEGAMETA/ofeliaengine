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
        let subsurface_on = self.subsurface_states().is_some();
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
            (((run.state as u64) << 32) | u64::from(distance), k as u32)
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
            states: States { starts, counts },
            min,
            max,
            subsurface_draws,
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
        let subsurface_on = self.subsurface_states().is_some();
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
            (((run.state as u64) << 32) | u64::from(distance), k as u32)
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
            states: States { starts, counts },
            min,
            max,
            subsurface_draws,
        })
    }
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
            Some(run) if run.state == state && mesh == Some(draw.mesh) => run.len += 1,
            _ => {
                runs.push(Run {
                    first: i as u32,
                    len: 1,
                    state,
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

/// Where each pipeline state's batches are among the commands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct States {
    pub(super) starts: [u32; STATES],
    pub(super) counts: [u32; STATES],
}

impl States {
    /// Records one indirect draw per pipeline state in use, from the commands at `offset`
    /// in `buffer` (one per batch, in state order), into `area` of the target: `pipelines`
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
        push: &Push,
        shadow: bool,
        mask: u32,
        buffer: vk::Buffer,
        offset: u64,
        area: vk::Rect2D,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.draw_slice(
                ctx,
                bindless,
                geometry,
                pipelines,
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
        push: &Push,
        shadow: bool,
        mask: u32,
        buffer: vk::Buffer,
        offset: u64,
        area: vk::Rect2D,
        (slice, slices): (u32, u32),
    ) {
        let slices = slices.max(1);
        let mut bound = None;
        for state in 0..STATES {
            let count = self.counts[state].saturating_sub(slice).div_ceil(slices);
            if count == 0 || mask & (1 << state) == 0 {
                continue;
            }
            let masked = state & MASKED != 0;
            let cull = match (state & DOUBLE_SIDED != 0, shadow) {
                (true, _) => vk::CullModeFlags::NONE,
                // Subsurface materials cast from the faces towards the light: the shadow
                // map then holds where light enters them, for thick transmission.
                (false, true) if state & SUBSURFACE != 0 => vk::CullModeFlags::BACK,
                (false, true) => vk::CullModeFlags::FRONT,
                (false, false) => vk::CullModeFlags::BACK,
            };
            let front = if state & MIRRORED != 0 {
                vk::FrontFace::CLOCKWISE
            } else {
                vk::FrontFace::COUNTER_CLOCKWISE
            };
            // SAFETY: guaranteed by the caller.
            unsafe {
                if bound != Some(masked) {
                    pipelines[usize::from(masked)].bind(ctx.cmd);
                    if bound.is_none() {
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        set_viewport(ctx.device, ctx.cmd, area);
                        bindless.push_constants(ctx.cmd, push_bytes(push));
                        geometry.bind_indices(ctx);
                    }
                    bound = Some(masked);
                }
                ctx.device.cmd_set_cull_mode(ctx.cmd, cull);
                ctx.device.cmd_set_front_face(ctx.cmd, front);
                ctx.device.cmd_draw_indexed_indirect(
                    ctx.cmd,
                    buffer,
                    offset + u64::from(self.starts[state] + slice) * COMMAND_BYTES,
                    count,
                    COMMAND_BYTES as u32 * slices,
                );
            }
        }
    }
}

/// A mesh's object-space bounding box as center and half extents (empty for removed
/// meshes).
pub(super) fn local_box(geometry: &GeometryFrame<'_>, mesh: MeshId) -> (Vec3, Vec3) {
    geometry.mesh(mesh).map_or((Vec3::ZERO, Vec3::ZERO), |m| {
        ((m.min + m.max) * 0.5, (m.max - m.min) * 0.5)
    })
}
