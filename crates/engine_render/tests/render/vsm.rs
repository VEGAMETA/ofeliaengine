//! Virtual shadow map pages (`engine_render::vsm`, ADR 0093 step 3a) held to their rules.
//! Each frame uploads the depth buffer of a camera over a ground plane, runs the pages'
//! bookkeeping and reads everything back; a checker then holds the frame's change, from
//! the tables the frame before left, to the rules:
//! - the pages the pixels mark, against the same math in f64 (a pixel within 1 % of a
//!   page's or a level's edge may land on either side);
//! - wrapped entries freed; one request per needed page without one; the free and old
//!   lists; free pages given first, then old ones, then failures;
//! - entries keep their page while their window holds them, pages are dirty when new;
//!   when the light turned, the needed pages are dirty and the others freed; dirty pages
//!   read the far plane after the clear.
//!
//! On top: a still frame allocates nothing, moving frees what wraps, and a short pool
//! fails, then takes the pages no pixel needed for `keep_frames` frames.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::collections::{HashMap, HashSet};

use engine_core::math::{Mat4, Vec3, Vec4, look_at, vulkan_perspective};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::upload::UploadRing;
use engine_render::vsm::{
    ALLOCATED, DIRTY, FAILED, HIERARCHY_CELLS, HIERARCHY_MIPS, LIST_HEADER, MARK_MARGIN_TEXELS,
    NEEDED, OWNED, PAGE_TEXELS, PHYSICAL, TABLE_PAGES, VirtualShadows, VsmLevel, VsmSettings,
    VsmView, hierarchy_mip,
};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation,
};

/// Entries of a level's page table.
const ENTRIES: usize = (TABLE_PAGES * TABLE_PAGES) as usize;
/// The far plane's depth, as the pool stores it.
const FAR_DEPTH: u32 = 0x3F80_0000;
/// How close to a page's or a level's edge a pixel may land on either side (pages,
/// levels).
const EDGE: f64 = 0.01;
const FOV_Y: f32 = core::f32::consts::FRAC_PI_3;
const NEAR: f32 = 0.1;
const FAR: f32 = 200.0;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render vsm test"),
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

/// A camera over the ground: at `eye`, turned `yaw` from −Z towards +X, tilted `pitch`
/// up.
#[derive(Debug, Clone, Copy)]
struct Camera {
    eye: Vec3,
    yaw: f32,
    pitch: f32,
}

impl Camera {
    fn clip_from_world(&self, size: [u32; 2]) -> Mat4 {
        let forward = Vec3::new(
            self.yaw.sin() * self.pitch.cos(),
            self.pitch.sin(),
            -self.yaw.cos() * self.pitch.cos(),
        );
        vulkan_perspective(FOV_Y, size[0] as f32 / size[1] as f32, NEAR, FAR)
            * look_at(self.eye, self.eye + forward, Vec3::Y)
    }

    fn view(&self, size: [u32; 2], light_direction: Vec3) -> VsmView {
        VsmView {
            world_from_clip: self.clip_from_world(size).inverse(),
            eye: self.eye,
            fov_y: FOV_Y,
            light_direction,
            depth_range: [-100.0, 100.0],
        }
    }

    /// The depth buffer of the ground plane (y = 0) where `ground` holds, the sky (1.0)
    /// elsewhere.
    fn depth(&self, size: [u32; 2], ground: impl Fn(Vec3) -> bool) -> Vec<f32> {
        let clip_from_world = self.clip_from_world(size);
        let world_from_clip = clip_from_world.inverse();
        let [w, h] = size;
        let mut depth = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                let ndc = [
                    (x as f32 + 0.5) / w as f32 * 2.0 - 1.0,
                    (y as f32 + 0.5) / h as f32 * 2.0 - 1.0,
                ];
                let far = world_from_clip * Vec4::new(ndc[0], ndc[1], 1.0, 1.0);
                let ray = far.truncate() / far.w - self.eye;
                let mut d = 1.0;
                if ray.y < 0.0 {
                    let p = self.eye + ray * (-self.eye.y / ray.y);
                    let clip = clip_from_world * p.extend(1.0);
                    let z = clip.z / clip.w;
                    if ground(p) && (0.0..1.0).contains(&z) {
                        d = z;
                    }
                }
                depth.push(d);
            }
        }
        depth
    }
}

/// What a frame left on the GPU.
#[derive(Debug, Clone)]
struct Snapshot {
    table: Vec<u32>,
    meta: Vec<[u32; 2]>,
    requests: Vec<u32>,
    lists: Vec<u32>,
    /// The pool's texels, rows of 64 pages.
    pool: Vec<u32>,
    /// Per level, its hierarchy of dirty pages.
    hierarchy: Vec<u32>,
}

impl Snapshot {
    /// Before the first frame: nothing backed.
    fn empty(settings: &VsmSettings) -> Self {
        Self {
            table: vec![0; settings.levels as usize * ENTRIES],
            meta: vec![[0; 2]; settings.pool_pages as usize],
            requests: Vec::new(),
            lists: Vec::new(),
            pool: Vec::new(),
            hierarchy: Vec::new(),
        }
    }
}

/// One frame's inputs, as the checker needs them.
struct Input<'a> {
    /// The frame's number (the first is 1).
    frame: u32,
    levels: &'a [VsmLevel],
    /// The levels of the frame before (`None` before the first).
    previous: Option<&'a [VsmLevel]>,
    /// The light's direction changed: the pages were drawn for another light.
    turned: bool,
}

/// What happened in a frame.
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    needed: usize,
    requests: usize,
    allocated: usize,
    failed: usize,
    evicted: usize,
    /// Backed entries freed by their window's move.
    wrapped: usize,
    /// Backed entries freed because the light turned and no pixel needed them.
    stale: usize,
    free: usize,
    old: usize,
    dirty: usize,
    backed: usize,
}

/// The entries a pixel at `light` (metres across the light) marks: at the level `k`
/// (fractional) asks for, or the first coarser one whose window holds its footprint, the
/// pages within the margin of it. `shift` moves the position by fractions of a page.
fn entries(levels: &[VsmLevel], k: f64, light: [f64; 2], shift: [f64; 2]) -> Vec<usize> {
    let count = levels.len();
    let first = k.floor().clamp(0.0, (count - 1) as f64) as usize;
    let page_texels = f64::from(PAGE_TEXELS);
    let margin = f64::from(MARK_MARGIN_TEXELS);
    let table = i64::from(TABLE_PAGES);
    for (level, l) in levels.iter().enumerate().skip(first) {
        let texel = f64::from(l.texel);
        let t = [0, 1].map(|i| light[i] / texel + shift[i] * page_texels);
        let page = |v: f64| (v / page_texels).floor() as i64;
        let lo = [page(t[0] - margin), page(t[1] - margin)];
        let hi = [page(t[0] + margin), page(t[1] + margin)];
        let first_page = [i64::from(l.first_page.x), i64::from(l.first_page.y)];
        let inside = |p: [i64; 2]| (0..2).all(|i| (0..table).contains(&(p[i] - first_page[i])));
        if !inside(lo) || !inside(hi) {
            continue;
        }
        let mut out = Vec::with_capacity(4);
        for py in lo[1]..=hi[1] {
            for px in lo[0]..=hi[0] {
                out.push(
                    level * ENTRIES
                        + (py.rem_euclid(table) * table + px.rem_euclid(table)) as usize,
                );
            }
        }
        return out;
    }
    Vec::new()
}

/// The entries the pixels must mark (`.0`: every pixel clear of an edge) and may mark
/// (`.1`).
fn marks(
    depth: &[f32],
    size: [u32; 2],
    view: &VsmView,
    levels: &[VsmLevel],
    settings: &VsmSettings,
) -> (HashSet<usize>, HashSet<usize>) {
    let m = view.world_from_clip.to_cols_array().map(f64::from);
    let light = VirtualShadows::light_from_world(view.light_direction);
    let rows = [0, 1].map(|i| light.row(i).to_array().map(f64::from));
    let footprint = 2.0 * (view.fov_y * 0.5).tan() / size[1] as f32;
    let first_texel = settings.first_extent / (PAGE_TEXELS * TABLE_PAGES) as f32;
    let bias = f64::from((footprint / first_texel).log2()) + f64::from(settings.level_bias);
    let eye = view.eye.to_array().map(f64::from);
    let (mut required, mut allowed) = (HashSet::new(), HashSet::new());
    for y in 0..size[1] {
        for x in 0..size[0] {
            let d = depth[(y * size[0] + x) as usize];
            if d >= 1.0 {
                continue;
            }
            let ndc = [
                f64::from((x as f32 + 0.5) / size[0] as f32 * 2.0 - 1.0),
                f64::from((y as f32 + 0.5) / size[1] as f32 * 2.0 - 1.0),
            ];
            let v = [ndc[0], ndc[1], f64::from(d), 1.0];
            let h = |r: usize| (0..4).map(|c| m[c * 4 + r] * v[c]).sum::<f64>();
            let world = [h(0) / h(3), h(1) / h(3), h(2) / h(3), 1.0];
            let at = rows.map(|row| (0..4).map(|c| row[c] * world[c]).sum::<f64>());
            let distance = (0..3)
                .map(|i| (world[i] - eye[i]).powi(2))
                .sum::<f64>()
                .sqrt();
            let k = distance.max(1e-4).log2() + bias;
            let primary = entries(levels, k, at, [0.0; 2]);
            let mut clear = true;
            for dk in [-EDGE, 0.0, EDGE] {
                for sx in [-EDGE, 0.0, EDGE] {
                    for sy in [-EDGE, 0.0, EDGE] {
                        let e = entries(levels, k + dk, at, [sx, sy]);
                        clear &= e == primary;
                        allowed.extend(e);
                    }
                }
            }
            if clear {
                required.extend(primary.iter().copied());
            }
        }
    }
    (required, allowed)
}

/// The set of `words`, which must hold no value twice.
fn set(words: &[u32]) -> HashSet<usize> {
    let set: HashSet<usize> = words.iter().map(|&w| w as usize).collect();
    assert_eq!(set.len(), words.len(), "a list holds a value twice");
    set
}

/// Holds the change from `prev` to `now` to the pages' rules (see the module
/// documentation); `needed` is checked against the pixels by the caller.
fn check(prev: &Snapshot, now: &Snapshot, input: &Input<'_>, settings: &VsmSettings) -> Stats {
    let entries = settings.levels as usize * ENTRIES;
    let pool = settings.pool_pages as usize;
    let header = LIST_HEADER as usize;
    let physical = |t: u32| (t & PHYSICAL) as usize;
    // An entry whose table position holds another page than before (all on the first
    // frame): the window's page at a position, for its first page `first`, is the one in
    // [first, first + 64) congruent to it.
    let wrapped = |e: usize| -> bool {
        let Some(previous) = input.previous else {
            return true;
        };
        let (level, local) = (e / ENTRIES, e % ENTRIES);
        let at = [local % TABLE_PAGES as usize, local / TABLE_PAGES as usize];
        let page = |first: i32, at: usize| first + (at as i32 - first).rem_euclid(64);
        let (old, new) = (previous[level].first_page, input.levels[level].first_page);
        page(old.x, at[0]) != page(new.x, at[0]) || page(old.y, at[1]) != page(new.y, at[1])
    };
    let mut stats = Stats::default();
    let mut surviving: HashMap<usize, usize> = HashMap::new();
    for e in 0..entries {
        let t = prev.table[e];
        if t & ALLOCATED != 0 {
            if wrapped(e) {
                stats.wrapped += 1;
            } else {
                surviving.insert(e, physical(t));
            }
        }
    }
    let needed: HashSet<usize> = (0..entries)
        .filter(|&e| now.table[e] & NEEDED != 0)
        .collect();
    let requests: HashSet<usize> = needed
        .iter()
        .copied()
        .filter(|e| !surviving.contains_key(e))
        .collect();
    // When the light turned, the kept entries no pixel needs are freed.
    if input.turned {
        let before = surviving.len();
        surviving.retain(|e, _| needed.contains(e));
        stats.stale = before - surviving.len();
    }
    let owned: HashSet<usize> = surviving.values().copied().collect();
    let free: HashSet<usize> = (0..pool).filter(|p| !owned.contains(p)).collect();
    let old: HashMap<usize, usize> = surviving
        .iter()
        .filter(|&(e, &p)| {
            !needed.contains(e) && input.frame.wrapping_sub(prev.meta[p][1]) > settings.keep_frames
        })
        .map(|(&e, &p)| (e, p))
        .collect();
    let old_pages: HashSet<usize> = old.values().copied().collect();

    // The lists and requests the frame built.
    assert_eq!(now.lists[0] as usize, free.len(), "free pages");
    assert_eq!(
        set(&now.lists[header..header + free.len()]),
        free,
        "the free list"
    );
    assert_eq!(now.lists[1] as usize, old.len(), "old pages");
    let old_list = &now.lists[header + pool..header + pool + old.len()];
    assert_eq!(set(old_list), old_pages, "the old list");
    assert_eq!(now.requests[0] as usize, requests.len(), "requests");
    assert_eq!(
        set(&now.requests[1..1 + requests.len()]),
        requests,
        "the requests"
    );

    // Requests: free pages, then old ones, then failures.
    let granted = requests.len().min(free.len() + old.len());
    let mut new_pages = HashSet::new();
    for &e in &requests {
        let t = now.table[e];
        if t & ALLOCATED != 0 {
            assert_eq!(
                t & !PHYSICAL,
                ALLOCATED | DIRTY | NEEDED,
                "entry {e}: a new page"
            );
            assert!(
                physical(t) < pool,
                "entry {e}: page {} past the pool",
                physical(t)
            );
            assert!(
                new_pages.insert(physical(t)),
                "page {} given twice",
                physical(t)
            );
            stats.allocated += 1;
        } else {
            assert_eq!(t, NEEDED | FAILED, "entry {e}: a request without a page");
            stats.failed += 1;
        }
    }
    assert_eq!(stats.allocated, granted, "pages given");
    let turned = if input.turned { DIRTY } else { 0 };
    let mut evicted_pages = HashSet::new();
    for (&e, &p) in &old {
        let t = now.table[e];
        if t == 0 {
            evicted_pages.insert(p);
        } else {
            assert_eq!(t, ALLOCATED | p as u32 | turned, "entry {e}: old and kept");
        }
    }
    stats.evicted = evicted_pages.len();
    assert_eq!(
        stats.evicted,
        granted.saturating_sub(free.len()),
        "old pages taken"
    );
    for p in &new_pages {
        assert!(
            free.contains(p) || evicted_pages.contains(p),
            "page {p} was not free"
        );
    }
    assert!(
        evicted_pages.is_subset(&new_pages),
        "an old page taken and not given"
    );

    // Kept entries: their page, this frame's flags; nothing else is backed.
    for (&e, &p) in surviving.iter().filter(|(e, _)| !old.contains_key(*e)) {
        let flag = if needed.contains(&e) { NEEDED } else { 0 };
        assert_eq!(
            now.table[e],
            ALLOCATED | p as u32 | flag | turned,
            "entry {e}: kept"
        );
    }
    for e in (0..entries).filter(|e| !surviving.contains_key(e) && !requests.contains(e)) {
        assert_eq!(now.table[e], 0, "entry {e}: neither kept nor asked for");
    }

    // Each backed entry's page names it; the others own nothing.
    let mut backed_pages = HashSet::new();
    for e in 0..entries {
        let t = now.table[e];
        if t & ALLOCATED == 0 {
            continue;
        }
        let p = physical(t);
        assert!(backed_pages.insert(p), "two entries on page {p}");
        let last = if t & NEEDED != 0 {
            input.frame
        } else {
            prev.meta[p][1]
        };
        assert_eq!(now.meta[p], [OWNED | e as u32, last], "page {p}'s meta");
    }
    stats.backed = backed_pages.len();
    for p in (0..pool).filter(|p| !backed_pages.contains(p)) {
        assert_eq!(
            now.meta[p][0] & OWNED,
            0,
            "page {p} owned by nothing backed"
        );
    }

    // Dirty pages: the new ones, or every page when the light turned; cleared.
    let dirty: HashSet<usize> = (0..entries)
        .map(|e| now.table[e])
        .filter(|&t| t & (ALLOCATED | DIRTY) == ALLOCATED | DIRTY)
        .map(physical)
        .collect();
    let expected = if input.turned {
        &backed_pages
    } else {
        &new_pages
    };
    assert_eq!(&dirty, expected, "dirty pages");
    assert_eq!(now.lists[2] as usize, dirty.len(), "dirty count");
    let dirty_list = &now.lists[header + 2 * pool..header + 2 * pool + dirty.len()];
    assert_eq!(set(dirty_list), dirty, "the dirty list");
    assert_eq!(
        now.lists[4..7],
        [dirty.len() as u32, 1, 1],
        "the clear's dispatch"
    );
    let width = (TABLE_PAGES * PAGE_TEXELS) as usize;
    let page = PAGE_TEXELS as usize;
    for &p in &dirty {
        let origin = (p / 64) * page * width + (p % 64) * page;
        for y in 0..page {
            let row = &now.pool[origin + y * width..origin + y * width + page];
            assert!(
                row.iter().all(|&t| t == FAR_DEPTH),
                "page {p}, row {y}: not cleared"
            );
        }
    }

    // The hierarchy of dirty pages: by window position, then each coarser level the OR of
    // its 2 × 2 cells.
    let side = TABLE_PAGES as usize;
    for (k, level) in input.levels.iter().enumerate() {
        let cells =
            &now.hierarchy[k * HIERARCHY_CELLS as usize..(k + 1) * HIERARCHY_CELLS as usize];
        let table = i64::from(TABLE_PAGES);
        for ry in 0..side {
            for rx in 0..side {
                let page = [
                    i64::from(level.first_page.x) + rx as i64,
                    i64::from(level.first_page.y) + ry as i64,
                ];
                let e = k * ENTRIES
                    + (page[1].rem_euclid(table) * table + page[0].rem_euclid(table)) as usize;
                let dirty = now.table[e] & (ALLOCATED | DIRTY) == ALLOCATED | DIRTY;
                assert_eq!(
                    cells[ry * side + rx],
                    u32::from(dirty),
                    "level {k}, page ({rx}, {ry}) in the hierarchy"
                );
            }
        }
        for m in 1..HIERARCHY_MIPS {
            let (fine, coarse) = (side >> (m - 1), side >> m);
            let below = hierarchy_mip(m - 1) as usize;
            let at = hierarchy_mip(m) as usize;
            for y in 0..coarse {
                for x in 0..coarse {
                    let any =
                        (0..4).any(|i| cells[below + (2 * y + i / 2) * fine + 2 * x + i % 2] != 0);
                    assert_eq!(
                        cells[at + y * coarse + x],
                        u32::from(any),
                        "level {k}, mip {m}, cell ({x}, {y})"
                    );
                }
            }
        }
    }

    stats.needed = needed.len();
    stats.requests = requests.len();
    stats.free = free.len();
    stats.old = old.len();
    stats.dirty = dirty.len();
    stats
}

/// A device, a graph and the means to run frames of pages on it.
struct Bed {
    instance: Instance,
    device: Device,
    allocator: Allocator,
    bindless: Bindless,
    graph: RenderGraph,
    ring: UploadRing,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
}

impl Bed {
    fn new() -> Option<Self> {
        let (instance, device) = gpu()?;
        println!("adapter: {}", device.adapter().summary());
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let ring = UploadRing::new(&device, &mut allocator, 1, 1 << 20).unwrap();
        let d = device.raw();
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
        Some(Self {
            instance,
            device,
            allocator,
            bindless,
            graph,
            ring,
            pool,
            cmd,
        })
    }

    fn buffer(
        &mut self,
        size: u64,
        usage: vk::BufferUsageFlags,
        location: MemoryLocation,
    ) -> (vk::Buffer, Allocation) {
        self.allocator
            .create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(usage),
                location,
            )
            .unwrap()
    }

    /// Runs `vsm`'s frame over `depth` and, with `read_back`, reads everything back.
    fn frame(
        &mut self,
        vsm: &VirtualShadows,
        depth: &[f32],
        size: [u32; 2],
        view: &VsmView,
        read_back: bool,
    ) -> Option<Snapshot> {
        let settings = vsm.settings();
        let entries = settings.levels as u64 * ENTRIES as u64;
        let pages = u64::from(settings.pool_pages);
        let rows = settings.pool_pages.div_ceil(TABLE_PAGES);
        let pool_extent = [TABLE_PAGES * PAGE_TEXELS, rows * PAGE_TEXELS];
        let bytes = [
            entries * 4,
            pages * 8,
            (1 + entries) * 4,
            (u64::from(LIST_HEADER) + 3 * pages) * 4,
            u64::from(settings.levels * HIERARCHY_CELLS) * 4,
            u64::from(pool_extent[0]) * u64::from(pool_extent[1]) * 4,
        ];
        let depth_bytes = (depth.len() * 4) as u64;
        let (upload, upload_mem) = self.buffer(
            depth_bytes,
            vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::Upload,
        );
        // SAFETY: the mapping covers the buffer; nothing else uses it yet.
        unsafe {
            core::ptr::copy_nonoverlapping(
                depth.as_ptr().cast::<u8>(),
                upload_mem.mapped_ptr().unwrap().as_ptr(),
                depth_bytes as usize,
            );
        }
        let readbacks: Vec<(vk::Buffer, Allocation)> = bytes
            .iter()
            .filter(|_| read_back)
            .map(|&size| {
                self.buffer(
                    size,
                    vk::BufferUsageFlags::TRANSFER_DST,
                    MemoryLocation::Readback,
                )
            })
            .collect();
        let d = self.device.raw().clone();
        let cmd = self.cmd;
        // SAFETY: one-shot recording; the previous frame has finished.
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
        self.ring.begin_slot(0);
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { self.graph.begin_slot(0, cmd) };
            let target = fg.create_texture(
                "depth",
                TextureDesc::new_2d(vk::Format::D32_SFLOAT, size[0], size[1]),
            );
            let values = fg.import_buffer(
                "depth values",
                ImportedBuffer {
                    buffer: upload,
                    size: depth_bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: None,
                },
            );
            fg.add_pass("upload depth")
                .read(values, Access::TransferRead)
                .write(target, Access::TransferWrite)
                .execute(move |ctx| {
                    let region = vk::BufferImageCopy {
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::DEPTH,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_extent: vk::Extent3D {
                            width: size[0],
                            height: size[1],
                            depth: 1,
                        },
                        ..Default::default()
                    };
                    // SAFETY: recording; layouts arranged by the graph.
                    unsafe {
                        ctx.device.cmd_copy_buffer_to_image(
                            ctx.cmd,
                            ctx.buffer(values),
                            ctx.image(target),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &[region],
                        );
                    }
                });
            let frame = vsm
                .add_passes(&mut fg, &mut self.ring, &self.bindless, target, size, view)
                .unwrap();
            if read_back {
                let outs = readbacks
                    .iter()
                    .zip(bytes)
                    .map(|((buffer, _), size)| {
                        fg.import_buffer(
                            "vsm read back",
                            ImportedBuffer {
                                buffer: *buffer,
                                size,
                                address: 0,
                                initial: BufferState::idle(),
                                final_state: Some(BufferState::host_read()),
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                let sources = [
                    frame.table,
                    frame.meta,
                    frame.requests,
                    frame.lists,
                    frame.hierarchy,
                ];
                let mut pass = fg
                    .add_pass("vsm read back")
                    .read(frame.pool, Access::TransferRead);
                for (&source, &out) in sources.iter().zip(&outs) {
                    pass = pass
                        .read(source, Access::TransferRead)
                        .write(out, Access::TransferWrite);
                }
                let pool_out = outs[5];
                pass.write(pool_out, Access::TransferWrite)
                    .execute(move |ctx| {
                        // SAFETY: recording; the graph orders the copies after the pages'
                        // passes.
                        unsafe {
                            for ((&source, &out), size) in sources.iter().zip(&outs).zip(bytes) {
                                ctx.device.cmd_copy_buffer(
                                    ctx.cmd,
                                    ctx.buffer(source),
                                    ctx.buffer(out),
                                    &[vk::BufferCopy {
                                        src_offset: 0,
                                        dst_offset: 0,
                                        size,
                                    }],
                                );
                            }
                            ctx.device.cmd_copy_image_to_buffer(
                                ctx.cmd,
                                ctx.image(frame.pool),
                                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                ctx.buffer(pool_out),
                                &[vk::BufferImageCopy {
                                    image_subresource: vk::ImageSubresourceLayers {
                                        aspect_mask: vk::ImageAspectFlags::COLOR,
                                        mip_level: 0,
                                        base_array_layer: 0,
                                        layer_count: 1,
                                    },
                                    image_extent: vk::Extent3D {
                                        width: pool_extent[0],
                                        height: pool_extent[1],
                                        depth: 1,
                                    },
                                    ..Default::default()
                                }],
                            );
                        }
                    });
            }
            fg.execute_with_bindless(&mut self.allocator, Some(&self.bindless))
                .unwrap();
        }
        // SAFETY: as above.
        unsafe {
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                self.device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                vk::Fence::null(),
            )
            .unwrap();
            self.device.wait_idle().unwrap();
        }
        let words = |(_, memory): &(vk::Buffer, Allocation), size: u64| -> Vec<u32> {
            let ptr = memory.mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping covers the buffer.
            unsafe { core::slice::from_raw_parts(ptr.as_ptr().cast::<u32>(), (size / 4) as usize) }
                .to_vec()
        };
        let snapshot = read_back.then(|| {
            let meta = words(&readbacks[1], bytes[1]);
            Snapshot {
                table: words(&readbacks[0], bytes[0]),
                meta: meta.chunks_exact(2).map(|m| [m[0], m[1]]).collect(),
                requests: words(&readbacks[2], bytes[2]),
                lists: words(&readbacks[3], bytes[3]),
                hierarchy: words(&readbacks[4], bytes[4]),
                pool: words(&readbacks[5], bytes[5]),
            }
        });
        // SAFETY: the GPU is idle; the graph only borrowed them for this frame.
        unsafe {
            self.allocator.destroy_buffer(upload, upload_mem);
            for (buffer, memory) in readbacks {
                self.allocator.destroy_buffer(buffer, memory);
            }
        }
        snapshot
    }

    fn finish(self, shadows: Vec<VirtualShadows>) {
        let Self {
            instance,
            device,
            mut allocator,
            bindless,
            mut graph,
            mut ring,
            pool,
            ..
        } = self;
        // SAFETY: the device is idle.
        unsafe {
            for mut vsm in shadows {
                vsm.destroy(&mut allocator, &bindless);
            }
            graph.destroy(&mut allocator, Some(&bindless));
            ring.destroy(&mut allocator);
            device.raw().destroy_command_pool(pool, None);
        }
        drop(graph);
        drop(ring);
        drop(bindless);
        drop(allocator);
        let report = instance.validation_report();
        assert_eq!(report.errors, 0, "validation errors");
        drop(device);
    }
}

/// Runs frames of one virtual shadow map and checks each against the rules.
struct Run {
    vsm: VirtualShadows,
    prev: Snapshot,
    levels: Option<Vec<VsmLevel>>,
    light: Option<Vec3>,
    frame: u32,
}

impl Run {
    fn new(bed: &mut Bed, settings: VsmSettings) -> Self {
        let vsm =
            VirtualShadows::new(&bed.device, &mut bed.allocator, &bed.bindless, settings).unwrap();
        let prev = Snapshot::empty(&vsm.settings());
        Self {
            vsm,
            prev,
            levels: None,
            light: None,
            frame: 0,
        }
    }

    /// One frame of `camera` (over the ground where `ground` holds) in `light`'s pages:
    /// checked, and what happened.
    fn frame(
        &mut self,
        bed: &mut Bed,
        camera: &Camera,
        size: [u32; 2],
        light: Vec3,
        ground: impl Fn(Vec3) -> bool,
    ) -> Stats {
        let settings = self.vsm.settings();
        let depth = camera.depth(size, ground);
        let view = camera.view(size, light);
        let now = bed.frame(&self.vsm, &depth, size, &view, true).unwrap();
        self.frame += 1;
        let levels: Vec<VsmLevel> = (0..settings.levels)
            .map(|k| self.vsm.level(k).unwrap())
            .collect();
        let input = Input {
            frame: self.frame,
            levels: &levels,
            previous: self.levels.as_deref(),
            turned: self.light.is_some_and(|l| l != light),
        };
        let stats = check(&self.prev, &now, &input, &settings);
        let (required, allowed) = marks(&depth, size, &view, &levels, &settings);
        let needed: HashSet<usize> = (0..now.table.len())
            .filter(|&e| now.table[e] & NEEDED != 0)
            .collect();
        let missing = required.difference(&needed).count();
        let extra = needed.difference(&allowed).count();
        assert!(
            missing == 0 && extra == 0,
            "frame {}: {missing} pages the pixels need are not marked, {extra} marked that no \
             pixel needs ({} required, {} allowed, {} marked)",
            self.frame,
            required.len(),
            allowed.len(),
            needed.len(),
        );
        println!(
            "frame {}: {} needed ({} by pixels clear of an edge), {} asked for, {} given, {} \
             failed, {} evicted, {} wrapped, {} stale, {} dirty, {} backed",
            self.frame,
            stats.needed,
            required.len(),
            stats.requests,
            stats.allocated,
            stats.failed,
            stats.evicted,
            stats.wrapped,
            stats.stale,
            stats.dirty,
            stats.backed,
        );
        self.prev = now;
        self.levels = Some(levels);
        self.light = Some(light);
        stats
    }
}

fn sun(x: f32, z: f32) -> Vec3 {
    Vec3::new(x, -1.0, z).normalize()
}

/// The ground everywhere.
fn everywhere(_: Vec3) -> bool {
    true
}

#[test]
fn pages_follow_their_rules_as_the_camera_moves() {
    let Some(mut bed) = Bed::new() else {
        return;
    };
    let size = [960, 540];
    let mut run = Run::new(&mut bed, VsmSettings::default());
    let light = sun(0.4, 0.3);
    let mut camera = Camera {
        eye: Vec3::new(0.0, 1.7, 0.0),
        yaw: 0.0,
        pitch: -10f32.to_radians(),
    };
    // The first frame: every page the pixels need is new and dirty.
    let first = run.frame(&mut bed, &camera, size, light, everywhere);
    assert!(first.needed > 0);
    assert_eq!(first.failed, 0);
    assert_eq!(first.allocated, first.needed);
    assert_eq!(first.dirty, first.allocated);
    // A still frame allocates and draws nothing.
    let still = run.frame(&mut bed, &camera, size, light, everywhere);
    assert_eq!((still.requests, still.allocated, still.dirty), (0, 0, 0));
    assert_eq!(still.backed, first.backed);
    // Moving: small steps, then large ones; the windows scroll and free what wraps.
    let mut wrapped = 0;
    for step in [0.3, 2.0, 10.0, 50.0] {
        camera.eye.x += step;
        camera.yaw += 0.2;
        let moved = run.frame(&mut bed, &camera, size, light, everywhere);
        assert_eq!(moved.failed, 0);
        wrapped += moved.wrapped;
    }
    assert!(wrapped > 0, "no backed page wrapped");
    // The light turning: the needed pages are drawn again, the others freed.
    let turned = run.frame(&mut bed, &camera, size, sun(-0.2, 0.5), everywhere);
    assert!(turned.stale > 0);
    assert_eq!(
        (turned.dirty, turned.backed),
        (turned.needed, turned.needed)
    );
    // A jump of a kilometre: the fine levels start over.
    camera.eye += Vec3::new(1000.0, 0.0, -700.0);
    let jumped = run.frame(&mut bed, &camera, size, sun(-0.2, 0.5), everywhere);
    assert!(jumped.wrapped > 0);
    assert_eq!(jumped.failed, 0);
    let still = run.frame(&mut bed, &camera, size, sun(-0.2, 0.5), everywhere);
    assert_eq!((still.requests, still.dirty), (0, 0));
    bed.finish(vec![run.vsm]);
}

#[test]
fn a_short_pool_fails_then_takes_pages_unused_for_keep_frames() {
    let Some(mut bed) = Bed::new() else {
        return;
    };
    let size = [128, 72];
    let light = sun(0.4, 0.3);
    let eye = Vec3::new(0.0, 1.7, 0.0);
    let pitch = -15f32.to_radians();
    let ahead = Camera {
        eye,
        yaw: 0.0,
        pitch,
    };
    let behind = Camera {
        eye,
        yaw: core::f32::consts::PI,
        pitch,
    };
    // How many pages each view needs, in a pool with room for both.
    let mut probe = Run::new(&mut bed, VsmSettings::default());
    probe.frame(&mut bed, &ahead, size, light, everywhere);
    let needed = |snapshot: &Snapshot| -> HashSet<usize> {
        (0..snapshot.table.len())
            .filter(|&e| snapshot.table[e] & NEEDED != 0)
            .collect()
    };
    let a = needed(&probe.prev);
    probe.frame(&mut bed, &behind, size, light, everywhere);
    let b = needed(&probe.prev);
    assert!(
        !a.is_subset(&b) && !b.is_subset(&a),
        "each view needs pages the other does not"
    );
    // A pool with room for either view, not both.
    let pool = a.len().max(b.len()) as u32;
    println!(
        "ahead needs {} pages, behind {}, both {}: a pool of {pool}",
        a.len(),
        b.len(),
        a.union(&b).count()
    );
    let keep_frames = 2;
    let mut run = Run::new(
        &mut bed,
        VsmSettings {
            pool_pages: pool,
            keep_frames,
            ..VsmSettings::default()
        },
    );
    assert_eq!(run.vsm.settings().pool_pages, pool);
    let first = run.frame(&mut bed, &ahead, size, light, everywhere);
    assert_eq!((first.allocated, first.failed), (a.len(), 0));
    // Turning around: the pages ahead were needed too recently to be taken.
    for frame in 2..=1 + keep_frames {
        let short = run.frame(&mut bed, &behind, size, light, everywhere);
        assert!(short.failed > 0, "frame {frame}: no request failed");
        assert_eq!(short.evicted, 0, "frame {frame}: a page taken too early");
    }
    // Now they are old: the requests take them.
    let taken = run.frame(&mut bed, &behind, size, light, everywhere);
    assert!(taken.evicted > 0);
    assert_eq!(taken.failed, 0);
    let still = run.frame(&mut bed, &behind, size, light, everywhere);
    assert_eq!((still.requests, still.dirty), (0, 0));
    bed.finish(vec![probe.vsm, run.vsm]);
}

/// The median of `values`.
fn median(mut values: Vec<f32>) -> f32 {
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

#[test]
fn what_the_pages_cost_at_1080p() {
    let Some(mut bed) = Bed::new() else {
        return;
    };
    let size = [1920, 1080];
    let vsm = VirtualShadows::new(
        &bed.device,
        &mut bed.allocator,
        &bed.bindless,
        VsmSettings::default(),
    )
    .unwrap();
    let light = sun(0.4, 0.3);
    let start = Camera {
        eye: Vec3::new(0.0, 1.7, 0.0),
        yaw: 0.0,
        pitch: -10f32.to_radians(),
    };
    // The first frame, 30 still ones, then 20 walking (5 cm a frame: 3 m/s at 60 frames
    // a second, turning slowly); one more reads the last (a frame's times are read when
    // its slot comes around).
    const STILL: usize = 30;
    const WALKING: usize = 20;
    let mut cameras = vec![start; 1 + STILL];
    cameras.extend((1..=WALKING).map(|i| Camera {
        eye: start.eye - Vec3::Z * (0.05 * i as f32),
        yaw: start.yaw + 0.002 * i as f32,
        ..start
    }));
    cameras.push(*cameras.last().unwrap());
    let mut readings = Vec::with_capacity(cameras.len());
    let mut depth = Vec::new();
    let mut backed = 0;
    for (i, camera) in cameras.iter().enumerate() {
        if i == 0 || camera.eye != cameras[i - 1].eye {
            depth = camera.depth(size, everywhere);
        }
        // The first frame read back: how many pages it backed (and cleared).
        let snapshot = bed.frame(&vsm, &depth, size, &camera.view(size, light), i == 0);
        if let Some(snapshot) = snapshot {
            backed = snapshot
                .table
                .iter()
                .filter(|&&t| t & ALLOCATED != 0)
                .count();
        }
        readings.push(
            bed.graph
                .gpu_pass_times()
                .iter()
                .find(|t| t.name == "vsm pages")
                .map(|t| t.gpu_us),
        );
    }
    let time = |frame: usize| readings[frame + 1].unwrap();
    let first = time(0);
    let still = median((1..=STILL).map(time).collect());
    let walking = median((1 + STILL..1 + STILL + WALKING).map(time).collect());
    println!(
        "vsm pages at {}×{} ({} levels, a pool of {} pages, {:.1} MiB): first frame {first:.0} \
         µs ({backed} pages backed), still {still:.0} µs, walking {walking:.0} µs (medians)",
        size[0],
        size[1],
        vsm.settings().levels,
        vsm.settings().pool_pages,
        vsm.memory_bytes() as f64 / f64::from(1 << 20),
    );
    bed.finish(vec![vsm]);
}
