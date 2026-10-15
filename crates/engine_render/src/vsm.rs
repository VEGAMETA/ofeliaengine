//! Virtual shadow maps for the sun (ADR 0093 step 3): levels of 4096 × 4096 virtual
//! texels around the camera, each covering twice the last, in pages of 64 × 64 texels;
//! only the pages the frame's pixels need are backed, by pages of one pool kept across
//! frames. A pixel gets about a texel: the cascades' texels are 2 to 10 pixels near the
//! camera (`docs/research/virtual-shadow-maps.md`).
//!
//! - **Levels.** Level `k` covers [`VsmSettings::first_extent`] × 2^k metres across the
//!   light, its window of 64 × 64 pages snapped to whole pages around the camera. Its page
//!   table is addressed by the page's coordinates modulo 64, so it scrolls with the window:
//!   the rows and columns whose page changed are freed when the window moves.
//! - **A frame** (`vsm.slang`, after the depth prepass): the frame's flags cleared and the
//!   wrapped entries freed; each pixel flags the page its surface needs, at the level
//!   whose texel is about its footprint; when the light turned, the needed pages are
//!   drawn again and the others freed; requests take free pages, then pages no pixel
//!   needed for [`VsmSettings::keep_frames`] frames, and fail when there are none (asked
//!   for again the next frame); dirty pages are cleared to the far plane.
//! - **Step 3a** (this module so far) keeps the pages; drawing into them and shading with
//!   them follow (steps 3b, 3c).
//!
//! The design follows Timberdoodle's virtual shadow maps (Apache-2.0; M. Sakmáry); the
//! code is written for this engine.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::environment::{Image, ImageKind};
use crate::graph::{
    Access, Buffer, BufferState, FrameGraph, ImportedBuffer, ShaderStages, Texture,
};
use crate::upload::UploadRing;
use core::cell::{Cell, RefCell};
use engine_core::math::{IVec2, Mat4, Vec2, Vec3, look_at};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError};

const BEGIN_FRAME: &[u8] = include_bytes!("../shaders/vsm.vsmBeginFrame.spv");
const MARK: &[u8] = include_bytes!("../shaders/vsm.vsmMark.spv");
const FIND_PAGES: &[u8] = include_bytes!("../shaders/vsm.vsmFindPages.spv");
const ALLOCATE: &[u8] = include_bytes!("../shaders/vsm.vsmAllocate.spv");
const COLLECT_DIRTY: &[u8] = include_bytes!("../shaders/vsm.vsmCollectDirty.spv");
const ARGS: &[u8] = include_bytes!("../shaders/vsm.vsmArgs.spv");
const CLEAR: &[u8] = include_bytes!("../shaders/vsm.vsmClear.spv");

/// Texels of a page, per axis.
pub const PAGE_TEXELS: u32 = 64;
/// Pages of a level, per axis (its page table's size).
pub const TABLE_PAGES: u32 = 64;
/// Virtual texels of a level, per axis.
pub const VIRTUAL_TEXELS: u32 = PAGE_TEXELS * TABLE_PAGES;
/// The most levels.
pub const MAX_LEVELS: u32 = 16;
/// Entries of one level's page table.
const ENTRIES: u32 = TABLE_PAGES * TABLE_PAGES;

/// A page table entry is backed by a physical page.
pub const ALLOCATED: u32 = 1 << 31;
/// A pixel needed the page this frame.
pub const NEEDED: u32 = 1 << 30;
/// The page must be drawn this frame (new, or invalidated).
pub const DIRTY: u32 = 1 << 29;
/// A pixel needed the page this frame and the pool had none for it.
pub const FAILED: u32 = 1 << 28;
/// The physical page's index in an entry.
pub const PHYSICAL: u32 = 0xFFFF;
/// A meta entry backs a page table entry.
pub const OWNED: u32 = 1 << 31;
/// Words before the lists in [`VsmFrame::lists`]: the free, old and dirty counts (0 to
/// 2), then the clear pass's dispatch (4 to 6). The free, old and dirty pages follow, room
/// for every page of the pool each.
pub const LIST_HEADER: u32 = 8;
/// The clear pass's dispatch in the lists, bytes.
const CLEAR_ARGS: u64 = 16;
/// Texels around a pixel's sample that its filter and normal offset may read: marking
/// backs the pages within it too (`VSM_MARGIN` in `vsm_common.slang`).
pub const MARK_MARGIN_TEXELS: f32 = 4.0;
/// Cells of a level's hierarchy of dirty pages ([`VsmFrame::hierarchy`]): its 64 × 64
/// pages by their place in the window, then each coarser level of them (2 × 2 cells into
/// one) down to 1 × 1, one `u32` each (1: a dirty page under it).
pub const HIERARCHY_CELLS: u32 = 5461;
/// Levels of a level's hierarchy (64², 32², ..., 1²).
pub const HIERARCHY_MIPS: u32 = 7;

/// The first cell of mip `m` of a level's hierarchy.
pub const fn hierarchy_mip(m: u32) -> u32 {
    (4096 - (4096 >> (2 * m))) * 4 / 3
}

const _: () = assert!(hierarchy_mip(HIERARCHY_MIPS - 1) + 1 == HIERARCHY_CELLS);

/// A virtual shadow map's settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VsmSettings {
    /// Levels (1 to [`MAX_LEVELS`]).
    pub levels: u32,
    /// The first level's width across the light, metres: its texel is this over 4096.
    pub first_extent: f32,
    /// Physical pages in the pool, 16 KiB each (kept in rows of 64 pages; at most 65,472,
    /// and as many rows as the device's largest image holds).
    pub pool_pages: u32,
    /// Frames a page no pixel needs stays backed before its page may be taken.
    pub keep_frames: u32,
    /// Added to each pixel's level before rounding down: +1 halves the texel density.
    pub level_bias: f32,
}

impl Default for VsmSettings {
    /// 16 levels from 2 m (0.5 mm texels) to 65 km across, 4,096 pages (64 MiB), pages
    /// kept 8 frames, a texel about a pixel.
    fn default() -> Self {
        Self {
            levels: MAX_LEVELS,
            first_extent: 2.0,
            pool_pages: 4096,
            keep_frames: 8,
            level_bias: 0.0,
        }
    }
}

/// `Level` in `vsm.slang` (32 bytes).
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct LevelData {
    first_page: [i32; 2],
    page_scale: f32,
    flags: u32,
    wrapped: [u32; 4],
}

/// `VsmFrame` in `vsm_common.slang` (704 bytes).
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct FrameData {
    clip_to_light: [[f32; 4]; 2],
    clip_to_view: [[f32; 4]; 4],
    light_rows: [[f32; 4]; 3],
    ndc_per_pixel: [f32; 2],
    depth_size: [u32; 2],
    level_offset: f32,
    levels: u32,
    frame: u32,
    pool_pages: u32,
    keep_frames: u32,
    max_requests: u32,
    depth_scale: f32,
    depth_offset: f32,
    level: [LevelData; MAX_LEVELS as usize],
}

/// `VsmPush` in `vsm.slang` (56 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    frame: u64,
    table: u64,
    meta: u64,
    requests: u64,
    lists: u64,
    hierarchy: u64,
    pool: u32,
    depth: u32,
}

const _: () = assert!(size_of::<LevelData>() == 32);
const _: () = assert!(size_of::<FrameData>() == 704);
const _: () = assert!(size_of::<Push>() == 56);

/// What a view gives the pages each frame.
#[derive(Debug, Clone, Copy)]
pub struct VsmView {
    /// The camera's inverse view-projection, as the depth buffer was drawn (with its
    /// jitter).
    pub world_from_clip: Mat4,
    /// The camera's position.
    pub eye: Vec3,
    /// Its vertical field of view, radians: with the depth buffer's height, a pixel's
    /// footprint.
    pub fov_y: f32,
    /// The direction the sun's light travels.
    pub light_direction: Vec3,
    /// The scene's extent along the light, metres (the least and greatest
    /// `dot(position, light_direction)` of its casters): the pages' depth range. It is
    /// rounded out to whole steps, so a change within a step keeps the pages.
    pub depth_range: [f32; 2],
}

/// The pages' depth range for a scene's extent along the light, `depth_range`: rounded
/// out to multiples of [`DEPTH_STEP`] metres with a step to spare on each side, so a
/// scene's casters moving a little change nothing (a change redraws every page).
pub fn depth_range(extent: [f32; 2]) -> [f32; 2] {
    let lo = (extent[0].min(extent[1]) / DEPTH_STEP).floor() * DEPTH_STEP - DEPTH_STEP;
    let hi = (extent[0].max(extent[1]) / DEPTH_STEP).ceil() * DEPTH_STEP + DEPTH_STEP;
    [lo, hi]
}

/// What [`depth_range`] rounds to, metres.
pub const DEPTH_STEP: f32 = 16.0;

/// A level's window: the pages its table holds this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VsmLevel {
    /// The window's first page (light space, in pages of this level): the table holds
    /// pages `first_page` to `first_page + 63` per axis, page `p` at `p mod 64`.
    pub first_page: IVec2,
    /// A texel's width, metres.
    pub texel: f32,
}

/// The pages in one frame's graph ([`VirtualShadows::add_passes`]): for the passes that
/// draw into them and read them.
#[derive(Debug, Clone, Copy)]
pub struct VsmFrame {
    /// The frame's data (light space, levels) in the upload ring (`Frame` in
    /// `vsm.slang`).
    pub frame: vk::DeviceAddress,
    /// The pool of physical pages (`R32_UINT`: depths as their bits; far is 1.0).
    pub pool: Texture,
    /// The page tables: [`VsmSettings::levels`] × 64 × 64 entries.
    pub table: Buffer,
    /// Per physical page: the entry it backs and the last frame a pixel needed it.
    pub meta: Buffer,
    /// The frame's allocation requests: a count, then entries.
    pub requests: Buffer,
    /// The free, old and dirty lists, after their header of [`LIST_HEADER`] words.
    pub lists: Buffer,
    /// Per level, [`HIERARCHY_CELLS`] cells: which pages of its window are dirty this
    /// frame, and each coarser level of that (the casters' cull).
    pub hierarchy: Buffer,
}

/// A buffer of the pages', kept across frames.
#[derive(Debug)]
struct Persistent {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    size: u64,
    state: Cell<BufferState>,
}

impl Persistent {
    fn new(device: &Device, allocator: &mut Allocator, size: u64) -> Result<Self, RhiError> {
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(size).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                    | vk::BufferUsageFlags::TRANSFER_DST
                    | vk::BufferUsageFlags::TRANSFER_SRC
                    | vk::BufferUsageFlags::INDIRECT_BUFFER,
            ),
            MemoryLocation::Device,
        )?;
        // SAFETY: created with the device address usage.
        let address = unsafe {
            device
                .raw()
                .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
        };
        Ok(Self {
            buffer,
            allocation: Some(allocation),
            address,
            size,
            state: Cell::new(BufferState::idle()),
        })
    }

    /// Imports it into `fg`, left as a compute shader read and wrote it.
    fn import(&self, fg: &mut FrameGraph<'_>, name: &'static str) -> Buffer {
        let final_state = BufferState {
            stages: vk::PipelineStageFlags2::COMPUTE_SHADER
                | vk::PipelineStageFlags2::DRAW_INDIRECT,
            access: vk::AccessFlags2::SHADER_STORAGE_READ
                | vk::AccessFlags2::SHADER_STORAGE_WRITE
                | vk::AccessFlags2::INDIRECT_COMMAND_READ,
        };
        let buffer = fg.import_buffer(
            name,
            ImportedBuffer {
                buffer: self.buffer,
                size: self.size,
                address: self.address,
                initial: self.state.get(),
                final_state: Some(final_state),
            },
        );
        self.state.set(final_state);
        buffer
    }

    /// # Safety
    ///
    /// The GPU no longer uses the buffer.
    unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        if let Some(allocation) = self.allocation.take() {
            // SAFETY: guaranteed by the caller.
            unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        }
    }
}

/// What the pages remember between frames, on the CPU.
#[derive(Debug, Default)]
struct State {
    /// The tables still hold nothing (filled with zeros by the first frame).
    reset: bool,
    /// Each level's window: its first page, as drawn last frame.
    first_pages: [Option<IVec2>; MAX_LEVELS as usize],
    /// The light's direction and the pages' depth range, as their bits.
    light: Option<[u32; 5]>,
    /// Frames so far.
    frame: u32,
}

/// The windows of every level for a camera at `eye` (light space), and the masks of the
/// rows and columns each window's move frees: see the module documentation.
fn windows(
    settings: &VsmSettings,
    eye_light: Vec2,
    previous: &[Option<IVec2>; MAX_LEVELS as usize],
) -> [LevelData; MAX_LEVELS as usize] {
    let mut levels = [LevelData::default(); MAX_LEVELS as usize];
    for (k, level) in levels.iter_mut().enumerate().take(settings.levels as usize) {
        let page = settings.first_extent * (k as f32).exp2() / TABLE_PAGES as f32;
        let center = (eye_light / page).floor();
        let half = (TABLE_PAGES / 2) as i32;
        let first = IVec2::new(center.x as i32 - half, center.y as i32 - half);
        let (columns, rows) = match previous[k] {
            Some(old) => (wrapped_mask(old.x, first.x), wrapped_mask(old.y, first.y)),
            None => (u64::MAX, u64::MAX),
        };
        *level = LevelData {
            first_page: first.to_array(),
            page_scale: page.recip(),
            flags: 0,
            wrapped: [
                columns as u32,
                (columns >> 32) as u32,
                rows as u32,
                (rows >> 32) as u32,
            ],
        };
    }
    levels
}

/// Which of the 64 table positions along one axis change page when the window's first
/// page moves from `old` to `new`.
fn wrapped_mask(old: i32, new: i32) -> u64 {
    let moved = i64::from(new) - i64::from(old);
    if moved.unsigned_abs() >= u64::from(TABLE_PAGES) {
        return u64::MAX;
    }
    let (from, to) = if moved > 0 {
        (
            i64::from(old) + i64::from(TABLE_PAGES),
            i64::from(new) + i64::from(TABLE_PAGES),
        )
    } else {
        (i64::from(new), i64::from(old))
    };
    (from..to).fold(0u64, |mask, page| {
        mask | 1 << (page & (i64::from(TABLE_PAGES) - 1))
    })
}

/// A virtual shadow map's pages (see the module documentation).
pub struct VirtualShadows {
    device: engine_rhi::ash::Device,
    settings: VsmSettings,
    begin_frame: ComputeShader<Push>,
    mark: ComputeShader<Push>,
    find_pages: ComputeShader<Push>,
    allocate: ComputeShader<Push>,
    collect_dirty: ComputeShader<Push>,
    args: ComputeShader<Push>,
    clear: ComputeShader<Push>,
    pool: Image,
    table: Persistent,
    meta: Persistent,
    requests: Persistent,
    lists: Persistent,
    hierarchy: Persistent,
    state: RefCell<State>,
}

impl core::fmt::Debug for VirtualShadows {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtualShadows")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl VirtualShadows {
    /// Makes the pool, the tables and the passes (`settings` clamped: 1 to 16 levels, the
    /// pool at most 65,472 pages and as tall as the device's images).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        settings: VsmSettings,
    ) -> Result<Self, RhiError> {
        let settings = VsmSettings {
            levels: settings.levels.clamp(1, MAX_LEVELS),
            first_extent: settings.first_extent.max(1e-3),
            // The pool image's rows, and fewer pages than a dispatch's groups.
            pool_pages: settings.pool_pages.clamp(
                1,
                (device.adapter().limits.max_image_dimension2_d / PAGE_TEXELS)
                    .min(PHYSICAL / TABLE_PAGES)
                    * TABLE_PAGES,
            ),
            ..settings
        };
        let [
            begin_frame,
            mark,
            find_pages,
            allocate,
            collect_dirty,
            args,
            clear,
        ] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(BEGIN_FRAME, c"vsmBeginFrame", &[])?,
                ComputeSource::new(MARK, c"vsmMark", &[])?,
                ComputeSource::new(FIND_PAGES, c"vsmFindPages", &[])?,
                ComputeSource::new(ALLOCATE, c"vsmAllocate", &[])?,
                ComputeSource::new(COLLECT_DIRTY, c"vsmCollectDirty", &[])?,
                ComputeSource::new(ARGS, c"vsmArgs", &[])?,
                ComputeSource::new(CLEAR, c"vsmClear", &[])?,
            ],
        )?;
        let rows = settings.pool_pages.div_ceil(TABLE_PAGES);
        let pool = Image::new(
            device,
            allocator,
            bindless,
            vk::Format::R32_UINT,
            TABLE_PAGES * PAGE_TEXELS,
            rows * PAGE_TEXELS,
            1,
            ImageKind::Flat,
        )?;
        let entries = u64::from(settings.levels * ENTRIES);
        let table = Persistent::new(device, allocator, entries * 4)?;
        let meta = Persistent::new(device, allocator, u64::from(settings.pool_pages) * 8)?;
        let requests = Persistent::new(device, allocator, (1 + entries) * 4)?;
        let lists = Persistent::new(
            device,
            allocator,
            u64::from(LIST_HEADER + 3 * settings.pool_pages) * 4,
        )?;
        let hierarchy = Persistent::new(
            device,
            allocator,
            u64::from(settings.levels * HIERARCHY_CELLS) * 4,
        )?;
        Ok(Self {
            device: device.raw().clone(),
            settings,
            begin_frame,
            mark,
            find_pages,
            allocate,
            collect_dirty,
            args,
            clear,
            pool,
            table,
            meta,
            requests,
            lists,
            hierarchy,
            state: RefCell::new(State {
                reset: true,
                ..State::default()
            }),
        })
    }

    /// The settings (as clamped when made).
    pub fn settings(&self) -> VsmSettings {
        self.settings
    }

    /// Level `k`'s window as the last frame declared it (`None` before the first frame,
    /// or past the levels in use).
    pub fn level(&self, k: u32) -> Option<VsmLevel> {
        let first_page = (*self.state.borrow().first_pages.get(k as usize)?)?;
        Some(VsmLevel {
            first_page,
            texel: self.settings.first_extent * (k as f32).exp2() / VIRTUAL_TEXELS as f32,
        })
    }

    /// GPU memory the pages keep: the pool (whole rows of 64 pages) and the tables, bytes.
    pub fn memory_bytes(&self) -> u64 {
        let pages = self.settings.pool_pages.next_multiple_of(TABLE_PAGES);
        let pool = u64::from(pages) * u64::from(PAGE_TEXELS * PAGE_TEXELS) * 4;
        pool + self.table.size
            + self.meta.size
            + self.requests.size
            + self.lists.size
            + self.hierarchy.size
    }

    /// The page tables' address, for the passes that draw into the pages and read them.
    pub fn table_address(&self) -> vk::DeviceAddress {
        self.table.address
    }

    /// The hierarchy of dirty pages' address ([`VsmFrame::hierarchy`]).
    pub fn hierarchy_address(&self) -> vk::DeviceAddress {
        self.hierarchy.address
    }

    /// The pool's index in the bindless storage images.
    pub fn pool_index(&self) -> u32 {
        self.pool.storage[0]
    }

    /// The light space of `direction`, the way the light travels: x and y across the
    /// light (the levels' texels), z against it (a view's).
    pub fn light_from_world(direction: Vec3) -> Mat4 {
        let direction = direction.normalize_or(Vec3::NEG_Y);
        let up = if direction.y.abs() > 0.99 {
            Vec3::Z
        } else {
            Vec3::Y
        };
        look_at(Vec3::ZERO, direction, up)
    }

    /// Declares this frame's bookkeeping (see the module documentation) over `depth`, the
    /// view's depth after its prepass (`depth_size` pixels; single-sampled). `None` when
    /// `ring` is full (the pages then stay as they were).
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        depth: Texture,
        depth_size: [u32; 2],
        view: &VsmView,
    ) -> Option<VsmFrame> {
        engine_profiler::zone!("render::VirtualShadows::add_passes");
        let settings = self.settings;
        let mut state = self.state.borrow_mut();
        let light_from_world = Self::light_from_world(view.light_direction);
        let eye_light = light_from_world.transform_point3(view.eye);
        let mut levels = windows(
            &settings,
            Vec2::new(eye_light.x, eye_light.y),
            &state.first_pages,
        );
        let direction = view.light_direction.normalize_or(Vec3::NEG_Y);
        let range = depth_range(view.depth_range);
        let light_key = [
            direction.x.to_bits(),
            direction.y.to_bits(),
            direction.z.to_bits(),
            range[0].to_bits(),
            range[1].to_bits(),
        ];
        let turned = state.light.is_some_and(|key| key != light_key);
        for level in levels.iter_mut().take(settings.levels as usize) {
            level.flags = u32::from(turned);
        }
        let number = state.frame.wrapping_add(1);
        // A pixel's footprint at a metre, over the first level's texel.
        let footprint = 2.0 * (view.fov_y * 0.5).tan() / depth_size[1].max(1) as f32;
        let first_texel = settings.first_extent / VIRTUAL_TEXELS as f32;
        let to_light = light_from_world * view.world_from_clip;
        let to_view = Mat4::from_translation(-view.eye) * view.world_from_clip;
        // A point's depth in the pages: its distance along the light (−z in light space)
        // across the range.
        let depth_scale = 1.0 / (range[1] - range[0]).max(1e-3);
        let frame = ring.push(&[FrameData {
            clip_to_light: [0, 1].map(|i| to_light.row(i).to_array()),
            clip_to_view: [0, 1, 2, 3].map(|i| to_view.row(i).to_array()),
            light_rows: [0, 1, 2].map(|i| light_from_world.row(i).to_array()),
            ndc_per_pixel: depth_size.map(|s| 2.0 / s.max(1) as f32),
            depth_size,
            level_offset: (footprint / first_texel).log2() + settings.level_bias,
            levels: settings.levels,
            frame: number,
            pool_pages: settings.pool_pages,
            keep_frames: settings.keep_frames,
            max_requests: settings.levels * ENTRIES,
            depth_scale,
            depth_offset: -range[0] * depth_scale,
            level: levels,
        }])?;
        // The frame is declared: what it moved is remembered.
        let reset = core::mem::take(&mut state.reset);
        state.frame = number;
        for (k, level) in levels.iter().enumerate().take(settings.levels as usize) {
            state.first_pages[k] = Some(IVec2::from(level.first_page));
        }
        state.light = Some(light_key);
        drop(state);

        let pool = self
            .pool
            .import_storage(fg, "vsm pool", ShaderStages::COMPUTE);
        let table = self.table.import(fg, "vsm page tables");
        let meta = self.meta.import(fg, "vsm meta");
        let requests = self.requests.import(fg, "vsm requests");
        let lists = self.lists.import(fg, "vsm lists");
        let hierarchy = self.hierarchy.import(fg, "vsm hierarchy");
        if reset {
            fg.add_pass("vsm reset")
                .write(table, Access::TransferWrite)
                .write(meta, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope.
                    unsafe {
                        ctx.device.cmd_fill_buffer(
                            ctx.cmd,
                            ctx.buffer(table),
                            0,
                            vk::WHOLE_SIZE,
                            0,
                        );
                        ctx.device
                            .cmd_fill_buffer(ctx.cmd, ctx.buffer(meta), 0, vk::WHOLE_SIZE, 0);
                    }
                });
        }
        let rw = Access::StorageReadWrite(ShaderStages::COMPUTE);
        let entries = settings.levels * ENTRIES;
        fg.add_compute("vsm pages", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .write(pool, Access::StorageWrite(ShaderStages::COMPUTE))
            .write(table, rw)
            .write(meta, rw)
            .write(requests, rw)
            .write(lists, rw)
            .write(hierarchy, rw)
            .read(lists, Access::Indirect)
            .record(move |rec| {
                let ctx = rec.context();
                let (Some(depth), Some(pool)) = (ctx.sampled_index(depth), ctx.storage_index(pool))
                else {
                    return;
                };
                let push = Push {
                    frame,
                    table: ctx.address(table),
                    meta: ctx.address(meta),
                    requests: ctx.address(requests),
                    lists: ctx.address(lists),
                    hierarchy: ctx.address(hierarchy),
                    pool,
                    depth,
                };
                let args = ctx.buffer(lists);
                rec.dispatch(&self.begin_frame, Dispatch::Threads([entries, 1, 1]), &push);
                rec.barrier();
                rec.dispatch(
                    &self.mark,
                    Dispatch::Threads([depth_size[0], depth_size[1], 1]),
                    &push,
                );
                rec.barrier();
                rec.dispatch(
                    &self.find_pages,
                    Dispatch::Threads([settings.pool_pages, 1, 1]),
                    &push,
                );
                rec.barrier();
                rec.dispatch(&self.allocate, Dispatch::Threads([entries, 1, 1]), &push);
                rec.barrier();
                rec.dispatch(
                    &self.collect_dirty,
                    Dispatch::Threads([entries, 1, 1]),
                    &push,
                );
                rec.barrier();
                rec.dispatch(&self.args, Dispatch::Groups([1, 1, 1]), &push);
                rec.barrier_for_indirect();
                rec.dispatch_indirect(&self.clear, args, CLEAR_ARGS, &push);
            });
        Some(VsmFrame {
            frame,
            pool,
            table,
            meta,
            requests,
            lists,
            hierarchy,
        })
    }

    /// Frees the pool and the tables.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.pool.destroy(&self.device, allocator, bindless, 0);
            self.table.destroy(allocator);
            self.meta.destroy(allocator);
            self.requests.destroy(allocator);
            self.lists.destroy(allocator);
            self.hierarchy.destroy(allocator);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_moving_frees_the_rows_and_columns_it_reuses() {
        // Still: nothing.
        assert_eq!(wrapped_mask(10, 10), 0);
        // One page right: the column of the page entering (old + 64), which is the one
        // leaving (old), modulo 64.
        assert_eq!(wrapped_mask(10, 11), 1 << 10);
        assert_eq!(wrapped_mask(10, 9), 1 << 9);
        // Across the table's edge, negative pages included.
        assert_eq!(wrapped_mask(62, 66), (1 << 62) | (1 << 63) | 1 | (1 << 1));
        assert_eq!(wrapped_mask(-1, -3), (1 << 61) | (1 << 62));
        // A jump of a whole window or more: everything.
        assert_eq!(wrapped_mask(0, 64), u64::MAX);
        assert_eq!(wrapped_mask(0, -200), u64::MAX);
    }

    #[test]
    fn levels_center_their_windows_on_the_camera() {
        let settings = VsmSettings::default();
        let none = [None; MAX_LEVELS as usize];
        let levels = windows(&settings, Vec2::new(3.0, -5.0), &none);
        for (k, level) in levels.iter().enumerate() {
            let page = level.page_scale.recip();
            assert!((page - 2.0 * (k as f32).exp2() / 64.0).abs() <= page * 1e-6);
            // The camera's page sits in the middle of the window.
            let camera_page = IVec2::new((3.0 / page).floor() as i32, (-5.0 / page).floor() as i32);
            assert_eq!(
                camera_page - IVec2::from(level.first_page),
                IVec2::splat(32)
            );
            // A first frame frees everything.
            assert_eq!(level.wrapped, [u32::MAX; 4]);
        }
        // The camera moving 1 cm moves the first level (3 cm pages) by a page at most.
        let previous = levels.map(|l| Some(IVec2::from(l.first_page)));
        let moved = windows(&settings, Vec2::new(3.01, -5.0), &previous);
        let column_bits = |l: &LevelData| l.wrapped[0].count_ones() + l.wrapped[1].count_ones();
        assert!(column_bits(&moved[0]) <= 1);
        assert_eq!(moved[15].wrapped, [0; 4]);
    }
}
