//! Picking in a view (ADR 0109): which instances are under pixels, for the editor.
//!
//! - **The nearest surfaces** in a [`SQUARE`] × [`SQUARE`] square centred on a pixel, at
//!   the view's size: the viewport shifts the view's pixel grid onto the square, so only it
//!   is rasterized. Each pixel keeps the nearest surface's instance over a depth of its own
//!   (`pickVertex`, `pickFragment` in `forward.slang`); masked materials are alpha-tested
//!   (`pickVertexMasked`, `pickFragmentMasked`), and blended surfaces are drawn too, with
//!   depth writes, unless the pick leaves them out. What is drawn:
//!   - [`Picker::add_scene_pass`]: the persistent scene's candidates, the instances whose
//!     world box reaches the square (the planes bounding its pixels), from commands written on the CPU
//!     at each mesh's finest resident level, with the camera without jitter;
//!   - [`Picker::add_pass`]: the view's culled commands (the culling pass's visible lists,
//!     early and late phases, with their detail levels), for a view that draws a list.
//! - **What lies in pixels** ([`Picker::add_list_pass`]): the candidates of a rectangle
//!   (a pixel and a radius around it, or a box), each keeping its nearest depth there
//!   (`pickListVertex` and `pickListFragment`, and their masked twins):
//!   - every surface, hidden ones too, drawn without a depth test: what lies under the
//!     pointer front to back, to cycle through overlapping objects or list them;
//!   - or only what shows: the candidates' depth first (`depthVertex`, and
//!     `depthVertexMasked` with `shadowFragmentMasked`), then the candidates again tested
//!     against it before shading (`pickVisibleFragment`): a box's visible pixels.
//! - **What an id is:** the index the view's vertex shaders read the instance with: the
//!   persistent scene's slot ([`crate::gpu_scene::GpuScene`]), or a draw list's item.
//! - **Cost.** Separate passes, so the view's pipelines carry no code for them (the
//!   register rule, CLAUDE.md §5): views nobody picks in pay nothing. A scene pick costs
//!   the candidates' vertex work and a scan of the scene's world boxes on the CPU; a pick of
//!   the view's commands the visible instances' vertex work, as texture feedback's
//!   ([`crate::feedback`]).
//! - **Results** reach the CPU through readback rings, `frames_in_flight` frames later
//!   ([`Picker::poll`], [`Picker::poll_list`]).

use crate::forward::{
    BLENDED, BLENDED_STATES, DEPTH_STATES, DEPTH_VERTEX, DEPTH_VERTEX_MASKED, DOUBLE_SIDED,
    DrawArea, ForwardPasses, MASKED, MIRRORED, Push, SHADOW_FRAGMENT_MASKED, TypedPipelines, faces,
    set_draw_area,
};
use crate::gpu_scene::SceneFrame;
use crate::graph::{
    Access, BufferDesc, BufferState, FrameGraph, ImportedBuffer, LoadOp, PassContext, ShaderStages,
    StoreOp, TextureDesc,
};
use crate::material::MaterialFrame;
use crate::mesh::GeometryFrame;
use crate::upload::{Staged, UploadRing};
use engine_core::math::{Mat4, Vec4};
use engine_rhi::ash::vk;
use engine_rhi::{
    Allocation, Allocator, Bindless, Device, GraphicsPipelineDesc, MemoryLocation, Pipeline,
    PipelineDesc, RhiError, ShaderStageDesc, spirv_words,
};

const VERTEX: &[u8] = include_bytes!("../shaders/forward.pickVertex.spv");
const VERTEX_MASKED: &[u8] = include_bytes!("../shaders/forward.pickVertexMasked.spv");
const FRAGMENT: &[u8] = include_bytes!("../shaders/forward.pickFragment.spv");
const FRAGMENT_MASKED: &[u8] = include_bytes!("../shaders/forward.pickFragmentMasked.spv");
const LIST_VERTEX: &[u8] = include_bytes!("../shaders/forward.pickListVertex.spv");
const LIST_VERTEX_MASKED: &[u8] = include_bytes!("../shaders/forward.pickListVertexMasked.spv");
const LIST_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.pickListFragment.spv");
const LIST_FRAGMENT_MASKED: &[u8] = include_bytes!("../shaders/forward.pickListFragmentMasked.spv");
const VISIBLE_FRAGMENT: &[u8] = include_bytes!("../shaders/forward.pickVisibleFragment.spv");
const VISIBLE_FRAGMENT_MASKED: &[u8] =
    include_bytes!("../shaders/forward.pickVisibleFragmentMasked.spv");

/// The side of the square a pick reads around its pixel, in the view's pixels.
pub const SQUARE: u32 = 32;
/// Where the pick's pixel is in the square.
const CENTER: u32 = SQUARE / 2;
const ID_FORMAT: vk::Format = vk::Format::R32_UINT;
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
/// Bytes of a square's ids.
const BYTES: u64 = (SQUARE * SQUARE * 4) as u64;
/// The most instances a list holds; more in its pixels are left out
/// ([`ListResult::truncated`]).
pub const MAX_LIST: u32 = 65_536;
/// Bytes of a list's depths, a word per instance.
const LIST_BYTES: u64 = MAX_LIST as u64 * 4;
/// Bytes of an indexed indirect draw.
const COMMAND_BYTES: u32 = core::mem::size_of::<vk::DrawIndexedIndirectCommand>() as u32;

/// A pick: a pixel of the view, at the view's size (its render size under upscaling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickRequest {
    /// The pixel's column, from the view's left.
    pub x: u32,
    /// Its row, from the view's top.
    pub y: u32,
    /// Blended surfaces can be picked: they hide what is behind them, as opaque ones do.
    pub transparent: bool,
}

/// What a pick saw: the ids of the square around its pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickResult {
    /// The frame that drew it.
    pub frame: u64,
    /// What was asked.
    pub request: PickRequest,
    /// [`SQUARE`]² ids by rows from the top: an instance's index plus one, 0 where none
    /// was. The pick's pixel is at ([`SQUARE`] / 2, [`SQUARE`] / 2).
    pub ids: Vec<u32>,
}

impl PickResult {
    /// The instance at the pick's pixel, or the nearest one within `radius` pixels of it
    /// (the first in reading order among equally near ones): thin objects stay easy to hit.
    pub fn nearest(&self, radius: u32) -> Option<u32> {
        let r = radius.min(CENTER - 1) as i32;
        let mut best: Option<(i32, u32)> = None;
        for dy in -r..=r {
            for dx in -r..=r {
                let d = dx * dx + dy * dy;
                if d > r * r || best.is_some_and(|(b, _)| d >= b) {
                    continue;
                }
                let (x, y) = (CENTER as i32 + dx, CENTER as i32 + dy);
                let id = self.ids[(y * SQUARE as i32 + x) as usize];
                if id != 0 {
                    best = Some((d, id - 1));
                }
            }
        }
        best.map(|(_, instance)| instance)
    }
}

/// The view a scene pick draws in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PickView {
    /// Its camera, without jitter.
    pub clip_from_world: Mat4,
    /// Its size in pixels (the render size under upscaling): what the picks' pixels are
    /// in.
    pub size: (u32, u32),
}

/// A pick of what lies in pixels of the view (at the view's size, as a [`PickRequest`]'s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListRequest {
    /// The pixels, `[x0, y0, x1, y1]` (the view's part of them counts).
    pub rect: [u32; 4],
    /// Only what shows there counts, each pixel's nearest surface (a box's visible
    /// pixels); otherwise every surface, hidden ones too.
    pub visible: bool,
    /// Blended surfaces are listed too (and with `visible` hide what is behind them).
    pub transparent: bool,
}

impl ListRequest {
    /// Every surface within `radius` pixels of the pixel `(x, y)` (a square of side
    /// `2 radius + 1`): what lies under the pointer, thin objects easy to list.
    pub fn around(x: u32, y: u32, radius: u32, transparent: bool) -> Self {
        Self {
            rect: [
                x.saturating_sub(radius),
                y.saturating_sub(radius),
                x.saturating_add(radius + 1),
                y.saturating_add(radius + 1),
            ],
            visible: false,
            transparent,
        }
    }
}

/// What lay in a list's pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct ListResult {
    /// The frame that drew it.
    pub frame: u64,
    /// What was asked.
    pub request: ListRequest,
    /// The instances with a surface in the pixels (only shown ones, with
    /// [`ListRequest::visible`]), nearest first: each one's index (the persistent scene's
    /// slot) and its nearest depth there (0 at the near plane, 1 at the far one).
    pub hits: Vec<(u32, f32)>,
    /// More than [`MAX_LIST`] instances could be there: the others were left out.
    pub truncated: bool,
}

/// What a pick's `add_*` call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickPass {
    /// The pick is declared: its result comes with [`Picker::poll`] (or
    /// [`Picker::poll_list`]).
    Declared,
    /// Nothing can be under the pixels (the view drew nothing, or no instance reaches
    /// them): nothing was declared, and that is the answer.
    Empty,
    /// Every readback holds a pick not yet polled, or the upload ring is full: nothing
    /// was declared.
    Busy,
}

/// A readback of a ring: its host buffer, and the request it holds until polled, with a
/// list's items' slots.
#[derive(Debug)]
struct Readback<R> {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    written: Option<(u64, R)>,
    /// A list's items' slots, and whether candidates were left out.
    slots: Vec<u32>,
    truncated: bool,
}

/// The newest of `ring`'s picks whose frame is `completed` or older (its index, frame and
/// request): it and the older ones are done with.
fn newest<R: Copy>(ring: &mut [Readback<R>], completed: u64) -> Option<(usize, u64, R)> {
    let mut newest: Option<(usize, u64, R)> = None;
    for (i, r) in ring.iter().enumerate() {
        if let Some((frame, request)) = r.written
            && frame <= completed
            && newest.is_none_or(|(_, f, _)| frame > f)
        {
            newest = Some((i, frame, request));
        }
    }
    for r in ring.iter_mut() {
        if r.written.is_some_and(|(frame, _)| frame <= completed) {
            r.written = None;
        }
    }
    newest
}

/// The candidates of a scene pick: the instances a region's planes reach, each mesh at its
/// finest resident level, by pipeline state.
#[derive(Debug, Default)]
struct Candidates {
    /// The scan's slots.
    slots: Vec<u32>,
    /// Each drawable one's state (the bits of its faces and alpha test), slot and draw.
    keyed: Vec<(u8, u32, vk::DrawIndexedIndirectCommand)>,
    /// The drawn slots and their draws, in state order and by mesh: a draw's instances
    /// are its items (consecutive candidates of one mesh are one instanced draw).
    visible: Vec<u32>,
    commands: Vec<vk::DrawIndexedIndirectCommand>,
    /// Runs of one state: the state, its first draw, their count.
    groups: Vec<(u8, u32, u32)>,
    /// More could be drawn than were kept.
    truncated: bool,
}

/// The runs of pipeline states a pass draws (the state, its first draw, their count): at
/// most one per state of the faces' and the alpha test's bits, and how many there are.
type Runs = ([(u8, u32, u32); 8], usize);

impl Candidates {
    /// The runs, copied for a pass's recording.
    fn runs(&self) -> Runs {
        let mut runs = [(0, 0, 0); 8];
        let n = self.groups.len().min(runs.len());
        runs[..n].copy_from_slice(&self.groups[..n]);
        (runs, n)
    }

    /// Finds the instances of `scene` that `planes` reach and that can be drawn (a resident
    /// mesh; blended surfaces only with `transparent`), at most `limit` of them.
    fn find(
        &mut self,
        scene: &SceneFrame<'_>,
        geometry: &GeometryFrame<'_>,
        planes: &[Vec4],
        transparent: bool,
        limit: usize,
    ) {
        self.slots.clear();
        scene.candidates(planes, &mut self.slots);
        self.keyed.clear();
        self.truncated = false;
        for &slot in &self.slots {
            let Some((mesh, state)) = scene.slot_draw(slot) else {
                continue;
            };
            if usize::from(state) & BLENDED != 0 && !transparent {
                continue;
            }
            let Some((first_index, index_count, first_vertex)) = geometry.finest_draw(mesh) else {
                continue;
            };
            if self.keyed.len() == limit {
                self.truncated = true;
                break;
            }
            self.keyed.push((
                state & (MASKED | DOUBLE_SIDED | MIRRORED) as u8,
                slot,
                vk::DrawIndexedIndirectCommand {
                    index_count,
                    instance_count: 1,
                    first_index,
                    vertex_offset: first_vertex as i32,
                    first_instance: 0,
                },
            ));
        }
        self.keyed.sort_unstable_by_key(|&(state, slot, c)| {
            (state, c.first_index, c.vertex_offset, c.index_count, slot)
        });
        self.visible.clear();
        self.commands.clear();
        self.groups.clear();
        for (item, &(state, slot, command)) in self.keyed.iter().enumerate() {
            let item = item as u32;
            self.visible.push(slot);
            // The same mesh as the draw before, in the same state: one more instance of it.
            let same_state = self.groups.last().is_some_and(|&(s, _, _)| s == state);
            if let Some(last) = self.commands.last_mut()
                && same_state
                && (last.first_index, last.vertex_offset, last.index_count)
                    == (
                        command.first_index,
                        command.vertex_offset,
                        command.index_count,
                    )
            {
                last.instance_count += 1;
                continue;
            }
            self.commands.push(vk::DrawIndexedIndirectCommand {
                first_instance: item,
                ..command
            });
            let first = self.commands.len() as u32 - 1;
            match self.groups.last_mut() {
                Some((s, _, count)) if *s == state => *count += 1,
                _ => self.groups.push((state, first, 1)),
            }
        }
    }
}

/// The planes bounding the pixels `[x0, y0, x1, y1]` of a view of `size` pixels through
/// `clip_from_world`, in world space and facing inward (`xyz · p + w ≥ 0` inside): the four
/// sides, then the near and far planes. A world box that reaches every one may show in
/// those pixels.
pub(crate) fn region_planes(
    clip_from_world: &Mat4,
    [x0, y0, x1, y1]: [f32; 4],
    (width, height): (u32, u32),
) -> [Vec4; 6] {
    // The rows of clip_from_world: a point's clip coordinates are its dot products with
    // them.
    let rows = clip_from_world.transpose();
    let (rx, ry, rz, rw) = (rows.x_axis, rows.y_axis, rows.z_axis, rows.w_axis);
    let ndc = |p: f32, size: u32| p / size.max(1) as f32 * 2.0 - 1.0;
    let (left, right) = (ndc(x0, width), ndc(x1, width));
    let (top, bottom) = (ndc(y0, height), ndc(y1, height));
    // Inside: left ≤ x / w ≤ right and top ≤ y / w ≤ bottom (w > 0 in front of the
    // camera), and 0 ≤ z ≤ w.
    [
        rx - rw * left,
        rw * right - rx,
        ry - rw * top,
        rw * bottom - ry,
        rz,
        rw - rz,
    ]
}

/// Records the candidates' draws: one indirect draw per run of a pipeline state, with
/// `pipelines` (opaque, masked) into `area`.
///
/// # Safety
///
/// `ctx` records inside a rendering scope of a pass that reads the geometry pool, the
/// material table and the scene's instances; `commands` holds `groups`' draws in the
/// ring's slot of this frame; the pipelines live until the frame has executed.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_groups(
    ctx: &PassContext<'_>,
    bindless: &Bindless,
    geometry: &GeometryFrame<'_>,
    pipelines: [vk::Pipeline; 2],
    push: &Push,
    groups: &[(u8, u32, u32)],
    commands: Staged,
    area: DrawArea,
) {
    let mut bound: Option<vk::Pipeline> = None;
    for &(state, first, count) in groups {
        let state = usize::from(state);
        let pipeline = pipelines[usize::from(state & MASKED != 0)];
        let (cull, front) = faces(state, false);
        // SAFETY: guaranteed by the caller.
        unsafe {
            if bound != Some(pipeline) {
                ctx.device
                    .cmd_bind_pipeline(ctx.cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
                if bound.is_none() {
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    set_draw_area(ctx.device, ctx.cmd, area);
                    bindless.push_constants(ctx.cmd, bytemuck::bytes_of(push));
                    geometry.bind_indices(ctx);
                }
                bound = Some(pipeline);
            }
            ctx.device.cmd_set_cull_mode(ctx.cmd, cull);
            ctx.device.cmd_set_front_face(ctx.cmd, front);
            ctx.device.cmd_draw_indexed_indirect(
                ctx.cmd,
                commands.buffer,
                commands.offset + u64::from(first * COMMAND_BYTES),
                count,
                COMMAND_BYTES,
            );
        }
    }
}

/// Picks in one view (see the module documentation).
#[derive(Debug)]
pub struct Picker {
    /// The nearest surfaces': opaque and masked (blended surfaces are drawn with them
    /// too).
    pipelines: [Pipeline; 2],
    /// What lies in pixels: every surface (opaque, masked); the candidates' depth, then
    /// what shows over it.
    list_pipelines: [Pipeline; 2],
    depth_pipelines: [Pipeline; 2],
    visible_pipelines: [Pipeline; 2],
    readbacks: Vec<Readback<PickRequest>>,
    lists: Vec<Readback<ListRequest>>,
    frames_in_flight: u64,
    latest: Option<PickResult>,
    latest_list: Option<ListResult>,
    candidates: Candidates,
}

impl Picker {
    /// A picker for one view; `frames_in_flight` sizes the readback rings.
    ///
    /// # Errors
    ///
    /// When a pipeline or a buffer cannot be made.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frames_in_flight: usize,
    ) -> Result<Self, RhiError> {
        let invalid = || RhiError::Vulkan {
            call: "embedded SPIR-V",
            result: vk::Result::ERROR_INVALID_SHADER_NV,
        };
        let words = |bytes| spirv_words(bytes).ok_or_else(invalid);
        let (vertex, vertex_masked) = (words(VERTEX)?, words(VERTEX_MASKED)?);
        let (fragment, fragment_masked) = (words(FRAGMENT)?, words(FRAGMENT_MASKED)?);
        let (list_vertex, list_vertex_masked) = (words(LIST_VERTEX)?, words(LIST_VERTEX_MASKED)?);
        let (list_fragment, list_fragment_masked) =
            (words(LIST_FRAGMENT)?, words(LIST_FRAGMENT_MASKED)?);
        let (depth_vertex, depth_vertex_masked) =
            (words(DEPTH_VERTEX)?, words(DEPTH_VERTEX_MASKED)?);
        let depth_fragment_masked = words(SHADOW_FRAGMENT_MASKED)?;
        let (visible_fragment, visible_fragment_masked) =
            (words(VISIBLE_FRAGMENT)?, words(VISIBLE_FRAGMENT_MASKED)?);
        /// What a pipeline draws into.
        #[derive(Clone, Copy)]
        enum Into {
            /// Ids over a depth (the nearest surfaces).
            Ids,
            /// Nothing: no attachments, no depth test (every surface).
            Nothing,
            /// A depth, written (the candidates' depth).
            Depth,
            /// A depth, tested (less or equal), not written (what shows over it).
            OverDepth,
        }
        fn desc<'a>(
            (vertex, v): (&'a [u32], &'static core::ffi::CStr),
            fragment: Option<(&'a [u32], &'static core::ffi::CStr)>,
            into: Into,
        ) -> PipelineDesc<'a> {
            let stages = GraphicsPipelineDesc::new(
                ShaderStageDesc {
                    spirv: vertex,
                    entry: v,
                },
                fragment.map(|(spirv, entry)| ShaderStageDesc { spirv, entry }),
            );
            let stages = GraphicsPipelineDesc {
                dynamic_culling: true,
                ..stages
            };
            PipelineDesc::Graphics(match into {
                Into::Ids => GraphicsPipelineDesc {
                    color_formats: &[ID_FORMAT],
                    depth_format: DEPTH_FORMAT,
                    ..stages
                },
                Into::Nothing => GraphicsPipelineDesc {
                    color_formats: &[],
                    depth_format: vk::Format::UNDEFINED,
                    depth_test: false,
                    depth_write: false,
                    ..stages
                },
                Into::Depth => GraphicsPipelineDesc {
                    color_formats: &[],
                    depth_format: DEPTH_FORMAT,
                    ..stages
                },
                Into::OverDepth => GraphicsPipelineDesc {
                    color_formats: &[],
                    depth_format: DEPTH_FORMAT,
                    depth_write: false,
                    depth_compare: vk::CompareOp::LESS_OR_EQUAL,
                    ..stages
                },
            })
        }
        let mut pipelines = Pipeline::create_many(
            device,
            bindless.pipeline_layout(),
            &[
                desc(
                    (&vertex, c"pickVertex"),
                    Some((&fragment, c"pickFragment")),
                    Into::Ids,
                ),
                desc(
                    (&vertex_masked, c"pickVertexMasked"),
                    Some((&fragment_masked, c"pickFragmentMasked")),
                    Into::Ids,
                ),
                desc(
                    (&list_vertex, c"pickListVertex"),
                    Some((&list_fragment, c"pickListFragment")),
                    Into::Nothing,
                ),
                desc(
                    (&list_vertex_masked, c"pickListVertexMasked"),
                    Some((&list_fragment_masked, c"pickListFragmentMasked")),
                    Into::Nothing,
                ),
                desc((&depth_vertex, c"depthVertex"), None, Into::Depth),
                desc(
                    (&depth_vertex_masked, c"depthVertexMasked"),
                    Some((&depth_fragment_masked, c"shadowFragmentMasked")),
                    Into::Depth,
                ),
                desc(
                    (&list_vertex, c"pickListVertex"),
                    Some((&visible_fragment, c"pickVisibleFragment")),
                    Into::OverDepth,
                ),
                desc(
                    (&list_vertex_masked, c"pickListVertexMasked"),
                    Some((&visible_fragment_masked, c"pickVisibleFragmentMasked")),
                    Into::OverDepth,
                ),
            ],
        )?
        .into_iter();
        let mut next = || {
            pipelines.next().ok_or(RhiError::Vulkan {
                call: "pick pipelines (fewer than described)",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        let (opaque, masked) = (next()?, next()?);
        let (list_opaque, list_masked) = (next()?, next()?);
        let (depth_opaque, depth_masked) = (next()?, next()?);
        let (visible_opaque, visible_masked) = (next()?, next()?);
        let frames_in_flight = frames_in_flight.max(1) as u64;
        // One more than the frames in flight in each ring: with a poll every frame, one is
        // free.
        fn ring<R>(
            allocator: &mut Allocator,
            frames_in_flight: u64,
            bytes: u64,
        ) -> Result<Vec<Readback<R>>, RhiError> {
            let mut ring = Vec::new();
            for _ in 0..=frames_in_flight {
                let (buffer, allocation) = allocator.create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST),
                    MemoryLocation::Readback,
                )?;
                ring.push(Readback {
                    buffer,
                    allocation: Some(allocation),
                    written: None,
                    slots: Vec::new(),
                    truncated: false,
                });
            }
            Ok(ring)
        }
        let readbacks = ring(allocator, frames_in_flight, BYTES)?;
        let lists = match ring(allocator, frames_in_flight, LIST_BYTES) {
            Ok(lists) => lists,
            Err(e) => {
                for r in readbacks {
                    if let Some(allocation) = r.allocation {
                        // SAFETY: just made, never used by the GPU.
                        unsafe { allocator.destroy_buffer(r.buffer, allocation) };
                    }
                }
                return Err(e);
            }
        };
        Ok(Self {
            pipelines: [opaque, masked],
            list_pipelines: [list_opaque, list_masked],
            depth_pipelines: [depth_opaque, depth_masked],
            visible_pipelines: [visible_opaque, visible_masked],
            readbacks,
            lists,
            frames_in_flight,
            latest: None,
            latest_list: None,
            candidates: Candidates::default(),
        })
    }

    /// The driver's statistics of the pipelines (registers, occupancy), when the device
    /// captures them: the nearest surfaces' two, the lists' two, the depth's two, and the
    /// two of what shows over it.
    pub fn statistics(&self, device: &Device) -> [Vec<engine_rhi::PipelineStatistic>; 8] {
        let all = [
            &self.pipelines,
            &self.list_pipelines,
            &self.depth_pipelines,
            &self.visible_pipelines,
        ];
        core::array::from_fn(|i| all[i / 2][i % 2].statistics(device))
    }

    /// Takes the newest pick whose frame has finished (call each frame before the `add_*`
    /// calls, with the same frame numbers): the latest result, when one arrived since the
    /// last call.
    pub fn poll(&mut self, frame_number: u64) -> Option<&PickResult> {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let (i, frame, request) = newest(&mut self.readbacks, completed)?;
        let ptr = self.readbacks[i]
            .allocation
            .as_ref()
            .and_then(Allocation::mapped_ptr)?;
        // SAFETY: the frame that wrote it has finished (its graph made the copy visible to
        // the host); the mapping holds a square of ids.
        let ids = unsafe {
            core::slice::from_raw_parts(ptr.as_ptr().cast::<u32>(), (SQUARE * SQUARE) as usize)
        };
        let latest = self.latest.get_or_insert_with(|| PickResult {
            frame,
            request,
            ids: Vec::new(),
        });
        latest.frame = frame;
        latest.request = request;
        latest.ids.clear();
        latest.ids.extend_from_slice(ids);
        self.latest.as_ref()
    }

    /// The latest result [`Self::poll`] took.
    pub fn latest(&self) -> Option<&PickResult> {
        self.latest.as_ref()
    }

    /// The latest list [`Self::poll_list`] took.
    pub fn latest_list(&self) -> Option<&ListResult> {
        self.latest_list.as_ref()
    }

    /// Takes the newest list whose frame has finished, as [`Self::poll`] does picks.
    pub fn poll_list(&mut self, frame_number: u64) -> Option<&ListResult> {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let (i, frame, request) = newest(&mut self.lists, completed)?;
        let readback = &self.lists[i];
        let ptr = readback
            .allocation
            .as_ref()
            .and_then(Allocation::mapped_ptr)?;
        // SAFETY: the frame that wrote it has finished (its graph made the copy visible to
        // the host); the mapping holds a word for each of the list's items.
        let depths = unsafe {
            core::slice::from_raw_parts(ptr.as_ptr().cast::<u32>(), readback.slots.len())
        };
        let latest = self.latest_list.get_or_insert_with(|| ListResult {
            frame,
            request,
            hits: Vec::new(),
            truncated: false,
        });
        latest.frame = frame;
        latest.request = request;
        latest.truncated = readback.truncated;
        latest.hits.clear();
        latest.hits.extend(
            readback
                .slots
                .iter()
                .zip(depths)
                .filter(|&(_, &depth)| depth != u32::MAX)
                .map(|(&slot, &depth)| (slot, f32::from_bits(depth))),
        );
        latest
            .hits
            .sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        self.latest_list.as_ref()
    }

    /// Declares a pick of `request` in a view drawn this frame (`passes`, from its
    /// [`crate::forward::ForwardRenderer::add_passes`]) of `width × height` pixels, from
    /// the view's culled commands.
    #[allow(clippy::too_many_arguments)]
    pub fn add_pass<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        passes: &ForwardPasses,
        (width, height): (u32, u32),
        request: PickRequest,
        frame_number: u64,
    ) -> PickPass {
        engine_profiler::zone!("render::Picker::add_pass");
        let Some(draws) = passes.draws else {
            return PickPass::Empty;
        };
        let Some(readback) = self.readbacks.iter_mut().find(|r| r.written.is_none()) else {
            return PickPass::Busy;
        };
        readback.written = Some((frame_number, request));
        let readback = readback.buffer;
        let ids = fg.create_texture("pick ids", TextureDesc::new_2d(ID_FORMAT, SQUARE, SQUARE));
        let depth = fg.create_texture(
            "pick depth",
            TextureDesc::new_2d(DEPTH_FORMAT, SQUARE, SQUARE),
        );
        let area = square_area(request, (width, height));
        let mask = if request.transparent {
            DEPTH_STATES | BLENDED_STATES
        } else {
            DEPTH_STATES
        };
        let pipelines = &self.pipelines;
        let geometry_copy = *geometry;
        materials
            .read(geometry.read(fg.add_pass("pick")))
            .read(draws.commands, Access::Indirect)
            .read(draws.visible, Access::StorageRead(ShaderStages::VERTEX))
            .maybe_read(draws.instances, Access::StorageRead(ShaderStages::VERTEX))
            .color(ids, LoadOp::Clear([0.0; 4]), StoreOp::Store)
            .depth(
                depth,
                LoadOp::Clear([1.0, 0.0, 0.0, 0.0]),
                StoreOp::DontCare,
            )
            .execute(move |ctx| {
                let push = Push {
                    visible: ctx.address(draws.visible),
                    ..draws.push
                };
                let [opaque, masked] = pipelines;
                for phase in 0..draws.phases {
                    // SAFETY: recording inside the pass's rendering scope; the pass reads
                    // the pool, the material table and the view's lists.
                    unsafe {
                        draws.states.draw_slice(
                            ctx,
                            bindless,
                            &geometry_copy,
                            [opaque, masked],
                            // Picks run no material code of the types' own (ADR 0092).
                            &TypedPipelines::BUILTIN,
                            &push,
                            false,
                            mask,
                            ctx.buffer(draws.commands),
                            u64::from(phase) * draws.command_bytes,
                            area,
                            (0, 1),
                        );
                        // Instances drawn by meshlet (ADR 0085; none are blended).
                        if let Some(m) = draws.meshlets {
                            draws.states.draw_meshlets(
                                ctx,
                                bindless,
                                &geometry_copy,
                                [opaque, masked],
                                &push,
                                false,
                                mask,
                                ctx.buffer(draws.commands),
                                m.lists(phase),
                                &m,
                                area,
                            );
                        }
                    }
                }
            });
        copy_square(fg, ids, readback);
        PickPass::Declared
    }

    /// Declares a pick of `request` in a view of the persistent scene drawn this frame,
    /// from its candidates: the instances whose world box reaches the square, drawn alone
    /// through `view`'s camera. The picker must live until the frame has executed (its
    /// pipelines are recorded).
    #[allow(clippy::too_many_arguments)]
    pub fn add_scene_pass<'g>(
        &mut self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        scene: &SceneFrame<'g>,
        view: &PickView,
        request: PickRequest,
        frame_number: u64,
    ) -> PickPass {
        engine_profiler::zone!("render::Picker::add_scene_pass");
        // A pixel outside the view has nothing under it.
        if request.x >= view.size.0 || request.y >= view.size.1 {
            return PickPass::Empty;
        }
        let [x0, y0, x1, y1] = region(request.x, request.y, CENTER, CENTER, view.size);
        let planes = region_planes(
            &view.clip_from_world,
            [x0, y0, x1, y1].map(|v| v as f32),
            view.size,
        );
        self.candidates
            .find(scene, geometry, &planes, request.transparent, usize::MAX);
        if self.candidates.commands.is_empty() {
            return PickPass::Empty;
        }
        let Some(readback) = self.readbacks.iter_mut().find(|r| r.written.is_none()) else {
            return PickPass::Busy;
        };
        let (Some(frame), Some(visible), Some(commands)) = (
            ring.push(&[view.clip_from_world.to_cols_array()]),
            ring.push(&self.candidates.visible),
            ring.stage(&self.candidates.commands),
        ) else {
            return PickPass::Busy;
        };
        readback.written = Some((frame_number, request));
        let readback = readback.buffer;
        let ids = fg.create_texture("pick ids", TextureDesc::new_2d(ID_FORMAT, SQUARE, SQUARE));
        let depth = fg.create_texture(
            "pick depth",
            TextureDesc::new_2d(DEPTH_FORMAT, SQUARE, SQUARE),
        );
        let area = square_area(request, view.size);
        let push = Push {
            frame,
            instances: scene.current_address,
            visible,
            positions: geometry.position_address(),
            attributes: geometry.attribute_address(),
            materials: materials.address(),
            ..bytemuck::Zeroable::zeroed()
        };
        let pipelines = [self.pipelines[0].raw(), self.pipelines[1].raw()];
        let (runs, count) = self.candidates.runs();
        let geometry_copy = *geometry;
        materials
            .read(geometry.read(fg.add_pass("pick")))
            .read(scene.current, Access::StorageRead(ShaderStages::VERTEX))
            .color(ids, LoadOp::Clear([0.0; 4]), StoreOp::Store)
            .depth(
                depth,
                LoadOp::Clear([1.0, 0.0, 0.0, 0.0]),
                StoreOp::DontCare,
            )
            .execute(move |ctx| {
                // SAFETY: recording inside the pass's rendering scope; the pass reads the
                // pool, the material table and the scene's instances, the draws lie in the
                // ring's slot for this frame, and the picker outlives the frame.
                unsafe {
                    draw_groups(
                        ctx,
                        bindless,
                        &geometry_copy,
                        pipelines,
                        &push,
                        &runs[..count],
                        commands,
                        area,
                    );
                }
            });
        copy_square(fg, ids, readback);
        PickPass::Declared
    }

    /// Declares a pick of what lies in `request`'s pixels in a view of the persistent
    /// scene drawn this frame: the candidates there (the first [`MAX_LIST`] in the scene's
    /// order), drawn alone through `view`'s camera, each keeping its nearest depth there:
    /// without a depth test, or over their own depth with [`ListRequest::visible`]. The
    /// picker must live until the frame has executed.
    #[allow(clippy::too_many_arguments)]
    pub fn add_list_pass<'g>(
        &mut self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        scene: &SceneFrame<'g>,
        view: &PickView,
        request: ListRequest,
        frame_number: u64,
    ) -> PickPass {
        engine_profiler::zone!("render::Picker::add_list_pass");
        let (width, height) = view.size;
        let [x0, y0, x1, y1] = [
            request.rect[0].min(width),
            request.rect[1].min(height),
            request.rect[2].min(width),
            request.rect[3].min(height),
        ];
        if x0 >= x1 || y0 >= y1 {
            return PickPass::Empty;
        }
        let planes = region_planes(
            &view.clip_from_world,
            [x0, y0, x1, y1].map(|v| v as f32),
            view.size,
        );
        self.candidates.find(
            scene,
            geometry,
            &planes,
            request.transparent,
            MAX_LIST as usize,
        );
        if self.candidates.commands.is_empty() {
            return PickPass::Empty;
        }
        let Some(readback) = self.lists.iter_mut().find(|r| r.written.is_none()) else {
            return PickPass::Busy;
        };
        let (Some(frame), Some(visible), Some(commands)) = (
            ring.push(&[view.clip_from_world.to_cols_array()]),
            ring.push(&self.candidates.visible),
            ring.stage(&self.candidates.commands),
        ) else {
            return PickPass::Busy;
        };
        readback.written = Some((frame_number, request));
        readback.slots.clear();
        readback.slots.extend_from_slice(&self.candidates.visible);
        readback.truncated = self.candidates.truncated;
        let bytes = readback.slots.len() as u64 * 4;
        let out = fg.import_buffer(
            "pick list readback",
            ImportedBuffer {
                buffer: readback.buffer,
                size: LIST_BYTES,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        let depths = fg.create_buffer("pick list", BufferDesc { size: LIST_BYTES });
        fg.add_pass("pick list clear")
            .write(depths, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording outside a rendering scope; within the buffer.
                unsafe {
                    ctx.device
                        .cmd_fill_buffer(ctx.cmd, ctx.buffer(depths), 0, bytes, u32::MAX);
                }
            });
        // The view's pixel grid shifted so that the region's corner lands at the origin;
        // only the region is rasterized.
        let extent = vk::Extent2D {
            width: x1 - x0,
            height: y1 - y0,
        };
        let area = DrawArea {
            viewport: vk::Rect2D {
                offset: vk::Offset2D {
                    x: -(x0 as i32),
                    y: -(y0 as i32),
                },
                extent: vk::Extent2D {
                    width: view.size.0,
                    height: view.size.1,
                },
            },
            scissor: vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent,
            },
        };
        let base = Push {
            frame,
            instances: scene.current_address,
            visible,
            positions: geometry.position_address(),
            attributes: geometry.attribute_address(),
            materials: materials.address(),
            ..bytemuck::Zeroable::zeroed()
        };
        let raw = |p: &[Pipeline; 2]| [p[0].raw(), p[1].raw()];
        let (runs, count) = self.candidates.runs();
        let geometry_copy = *geometry;
        if request.visible {
            // The candidates' nearest surfaces first, then each keeps its depth where it is
            // the nearest.
            let depth = fg.create_texture(
                "pick list depth",
                TextureDesc::new_2d(DEPTH_FORMAT, extent.width, extent.height),
            );
            let pipelines = raw(&self.depth_pipelines);
            materials
                .read(geometry.read(fg.add_pass("pick list depth")))
                .read(scene.current, Access::StorageRead(ShaderStages::VERTEX))
                .depth(depth, LoadOp::Clear([1.0, 0.0, 0.0, 0.0]), StoreOp::Store)
                .execute(move |ctx| {
                    // SAFETY: recording inside the pass's rendering scope; the pass reads
                    // the pool, the material table and the scene's instances, the draws lie
                    // in the ring's slot for this frame, and the picker outlives the frame.
                    unsafe {
                        draw_groups(
                            ctx,
                            bindless,
                            &geometry_copy,
                            pipelines,
                            &base,
                            &runs[..count],
                            commands,
                            area,
                        );
                    }
                });
            let pipelines = raw(&self.visible_pipelines);
            materials
                .read(geometry.read(fg.add_pass("pick list")))
                .read(scene.current, Access::StorageRead(ShaderStages::VERTEX))
                .write(depths, Access::StorageReadWrite(ShaderStages::FRAGMENT))
                .depth(depth, LoadOp::Load, StoreOp::DontCare)
                .execute(move |ctx| {
                    let push = Push {
                        light_tiles: ctx.address(depths),
                        ..base
                    };
                    // SAFETY: as the depth's pass; it writes only the depths it declared.
                    unsafe {
                        draw_groups(
                            ctx,
                            bindless,
                            &geometry_copy,
                            pipelines,
                            &push,
                            &runs[..count],
                            commands,
                            area,
                        );
                    }
                });
        } else {
            let pipelines = raw(&self.list_pipelines);
            materials
                .read(geometry.read(fg.add_pass("pick list")))
                .read(scene.current, Access::StorageRead(ShaderStages::VERTEX))
                .write(depths, Access::StorageReadWrite(ShaderStages::FRAGMENT))
                .execute(move |ctx| {
                    let push = Push {
                        light_tiles: ctx.address(depths),
                        ..base
                    };
                    // SAFETY: the pass has no attachments and no rendering is begun; it
                    // reads the pool, the material table and the scene's instances, writes
                    // only the depths it declared, the draws lie in the ring's slot for
                    // this frame, and the picker outlives the frame.
                    unsafe {
                        ctx.begin_rendering_without_attachments(extent);
                        draw_groups(
                            ctx,
                            bindless,
                            &geometry_copy,
                            pipelines,
                            &push,
                            &runs[..count],
                            commands,
                            area,
                        );
                        ctx.end_rendering();
                    }
                });
        }
        fg.add_pass("pick list copy")
            .read(depths, Access::TransferRead)
            .write(out, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording outside a rendering scope; both buffers hold `bytes`.
                unsafe {
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        ctx.buffer(depths),
                        ctx.buffer(out),
                        &[vk::BufferCopy {
                            src_offset: 0,
                            dst_offset: 0,
                            size: bytes,
                        }],
                    );
                }
            });
        PickPass::Declared
    }

    /// Frees the readback buffers (the pipelines go when it is dropped).
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        let lists = self.lists.iter_mut().map(|r| (r.buffer, &mut r.allocation));
        let squares = self
            .readbacks
            .iter_mut()
            .map(|r| (r.buffer, &mut r.allocation));
        for (buffer, allocation) in squares.chain(lists) {
            if let Some(allocation) = allocation.take() {
                // SAFETY: guaranteed by the caller; the pair comes from `create_buffer`.
                unsafe { allocator.destroy_buffer(buffer, allocation) };
            }
        }
    }
}

/// A pick's region: the view's pixels `[x0, y0, x1, y1]` from `before` pixels before
/// `(x, y)` to `after` pixels after it in each direction, within a view of `size`.
fn region(x: u32, y: u32, before: u32, after: u32, (width, height): (u32, u32)) -> [u32; 4] {
    [
        x.saturating_sub(before).min(width),
        y.saturating_sub(before).min(height),
        x.saturating_add(after).min(width),
        y.saturating_add(after).min(height),
    ]
}

/// The draw area of a square pick at `request`'s pixel in a view of `size` pixels: the
/// view's pixel grid shifted so that the pixel lands at the square's center; only the
/// view's part of the square is rasterized.
fn square_area(request: PickRequest, size: (u32, u32)) -> DrawArea {
    let [x0, y0, x1, y1] = region(request.x, request.y, CENTER, CENTER, size);
    // The square's origin is the view's pixel (x - CENTER, y - CENTER).
    let (dx, dy) = (
        CENTER as i32 - request.x as i32,
        CENTER as i32 - request.y as i32,
    );
    DrawArea {
        viewport: vk::Rect2D {
            offset: vk::Offset2D { x: dx, y: dy },
            extent: vk::Extent2D {
                width: size.0,
                height: size.1,
            },
        },
        scissor: vk::Rect2D {
            offset: vk::Offset2D {
                x: x0 as i32 + dx,
                y: y0 as i32 + dy,
            },
            extent: vk::Extent2D {
                width: x1.saturating_sub(x0),
                height: y1.saturating_sub(y0),
            },
        },
    }
}

/// Copies a square's ids into a readback buffer.
fn copy_square<'g>(fg: &mut FrameGraph<'g>, ids: crate::graph::Texture, readback: vk::Buffer) {
    let out = fg.import_buffer(
        "pick readback",
        ImportedBuffer {
            buffer: readback,
            size: BYTES,
            address: 0,
            initial: BufferState::idle(),
            final_state: Some(BufferState::host_read()),
        },
    );
    fg.add_pass("pick copy")
        .read(ids, Access::TransferRead)
        .write(out, Access::TransferWrite)
        .execute(move |ctx| {
            // SAFETY: recording outside a rendering scope; the image is in
            // TRANSFER_SRC layout for this pass, the buffer holds a square.
            unsafe {
                ctx.device.cmd_copy_image_to_buffer(
                    ctx.cmd,
                    ctx.image(ids),
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ctx.buffer(out),
                    &[vk::BufferImageCopy {
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_extent: vk::Extent3D {
                            width: SQUARE,
                            height: SQUARE,
                            depth: 1,
                        },
                        ..Default::default()
                    }],
                );
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::{Vec3, look_at, vulkan_perspective};

    fn result(at: &[(u32, u32, u32)]) -> PickResult {
        let mut ids = vec![0; (SQUARE * SQUARE) as usize];
        for &(x, y, id) in at {
            ids[(y * SQUARE + x) as usize] = id;
        }
        PickResult {
            frame: 1,
            request: PickRequest {
                x: 100,
                y: 100,
                transparent: true,
            },
            ids,
        }
    }

    #[test]
    fn the_nearest_id_within_the_radius_wins() {
        // Nothing at the center: the nearest within 3 pixels.
        let r = result(&[(CENTER + 2, CENTER, 8), (CENTER, CENTER + 1, 5)]);
        assert_eq!(r.nearest(3), Some(4), "one pixel away beats two");
        assert_eq!(r.nearest(0), None);
        let r = result(&[(CENTER, CENTER, 1), (CENTER + 1, CENTER, 9)]);
        assert_eq!(r.nearest(3), Some(0), "the pixel itself first");
        let r = result(&[(CENTER + 3, CENTER + 3, 2)]);
        assert_eq!(r.nearest(3), None, "outside the circle");
        assert_eq!(r.nearest(5), Some(1));
    }

    #[test]
    fn region_planes_bound_the_pixels_seen_through_the_camera() {
        // A camera at z = 5 looking down -z, 90° vertically, square pixels of a 200 × 100
        // view.
        let clip_from_world = vulkan_perspective(core::f32::consts::FRAC_PI_2, 2.0, 0.1, 100.0)
            * look_at(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
        let size = (200, 100);
        let inside =
            |planes: &[Vec4; 6], p: Vec3| planes.iter().all(|q| q.dot(p.extend(1.0)) >= 0.0);
        // Where a point lands in the view's pixels.
        let pixel = |p: Vec3| {
            let c = clip_from_world * p.extend(1.0);
            (
                (c.x / c.w * 0.5 + 0.5) * 200.0,
                (c.y / c.w * 0.5 + 0.5) * 100.0,
            )
        };
        let (px, py) = pixel(Vec3::new(1.0, 0.5, 0.0));
        let planes = region_planes(
            &clip_from_world,
            [px - 2.0, py - 2.0, px + 2.0, py + 2.0],
            size,
        );
        assert!(
            inside(&planes, Vec3::new(1.0, 0.5, 0.0)),
            "the point itself"
        );
        // Along the same ray, nearer and farther.
        assert!(inside(&planes, Vec3::new(0.8, 0.4, 1.0)));
        assert!(inside(&planes, Vec3::new(2.0, 1.0, -5.0)));
        // Beside it, behind the camera, beyond the far plane.
        assert!(!inside(&planes, Vec3::new(1.5, 0.5, 0.0)));
        assert!(!inside(&planes, Vec3::new(1.0, 0.0, 0.0)));
        assert!(!inside(&planes, Vec3::new(-1.0, -0.5, 10.0)));
        assert!(!inside(&planes, Vec3::new(40.0, 20.0, -195.0)));
    }
}
