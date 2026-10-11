//! Async compute scheduling (ADR 0065): which queue each needed pass runs on, in which
//! order, and how the frame splits into segments (batches) joined by timeline semaphores.
//!
//! - Passes declared with `.async_compute()` run on the async compute queue when the
//!   graph has one and the pass can (no attachments, only compute and transfer stages).
//! - The order is a topological order of the passes' dependencies (read after write,
//!   write after read, write after write; a layout change counts as a write; resources
//!   owned by one queue family at a time also serialize their accesses across queues; a
//!   fence follows every pass declared before it and precedes every pass declared after
//!   it) that prefers the ancestors of earlier async passes: whatever an async pass needs is
//!   scheduled first, so the graphics work that does not lead to it runs while it does.
//!   Frostbite's frame graph and Granite schedule the same way: async work as early as
//!   its inputs allow.
//! - A segment ends where the other queue needs its results, and one starts where a
//!   pass needs the other queue's: its batch waits for that segment's signal. A compute
//!   segment always waits for the graphics work before it (in the order), which also
//!   orders it after the previous frames' graphics work and keeps transient memory
//!   reuse safe. The last graphics segment waits for every compute segment.
//!
//! Without async passes (or with async compute off) the order is the declaration order
//! and the frame is one graphics segment, exactly as before async compute.

use super::compile::{Kind, NO_SEGMENT, QueueKind, Records, Segment, UNUSED};
use engine_rhi::ash::vk;

/// Stages a compute-only queue supports, for the accesses of async passes.
const COMPUTE_QUEUE_STAGES: vk::PipelineStageFlags2 = vk::PipelineStageFlags2::from_raw(
    vk::PipelineStageFlags2::COMPUTE_SHADER.as_raw()
        | vk::PipelineStageFlags2::ALL_TRANSFER.as_raw()
        | vk::PipelineStageFlags2::COPY.as_raw()
        | vk::PipelineStageFlags2::CLEAR.as_raw()
        | vk::PipelineStageFlags2::DRAW_INDIRECT.as_raw(),
);

/// How the frame may be scheduled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ScheduleOptions {
    /// Async passes may run on the async compute queue.
    pub(crate) async_compute: bool,
    /// Transient resources used on both queues are created with concurrent sharing
    /// instead of moving between queue families with ownership transfers.
    pub(crate) concurrent: bool,
}

/// Bookkeeping reused across frames.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    queues_used: Vec<u8>,
    last_writer: Vec<u32>,
    last_access: Vec<u32>,
    reader_head: Vec<u32>,
    reader_nodes: Vec<(u32, u32)>,
    layout: Vec<Option<vk::ImageLayout>>,
    group: Vec<u32>,
    scheduled: Vec<bool>,
    counts: Vec<u32>,
}

impl Records {
    /// Decides where and in which order the needed passes run (after culling). Returns
    /// whether the frame uses the async compute queue.
    pub(crate) fn schedule(&mut self, options: ScheduleOptions) -> bool {
        if options.async_compute && self.schedule_async(options.concurrent) {
            return true;
        }
        self.schedule_single();
        false
    }

    /// Everything on the graphics queue in declaration order, one segment.
    fn schedule_single(&mut self) {
        self.order.clear();
        for (index, pass) in self.passes.iter_mut().enumerate() {
            pass.queue = QueueKind::Graphics;
            pass.segment = 0;
            pass.preds = 0..0;
            if pass.needed {
                self.order.push(index as u32);
            }
        }
        for r in &mut self.resources {
            r.concurrent = false;
        }
        self.segments.clear();
        let mut segment = Segment::new(QueueKind::Graphics, NO_SEGMENT, 0);
        segment.passes = 0..self.order.len() as u32;
        self.segments.push(segment);
        self.segment_passes.clear();
        self.segment_passes.extend_from_slice(&self.order);
    }

    #[allow(clippy::too_many_lines)]
    fn schedule_async(&mut self, concurrent: bool) -> bool {
        let Self {
            resources,
            passes,
            accesses,
            order,
            segments,
            segment_passes,
            edges,
            schedule_scratch: sc,
            ..
        } = self;

        // Which passes run on the compute queue.
        let mut any = false;
        for pass in passes.iter_mut() {
            pass.queue = QueueKind::Graphics;
            pass.segment = 0;
            let range = pass.accesses.start as usize..pass.accesses.end as usize;
            if pass.needed
                && pass.async_requested
                && pass.attachments.is_empty()
                && accesses[range]
                    .iter()
                    .all(|a| COMPUTE_QUEUE_STAGES.contains(a.info.stages))
            {
                pass.queue = QueueKind::AsyncCompute;
                any = true;
            }
        }
        if !any {
            return false;
        }

        // Resources both queues use: concurrent sharing for transients if chosen.
        let count = resources.len();
        sc.queues_used.clear();
        sc.queues_used.resize(count, 0);
        for pass in passes.iter().filter(|p| p.needed) {
            for a in &accesses[pass.accesses.start as usize..pass.accesses.end as usize] {
                sc.queues_used[a.resource as usize] |= 1 << pass.queue.index();
            }
        }
        for (r, resource) in resources.iter_mut().enumerate() {
            resource.concurrent = concurrent && sc.queues_used[r] == 3 && resource.is_transient();
        }

        // Dependencies, from the declaration order.
        edges.clear();
        sc.last_writer.clear();
        sc.last_writer.resize(count, UNUSED);
        sc.last_access.clear();
        sc.last_access.resize(count, UNUSED);
        sc.reader_head.clear();
        sc.reader_head.resize(count, UNUSED);
        sc.reader_nodes.clear();
        sc.layout.clear();
        sc.layout
            .extend(resources.iter().map(|r| r.initial.map(|b| b.layout)));
        // A pass that declares no accesses may depend on anything: keep the declared
        // order then.
        let mut reorder = true;
        // The latest fence, and the first pass declared after it.
        let mut fence = UNUSED;
        let mut since_fence = 0;
        for index in 0..passes.len() {
            let start = edges.len() as u32;
            let pass = &passes[index];
            if pass.needed {
                if fence != UNUSED {
                    edges.push(fence);
                }
                if pass.fence {
                    edges.extend(
                        (since_fence..index)
                            .filter(|&p| passes[p].needed)
                            .map(|p| p as u32),
                    );
                    fence = index as u32;
                    since_fence = index + 1;
                }
                let index = index as u32;
                let range = pass.accesses.start as usize..pass.accesses.end as usize;
                reorder &= !range.is_empty();
                for a in &accesses[range] {
                    let r = a.resource as usize;
                    let image = resources[r].kind == Kind::Texture;
                    let layout_change =
                        image && sc.layout[r].is_some_and(|layout| layout != a.info.layout);
                    let writes = a.info.writes || layout_change;
                    // One queue family owns it at a time: accesses on both queues are
                    // ordered by ownership transfers.
                    if sc.queues_used[r] == 3
                        && !resources[r].concurrent
                        && sc.last_access[r] != UNUSED
                    {
                        edges.push(sc.last_access[r]);
                    }
                    if sc.last_writer[r] != UNUSED {
                        edges.push(sc.last_writer[r]);
                    }
                    if writes {
                        let mut node = sc.reader_head[r];
                        while node != UNUSED {
                            let (reader, next) = sc.reader_nodes[node as usize];
                            edges.push(reader);
                            node = next;
                        }
                        sc.reader_head[r] = UNUSED;
                        sc.last_writer[r] = index;
                    } else {
                        sc.reader_nodes.push((index, sc.reader_head[r]));
                        sc.reader_head[r] = (sc.reader_nodes.len() - 1) as u32;
                    }
                    if image {
                        sc.layout[r] = Some(a.info.layout);
                    }
                    sc.last_access[r] = index;
                }
            }
            passes[index].preds = start..edges.len() as u32;
        }

        // The order: ancestors of earlier async passes first, otherwise as declared.
        order.clear();
        let n = passes.len();
        if reorder {
            sc.group.clear();
            sc.group.extend(passes.iter().enumerate().map(|(i, p)| {
                if p.needed && p.queue == QueueKind::AsyncCompute {
                    i as u32
                } else {
                    UNUSED
                }
            }));
            for i in (0..n).rev() {
                if !passes[i].needed {
                    continue;
                }
                let group = sc.group[i];
                for &e in &edges[passes[i].preds.start as usize..passes[i].preds.end as usize] {
                    let e = e as usize;
                    sc.group[e] = sc.group[e].min(group);
                }
            }
            sc.scheduled.clear();
            sc.scheduled.resize(n, false);
            let needed = passes.iter().filter(|p| p.needed).count();
            for _ in 0..needed {
                let mut best: Option<(u32, u32)> = None;
                for (i, pass) in passes.iter().enumerate() {
                    if !pass.needed || sc.scheduled[i] {
                        continue;
                    }
                    let ready = edges[pass.preds.start as usize..pass.preds.end as usize]
                        .iter()
                        .all(|&e| sc.scheduled[e as usize]);
                    let key = (sc.group[i], i as u32);
                    if ready && best.is_none_or(|b| key < b) {
                        best = Some(key);
                    }
                }
                // The declaration order is a valid order, so some pass is always ready.
                let Some((_, i)) = best else { break };
                sc.scheduled[i as usize] = true;
                order.push(i);
            }
        } else {
            order.extend(
                passes
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.needed)
                    .map(|(i, _)| i as u32),
            );
        }

        // Segments. `open` is the segment each queue is filling, `waited` the latest
        // segment of the other queue each queue has waited for.
        const G: usize = 0;
        const C: usize = 1;
        segments.clear();
        segments.push(Segment::new(QueueKind::Graphics, NO_SEGMENT, 0));
        sc.counts.clear();
        sc.counts.push(0);
        let mut open = [0, NO_SEGMENT];
        let mut latest = [0, NO_SEGMENT];
        let mut waited = [NO_SEGMENT; 2];
        let newer = |a: u32, b: u32| {
            if a == NO_SEGMENT || (b != NO_SEGMENT && b > a) {
                b
            } else {
                a
            }
        };
        for (position, &p) in order.iter().enumerate() {
            let position = position as u32;
            let pass = &passes[p as usize];
            let queue = pass.queue;
            let (q, o) = (queue.index(), queue.other().index());
            let mut need = NO_SEGMENT;
            for &e in &edges[pass.preds.start as usize..pass.preds.end as usize] {
                let e = &passes[e as usize];
                if e.queue != queue {
                    need = newer(need, e.segment);
                }
            }
            if q == C {
                need = newer(need, latest[G]);
            }
            if need != NO_SEGMENT && newer(waited[q], need) != waited[q] {
                // The segment waited for ends here (any older one has ended already) ...
                if open[o] == need {
                    open[o] = NO_SEGMENT;
                }
                // ... and this queue's next batch waits for it.
                if open[q] != NO_SEGMENT && sc.counts[open[q] as usize] > 0 {
                    open[q] = NO_SEGMENT;
                }
                if open[q] == NO_SEGMENT {
                    segments.push(Segment::new(queue, need, position));
                    sc.counts.push(0);
                    open[q] = (segments.len() - 1) as u32;
                    latest[q] = open[q];
                } else {
                    segments[open[q] as usize].wait = need;
                }
                waited[q] = need;
                if q == G {
                    for (s, segment) in segments.iter_mut().enumerate() {
                        if segment.queue == QueueKind::AsyncCompute
                            && segment.join_position == UNUSED
                            && s as u32 <= need
                        {
                            segment.join_position = position;
                        }
                    }
                }
            }
            if open[q] == NO_SEGMENT {
                segments.push(Segment::new(queue, NO_SEGMENT, position));
                sc.counts.push(0);
                open[q] = (segments.len() - 1) as u32;
                latest[q] = open[q];
            }
            passes[p as usize].segment = open[q];
            sc.counts[open[q] as usize] += 1;
        }
        // The last graphics segment (the caller's command buffer) waits for all compute
        // work: the frame's completion then covers both queues.
        let end = order.len() as u32;
        if latest[C] != NO_SEGMENT && newer(waited[G], latest[C]) != waited[G] {
            if open[G] != NO_SEGMENT && sc.counts[open[G] as usize] > 0 {
                open[G] = NO_SEGMENT;
            }
            if open[G] == NO_SEGMENT {
                segments.push(Segment::new(QueueKind::Graphics, latest[C], end));
                sc.counts.push(0);
            } else {
                segments[open[G] as usize].wait = latest[C];
            }
        }
        for segment in segments.iter_mut() {
            if segment.queue == QueueKind::AsyncCompute && segment.join_position == UNUSED {
                segment.join_position = end;
            }
        }

        // Each segment's passes, in order.
        let mut next = 0;
        for (segment, count) in segments.iter_mut().zip(sc.counts.iter_mut()) {
            segment.passes = next..next + *count;
            next += *count;
            *count = segment.passes.start;
        }
        segment_passes.clear();
        segment_passes.resize(order.len(), 0);
        for &p in order.iter() {
            let s = passes[p as usize].segment as usize;
            segment_passes[sc.counts[s] as usize] = p;
            sc.counts[s] += 1;
        }

        // An acquired swapchain image may only be used by the caller's command buffer,
        // whose submission waits for it: otherwise this frame runs on one queue.
        let last_graphics = segments
            .iter()
            .rposition(|s| s.queue == QueueKind::Graphics)
            .unwrap_or(0) as u32;
        for &p in order.iter() {
            let pass = &passes[p as usize];
            if pass.segment != last_graphics
                && accesses[pass.accesses.start as usize..pass.accesses.end as usize]
                    .iter()
                    .any(|a| resources[a.resource as usize].caller_waits)
            {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Access;
    use crate::graph::access::{AccessInfo, ShaderStages, TextureState};
    use crate::graph::compile::{ImageBarrier, NO_SCOPE, PassRecord, ResourceRecord};

    fn texture(rec: &mut Records, name: &'static str) -> u32 {
        rec.resources.push(ResourceRecord::new(name, Kind::Texture));
        (rec.resources.len() - 1) as u32
    }

    fn pass(rec: &mut Records, name: &'static str, accesses: &[(u32, AccessInfo)]) -> usize {
        let start = rec.accesses.len() as u32;
        rec.passes.push(PassRecord::new(name, NO_SCOPE, start, 0));
        for &(r, info) in accesses {
            rec.add_access(r, info);
        }
        rec.passes.len() - 1
    }

    fn compute(access: Access) -> AccessInfo {
        AccessInfo::of(access)
    }

    /// A forward frame with ambient occlusion: shadows declared before the depth prepass,
    /// AO (async) needs the prepass, the shading needs both.
    struct Frame {
        rec: Records,
        shadow: usize,
        prepass: usize,
        ao: usize,
        blur: usize,
        forward: usize,
        post: usize,
    }

    fn frame() -> Frame {
        frame_after(|_| {})
    }

    /// [`frame`] after the passes `first` declares.
    fn frame_after(first: impl FnOnce(&mut Records)) -> Frame {
        let mut rec = Records::default();
        first(&mut rec);
        let shadow_map = texture(&mut rec, "shadow map");
        let depth = texture(&mut rec, "depth");
        let ao = texture(&mut rec, "ao");
        let blurred = texture(&mut rec, "ao blurred");
        let color = texture(&mut rec, "color");
        let out = texture(&mut rec, "out");
        rec.resources[out as usize].initial = Some(TextureState::undefined().into());
        rec.resources[out as usize].final_state =
            Some(TextureState::shader_read(ShaderStages::FRAGMENT).into());
        let shadow = pass(
            &mut rec,
            "shadow",
            &[(shadow_map, AccessInfo::depth_attachment(false, true))],
        );
        let prepass = pass(
            &mut rec,
            "prepass",
            &[(depth, AccessInfo::depth_attachment(false, true))],
        );
        let ao_pass = pass(
            &mut rec,
            "ao",
            &[
                (depth, compute(Access::Sampled(ShaderStages::COMPUTE))),
                (ao, compute(Access::StorageWrite(ShaderStages::COMPUTE))),
            ],
        );
        let blur = pass(
            &mut rec,
            "ao blur",
            &[
                (ao, compute(Access::Sampled(ShaderStages::COMPUTE))),
                (
                    blurred,
                    compute(Access::StorageWrite(ShaderStages::COMPUTE)),
                ),
            ],
        );
        let forward = pass(
            &mut rec,
            "forward",
            &[
                (shadow_map, compute(Access::Sampled(ShaderStages::FRAGMENT))),
                (blurred, compute(Access::Sampled(ShaderStages::FRAGMENT))),
                (depth, AccessInfo::depth_attachment(true, false)),
                (color, AccessInfo::color_attachment(false)),
            ],
        );
        let post = pass(
            &mut rec,
            "post",
            &[
                (color, compute(Access::Sampled(ShaderStages::FRAGMENT))),
                (out, AccessInfo::color_attachment(false)),
            ],
        );
        // Attachments make passes graphics-only: give the attachment passes a range.
        for p in [shadow, prepass, forward, post] {
            rec.passes[p].attachments = 0..1;
        }
        rec.passes[ao_pass].async_requested = true;
        rec.passes[blur].async_requested = true;
        Frame {
            rec,
            shadow,
            prepass,
            ao: ao_pass,
            blur,
            forward,
            post,
        }
    }

    fn compile(rec: &mut Records, options: ScheduleOptions) -> bool {
        rec.cull();
        let used = rec.schedule(options);
        rec.resolve_lifetimes();
        rec.build_barriers();
        used
    }

    const ASYNC: ScheduleOptions = ScheduleOptions {
        async_compute: true,
        concurrent: true,
    };

    #[test]
    fn without_async_compute_the_frame_is_one_graphics_segment_in_declared_order() {
        let mut f = frame();
        assert!(!compile(&mut f.rec, ScheduleOptions::default()));
        assert_eq!(f.rec.order, [0, 1, 2, 3, 4, 5]);
        assert_eq!(f.rec.segments.len(), 1);
        assert!(f.rec.passes.iter().all(|p| p.queue == QueueKind::Graphics));
    }

    #[test]
    fn async_passes_run_early_and_overlap_the_graphics_work_that_does_not_lead_to_them() {
        let mut f = frame();
        assert!(compile(&mut f.rec, ASYNC));
        let rec = &f.rec;
        // The prepass moves before the shadows: AO needs it, the shadows can overlap AO.
        let order: Vec<usize> = rec.order.iter().map(|&p| p as usize).collect();
        assert_eq!(
            order,
            [f.prepass, f.ao, f.blur, f.shadow, f.forward, f.post]
        );
        assert_eq!(rec.passes[f.ao].queue, QueueKind::AsyncCompute);
        assert_eq!(rec.passes[f.blur].queue, QueueKind::AsyncCompute);
        // Segments: [prepass] → compute [ao, blur] waits for it; [shadow] runs meanwhile;
        // [forward, post] waits for the compute segment.
        let seg = |p: usize| rec.passes[p].segment as usize;
        assert_eq!(seg(f.ao), seg(f.blur));
        let compute = &rec.segments[seg(f.ao)];
        assert_eq!(compute.wait as usize, seg(f.prepass));
        assert_eq!(rec.segments[seg(f.shadow)].wait, NO_SEGMENT);
        assert_ne!(seg(f.shadow), seg(f.prepass));
        assert_eq!(rec.segments[seg(f.forward)].wait as usize, seg(f.ao));
        assert_eq!(seg(f.forward), seg(f.post));
        // The last segment is graphics (the caller's command buffer).
        assert_eq!(
            rec.segments.last().map(|s| s.queue),
            Some(QueueKind::Graphics)
        );
        // The compute window is from the AO pass to the forward pass.
        assert_eq!(compute.first_position, 1);
        assert_eq!(compute.join_position, 4);
    }

    #[test]
    fn resources_of_async_passes_live_over_the_whole_overlap() {
        let mut f = frame();
        compile(&mut f.rec, ASYNC);
        // "ao" is used at positions 1 and 2 only, but the shadow pass at 3 may run while
        // the compute segment does: its memory must not be shared with the shadow map.
        let ao = &f.rec.resources[2];
        assert_eq!((ao.first, ao.last), (1, 3));
        let shadow_map = &f.rec.resources[0];
        assert!(shadow_map.first <= ao.last && ao.first <= shadow_map.last);
        // Transients on both queues are shared concurrently, the others are not.
        assert!(f.rec.resources[1].concurrent); // depth
        assert!(f.rec.resources[3].concurrent); // ao blurred
        assert!(!f.rec.resources[2].concurrent); // ao: compute only
        assert!(!f.rec.resources[0].concurrent); // shadow map: graphics only
    }

    fn releases(rec: &Records, segment: u32) -> &[ImageBarrier] {
        let r = &rec.segments[segment as usize].release_images;
        &rec.release_images[r.start as usize..r.end as usize]
    }

    #[test]
    fn concurrent_sharing_transitions_layouts_on_the_queue_that_used_them_last() {
        let mut f = frame();
        compile(&mut f.rec, ASYNC);
        let rec = &f.rec;
        // Depth: written as an attachment by the prepass, sampled by AO: the graphics
        // segment transitions it before signaling, the compute queue needs no barrier.
        let prepass_segment = rec.passes[f.prepass].segment;
        let depth = releases(rec, prepass_segment)
            .iter()
            .find(|b| b.resource == 1)
            .copied();
        let depth = depth.expect("depth transition at the end of the prepass segment");
        assert_eq!(depth.old_layout, vk::ImageLayout::ATTACHMENT_OPTIMAL);
        assert_eq!(depth.new_layout, vk::ImageLayout::READ_ONLY_OPTIMAL);
        assert!(depth.transfer.is_none());
        let ao_barriers = &rec.image_barriers[rec.passes[f.ao].image_barriers.start as usize
            ..rec.passes[f.ao].image_barriers.end as usize];
        assert!(ao_barriers.iter().all(|b| b.resource != 1));
        // Every barrier on the compute queue uses compute-queue stages only.
        for p in [f.ao, f.blur] {
            let range = &rec.passes[p].image_barriers;
            for b in &rec.image_barriers[range.start as usize..range.end as usize] {
                assert!(COMPUTE_QUEUE_STAGES.contains(b.src_stages), "{b:?}");
                assert!(COMPUTE_QUEUE_STAGES.contains(b.dst_stages), "{b:?}");
            }
        }
    }

    #[test]
    fn exclusive_sharing_moves_ownership_with_release_and_acquire() {
        let mut f = frame();
        compile(
            &mut f.rec,
            ScheduleOptions {
                async_compute: true,
                concurrent: false,
            },
        );
        let rec = &f.rec;
        assert!(rec.resources.iter().all(|r| !r.concurrent));
        // Depth: released by the graphics segment, acquired by AO with the same layouts.
        let release = releases(rec, rec.passes[f.prepass].segment)
            .iter()
            .find(|b| b.resource == 1)
            .copied()
            .expect("depth release");
        assert_eq!(
            release.transfer,
            Some((QueueKind::Graphics, QueueKind::AsyncCompute))
        );
        assert_eq!(release.dst_stages, vk::PipelineStageFlags2::NONE);
        let acquire = rec.image_barriers[rec.passes[f.ao].image_barriers.start as usize
            ..rec.passes[f.ao].image_barriers.end as usize]
            .iter()
            .find(|b| b.resource == 1)
            .copied()
            .expect("depth acquire");
        assert_eq!(acquire.transfer, release.transfer);
        assert_eq!(
            (acquire.old_layout, acquire.new_layout),
            (release.old_layout, release.new_layout)
        );
        // And back for the forward pass's depth test.
        let back = releases(rec, rec.passes[f.blur].segment)
            .iter()
            .filter(|b| b.transfer == Some((QueueKind::AsyncCompute, QueueKind::Graphics)))
            .count();
        assert_eq!(back, 2, "depth and the blurred AO move back to graphics");
    }

    #[test]
    fn a_swapchain_image_outside_the_callers_command_buffer_keeps_one_queue() {
        let mut f = frame();
        // The shadow pass (an early graphics segment) renders into the swapchain image.
        let swapchain = texture(&mut f.rec, "swapchain");
        f.rec.resources[swapchain as usize].initial =
            Some(TextureState::acquired_swapchain_image().into());
        f.rec.resources[swapchain as usize].caller_waits = true;
        let start = f.rec.accesses.len() as u32;
        let first = &mut f.rec.passes[f.shadow];
        // Move the shadow pass's accesses to the end of the table with the new one.
        let range = first.accesses.start as usize..first.accesses.end as usize;
        let mut copied: Vec<_> = f.rec.accesses[range].to_vec();
        copied.push(super::super::compile::AccessRecord {
            resource: swapchain,
            info: AccessInfo::color_attachment(false),
        });
        f.rec.accesses.extend(copied);
        f.rec.passes[f.shadow].accesses = start..f.rec.accesses.len() as u32;
        assert!(!compile(&mut f.rec, ASYNC));
        assert_eq!(f.rec.segments.len(), 1);
    }

    /// An upload (side effects) whose results the shading reads without declaring them
    /// (through bindless indices): it declares only its staging buffer.
    fn upload(rec: &mut Records) -> usize {
        rec.resources
            .push(ResourceRecord::new("staging", Kind::Buffer));
        let staging = (rec.resources.len() - 1) as u32;
        let p = pass(rec, "upload", &[(staging, compute(Access::TransferRead))]);
        rec.passes[p].side_effects = true;
        p
    }

    #[test]
    fn a_fence_follows_the_passes_declared_before_it_and_precedes_the_rest() {
        let ids = |rec: &Records| -> Vec<usize> { rec.order.iter().map(|&p| p as usize).collect() };
        // Declared first but unfenced, the upload runs after the AO pass and its ancestors.
        let mut f = frame_after(|rec| {
            upload(rec);
        });
        assert!(compile(&mut f.rec, ASYNC));
        assert_eq!(
            ids(&f.rec),
            [f.prepass, f.ao, f.blur, 0, f.shadow, f.forward, f.post]
        );
        // Fenced, it keeps its place: first.
        let mut f = frame_after(|rec| {
            let p = upload(rec);
            rec.passes[p].fence = true;
        });
        assert!(compile(&mut f.rec, ASYNC));
        assert_eq!(
            ids(&f.rec),
            [0, f.prepass, f.ao, f.blur, f.shadow, f.forward, f.post]
        );
        // Declared last, it runs last.
        let mut f = frame();
        let last = upload(&mut f.rec);
        f.rec.passes[last].fence = true;
        assert!(compile(&mut f.rec, ASYNC));
        assert_eq!(
            ids(&f.rec),
            [f.prepass, f.ao, f.blur, f.shadow, f.forward, f.post, last]
        );
    }

    #[test]
    fn scheduling_reuses_its_memory() {
        let mut f = frame();
        compile(&mut f.rec, ASYNC);
        let capacities = |rec: &Records| {
            (
                rec.order.capacity(),
                rec.edges.capacity(),
                rec.segments.capacity(),
                rec.segment_passes.capacity(),
            )
        };
        let before = capacities(&f.rec);
        for _ in 0..5 {
            compile(&mut f.rec, ASYNC);
        }
        assert_eq!(before, capacities(&f.rec));
    }
}
