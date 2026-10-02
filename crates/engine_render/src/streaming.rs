//! Texture streaming, step 2 (ADR 0076): which levels of each texture to keep resident,
//! from what the GPU samples, within the GPU memory budget.
//!
//! - **Wanted levels come from texture feedback** ([`crate::feedback`]) and nothing else:
//!   each material slot's sampled level, plus `log2` of each of its textures' sizes, is
//!   the finest level that texture needs (rounded down: the sampler blends it with the
//!   next). A texture is **visible** while the feedback showed it within the last
//!   [`StreamerSettings::visible_frames`] frames, with a priority from the pixels its
//!   materials covered.
//! - **Required levels**, a texture's tail (its levels of at most 128 texels, ADR 0056),
//!   are loaded first and never evicted: every texture stays drawable.
//! - **Fitting the budget.** Visible textures are planned at their wanted level. When that
//!   does not fit the client's grant ([`crate::budget::GpuBudget`]), every visible
//!   texture drops its finest levels together (a common bias, one level at a time), then
//!   the textures covering the most pixels get one level back while room remains: the
//!   picture degrades evenly, finest levels first.
//! - **Caching.** Levels resident beyond the plan (textures not seen recently, or seen at
//!   a coarser level now) stay while there is room, the most recently seen kept first;
//!   when there is not, they are evicted ([`TexturePool::set_base`] copies the rest into
//!   a smaller image). Nothing is evicted without pressure, so turning back costs no I/O.
//! - **Loading.** Growing a texture reserves its larger image at once (so the budget
//!   counts it), then asks for the missing levels one at a time, coarsest first, in
//!   priority order (required tails, then visible textures by pixels), within
//!   [`StreamerSettings::request_bytes_per_frame`]. The caller reads them
//!   ([`LevelRequest`]: from a pack through the I/O ring, or from memory) and uploads them
//!   with [`TexturePool::upload_levels`]; the pool's per-frame byte budget spreads the GPU
//!   copies so no frame hitches.
//!
//! Nothing here predicts: no camera extrapolation and no screen-size estimate adds levels
//! the GPU did not sample (see the research note on patents). Allocates nothing per frame
//! once its scratch vectors have grown.

use crate::budget::{ClientId, Demand, GpuBudget};
use crate::cull::{Frustum, choose_lod, lod_scale, transform_box};
use crate::feedback::MaterialFeedback;
use crate::material::MaterialTable;
use crate::mesh::{GeometryPool, MAX_LODS, MeshError, MeshId, StreamedLevel};
use crate::texture_pool::{
    TextureError, TextureFormat, TextureId, TextureInfo, TexturePool, levels_bytes,
};
use engine_core::math::{Mat4, Vec3};

/// What the streamer needs of a texture pool ([`TexturePool`]; tests use a pool of their
/// own).
pub trait StreamedPool {
    /// [`TexturePool::info`].
    fn info(&self, id: TextureId) -> Option<TextureInfo>;
    /// [`TexturePool::set_base`].
    fn set_base(&mut self, id: TextureId, base: u32) -> Result<(), TextureError>;
    /// [`TexturePool::memory`].
    fn memory(&self) -> (u64, u64);
}

impl StreamedPool for TexturePool {
    fn info(&self, id: TextureId) -> Option<TextureInfo> {
        TexturePool::info(self, id)
    }

    fn set_base(&mut self, id: TextureId, base: u32) -> Result<(), TextureError> {
        TexturePool::set_base(self, id, base)
    }

    fn memory(&self) -> (u64, u64) {
        TexturePool::memory(self)
    }
}

/// Levels of at most this many texels per side are a texture's tail: required, loaded
/// first, never evicted (`engine_assets::TAIL_SIZE`).
pub const TAIL_SIZE: u32 = 128;

/// The first level of a texture's tail.
pub fn first_tail_level(width: u32, height: u32, levels: u32) -> u32 {
    (0..levels)
        .find(|&k| (width >> k).max(height >> k).max(1) <= TAIL_SIZE)
        .unwrap_or(levels.saturating_sub(1))
}

/// How the streamer plans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamerSettings {
    /// A texture counts as visible this many frames after the feedback last showed it.
    pub visible_frames: u32,
    /// Bytes of levels asked for per frame (the I/O rate; the pool's frame budget limits
    /// the GPU copies apart).
    pub request_bytes_per_frame: u64,
    /// Levels added to every wanted level (positive: blurrier, less memory).
    pub bias: f32,
    /// A texture's wanted level gets coarser only after this many frames without the
    /// finer one being asked for (it gets finer at once).
    pub hold_frames: u32,
}

impl Default for StreamerSettings {
    /// Visible for 60 frames (a second at 60 Hz), 32 MiB of requests per frame, no bias,
    /// coarser levels after 30 frames.
    fn default() -> Self {
        Self {
            visible_frames: 60,
            hold_frames: 30,
            request_bytes_per_frame: 32 << 20,
            bias: 0.0,
        }
    }
}

/// Levels of a texture to read and upload ([`TextureStreamer::update`]). Report the
/// outcome with [`TextureStreamer::arrived`] or [`TextureStreamer::failed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelRequest {
    /// The texture.
    pub texture: TextureId,
    /// Its first level asked for.
    pub first: u32,
    /// Levels from `first` (the tail's levels together, larger levels one at a time).
    pub count: u32,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    id: TextureId,
    width: u32,
    height: u32,
    levels: u32,
    format: TextureFormat,
    /// First tail level.
    tail: u32,
    /// Finest level the feedback asked for since it was last folded in (`levels`: none).
    wanted: u32,
    /// Its level while visible.
    level: u32,
    /// When `level` last got finer (or was confirmed).
    held_since: u64,
    last_seen: Option<u64>,
    pixels: u32,
    /// Levels asked for and not yet arrived (one bit each).
    requested: u32,
    /// The planned first level this frame.
    plan: u32,
    /// The first level the pool's image holds (or will, once a reallocation is made).
    base: u32,
}

impl Entry {
    fn bytes(&self, base: u32) -> u64 {
        levels_bytes(
            self.format,
            self.width,
            self.height,
            self.levels,
            base.min(self.tail),
        )
    }

    fn size_log2(&self) -> f32 {
        (self.width.max(self.height) as f32).log2()
    }
}

/// What a frame of [`TextureStreamer::update`] did, for overlays and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamerStats {
    /// Textures tracked.
    pub textures: u32,
    /// Textures visible.
    pub visible: u32,
    /// The common bias the plan needed to fit (0: everything visible at its wanted level).
    pub pressure_bias: u32,
    /// Estimated bytes of the required levels.
    pub required: u64,
    /// Estimated bytes of the plan (required levels included).
    pub planned: u64,
    /// The client's grant, in the same estimated bytes.
    pub grant: u64,
    /// Requests made this frame.
    pub requests: u32,
    /// Their bytes.
    pub request_bytes: u64,
    /// Textures shrunk (evicted levels) this frame.
    pub evictions: u32,
    /// Levels asked for and not yet arrived.
    pub in_flight: u32,
}

/// Texture streaming policy (see the module documentation).
#[derive(Debug)]
pub struct TextureStreamer {
    settings: StreamerSettings,
    entries: Vec<Entry>,
    /// Entry of each pool slot (`u32::MAX`: none).
    by_slot: Vec<u32>,
    client: ClientId,
    /// Scratch: entries by priority, and evictable extras.
    order: Vec<u32>,
    extras: Vec<(u64, u32)>,
    stats: StreamerStats,
}

fn slot_of(id: TextureId) -> usize {
    id.slot() as usize
}

impl TextureStreamer {
    /// A streamer that registers with `budget` as its "textures" client (`None` when the
    /// budget has no room for another client).
    pub fn new(budget: &mut GpuBudget, settings: StreamerSettings) -> Option<Self> {
        Some(Self {
            settings,
            entries: Vec::new(),
            by_slot: Vec::new(),
            client: budget.register("textures")?,
            order: Vec::new(),
            extras: Vec::new(),
            stats: StreamerStats::default(),
        })
    }

    /// The settings.
    pub fn settings(&self) -> StreamerSettings {
        self.settings
    }

    /// Changes the settings.
    pub fn set_settings(&mut self, settings: StreamerSettings) {
        self.settings = settings;
    }

    /// Its budget client.
    pub fn client(&self) -> ClientId {
        self.client
    }

    /// The last frame's numbers.
    pub fn stats(&self) -> StreamerStats {
        self.stats
    }

    /// Streams a texture of the pool, made with [`TexturePool::add_streamed`] holding its
    /// tail ([`first_tail_level`]); its tail is requested first.
    pub fn track(&mut self, pool: &impl StreamedPool, id: TextureId) {
        let Some(info) = pool.info(id) else {
            return;
        };
        let tail = first_tail_level(info.width, info.height, info.levels);
        let slot = slot_of(id);
        if self.by_slot.len() <= slot {
            self.by_slot.resize(slot + 1, u32::MAX);
        }
        self.by_slot[slot] = self.entries.len() as u32;
        self.entries.push(Entry {
            id,
            width: info.width,
            height: info.height,
            levels: info.levels,
            format: info.format,
            tail,
            wanted: info.levels,
            level: tail,
            held_since: 0,
            last_seen: None,
            pixels: 0,
            requested: 0,
            plan: tail,
            base: tail,
        });
    }

    /// Stops streaming a texture (before removing it from the pool).
    pub fn untrack(&mut self, id: TextureId) {
        let Some(&e) = self.by_slot.get(slot_of(id)) else {
            return;
        };
        let e = e as usize;
        if e >= self.entries.len() || self.entries[e].id != id {
            return;
        }
        self.by_slot[slot_of(id)] = u32::MAX;
        self.entries.swap_remove(e);
        if let Some(moved) = self.entries.get(e) {
            self.by_slot[slot_of(moved.id)] = e as u32;
        }
    }

    fn entry_mut(&mut self, id: TextureId) -> Option<&mut Entry> {
        let e = *self.by_slot.get(slot_of(id))? as usize;
        self.entries.get_mut(e).filter(|e| e.id == id)
    }

    /// Folds a feedback frame in ([`crate::feedback::TextureFeedback::results`]): each
    /// material's level, plus each of its textures' sizes, is that texture's wanted level.
    pub fn feedback(
        &mut self,
        results: impl Iterator<Item = MaterialFeedback>,
        materials: &MaterialTable,
        frame: u64,
    ) {
        let bias = self.settings.bias;
        let hold = u64::from(self.settings.hold_frames);
        for r in results {
            for id in materials.slot_textures(r.slot).into_iter().flatten() {
                let Some(e) = self.entry_mut(id) else {
                    continue;
                };
                let level = (r.level + e.size_log2() + bias).floor().max(0.0) as u32;
                let level = level.min(e.tail);
                if e.last_seen != Some(frame) {
                    e.last_seen = Some(frame);
                    e.wanted = level;
                    e.pixels = r.pixels;
                } else {
                    e.wanted = e.wanted.min(level);
                    e.pixels = e.pixels.saturating_add(r.pixels);
                }
                // Finer at once; coarser only once the finer level has not been asked for
                // in `hold_frames` (levels flickering across a boundary would otherwise be
                // evicted and read again over and over under pressure).
                if e.wanted <= e.level || frame.saturating_sub(e.held_since) >= hold {
                    e.level = e.wanted;
                    e.held_since = frame;
                }
            }
        }
    }

    /// The levels a texture arrived with (uploaded to the pool): asked for no more.
    pub fn arrived(&mut self, id: TextureId, first: u32, count: u32) {
        if let Some(e) = self.entry_mut(id) {
            e.requested &= !level_mask(first, count);
        }
    }

    /// Levels that could not be read or uploaded: asked for again later.
    pub fn failed(&mut self, id: TextureId, first: u32, count: u32) {
        self.arrived(id, first, count);
    }

    /// Plans this frame: reports demand to the budget (call [`GpuBudget::update`] after),
    /// fits the plan into the client's grant from the last update, reserves or evicts
    /// levels in `pool`, and appends the levels to read to `requests`.
    pub fn update(
        &mut self,
        pool: &mut impl StreamedPool,
        budget: &mut GpuBudget,
        frame: u64,
        requests: &mut Vec<LevelRequest>,
    ) {
        engine_profiler::zone!("render::TextureStreamer::update");
        let visible_frames = u64::from(self.settings.visible_frames);
        let is_visible = |e: &Entry| {
            e.last_seen
                .is_some_and(|f| frame.saturating_sub(f) < visible_frames)
        };
        // Estimated bytes against the real ones: images are padded by the driver.
        let (live, retiring) = pool.memory();
        let mut estimated_live = 0u64;
        let mut required = 0u64;
        let mut visible_extra = 0u64;
        let mut cached = 0u64;
        let mut visible = 0u32;
        for e in &self.entries {
            let info = pool.info(e.id);
            let base = info.map_or(e.tail, |i| i.planned_base);
            estimated_live += e.bytes(base);
            required += e.bytes(e.tail);
            if is_visible(e) {
                visible += 1;
                visible_extra += e.bytes(e.level) - e.bytes(e.tail);
                cached += e.bytes(base).saturating_sub(e.bytes(e.level));
            } else {
                cached += e.bytes(base) - e.bytes(e.tail);
            }
        }
        let scale = if estimated_live > 0 {
            (live as f64 / estimated_live as f64).max(1.0)
        } else {
            1.0
        };
        let real = |bytes: u64| (bytes as f64 * scale) as u64;
        budget.report(
            self.client,
            live + retiring,
            Demand {
                required: real(required),
                visible: real(visible_extra),
                cached: real(cached),
            },
        );
        // The grant, in estimated bytes; retiring images still hold memory.
        let grant = ((budget.grant(self.client) as f64 / scale) as u64)
            .saturating_sub((retiring as f64 / scale) as u64);

        // Plan the visible textures: a common bias until they fit, then a level back for
        // some, by pixels.
        for e in &mut self.entries {
            e.base = pool.info(e.id).map_or(e.tail, |i| i.planned_base);
        }
        let mut bias = 0u32;
        let planned_with = |entries: &[Entry], bias: u32| -> u64 {
            entries
                .iter()
                .map(|e| {
                    if is_visible(e) {
                        e.bytes(e.level + bias)
                    } else {
                        e.bytes(e.tail)
                    }
                })
                .sum()
        };
        let mut planned = planned_with(&self.entries, 0);
        while planned > grant && bias < 16 {
            bias += 1;
            planned = planned_with(&self.entries, bias);
        }
        self.order.clear();
        for (k, e) in self.entries.iter_mut().enumerate() {
            e.plan = if is_visible(e) {
                (e.level + bias).min(e.tail)
            } else {
                e.tail
            };
            self.order.push(k as u32);
        }
        // Priority: visible first, most pixels first.
        let entries = &self.entries;
        self.order.sort_unstable_by_key(|&k| {
            let e = &entries[k as usize];
            (
                core::cmp::Reverse(is_visible(e)),
                core::cmp::Reverse(e.pixels),
            )
        });
        if bias > 0 {
            // Textures that already hold the finer level keep it first: the levels given
            // back do not move between textures from frame to frame as pixel counts
            // change (each move would be an eviction and a read).
            for held in [true, false] {
                for &k in &self.order {
                    let e = &mut self.entries[k as usize];
                    if !is_visible(e) || e.plan == 0 || e.plan <= e.level {
                        continue;
                    }
                    if (e.base < e.plan) != held {
                        continue;
                    }
                    let more = e.bytes(e.plan - 1) - e.bytes(e.plan);
                    if planned + more <= grant {
                        planned += more;
                        e.plan -= 1;
                    }
                }
            }
        }

        // What the textures will hold: at least the plan or what they hold, whichever is
        // coarser; growth towards the plan (visible) first, then levels held beyond it
        // (cached, most recently seen first), while the grant has room. Everything else
        // beyond the plan is evicted.
        let mut committed: u64 = self
            .entries
            .iter()
            .map(|e| e.bytes(e.plan.max(e.base)))
            .sum();
        let mut room = grant.saturating_sub(committed);
        let mut evictions = 0u32;
        for &k in &self.order {
            let e = &mut self.entries[k as usize];
            if e.plan >= e.base {
                continue;
            }
            // Grow as far towards the plan as the room allows. While the copy is made the
            // old image lives on for the frames using it: the new one must fit whole.
            let Some(target) = (e.plan..e.base).find(|&l| e.bytes(l) <= room) else {
                e.plan = e.base;
                continue;
            };
            let cost = e.bytes(target) - e.bytes(e.base);
            if pool.set_base(e.id, target).is_ok() {
                room -= cost;
                committed += cost;
                e.plan = target;
            } else {
                e.plan = e.base;
            }
        }
        self.extras.clear();
        for (k, e) in self.entries.iter().enumerate() {
            if e.base < e.plan {
                self.extras
                    .push((e.last_seen.map_or(0, |f| f + 1), k as u32));
            }
        }
        self.extras
            .sort_unstable_by_key(|&(seen, _)| core::cmp::Reverse(seen));
        for &(_, k) in &self.extras {
            let e = &mut self.entries[k as usize];
            let extra = e.bytes(e.base) - e.bytes(e.plan);
            if extra <= room {
                room -= extra;
                committed += extra;
                // Kept: the plan is what is there.
                e.plan = e.base;
            } else if pool.set_base(e.id, e.plan).is_ok() {
                evictions += 1;
                e.requested &= level_mask(e.plan, e.levels - e.plan);
            }
        }
        let planned = committed;

        // Requests: tails first, then visible textures by priority, the next missing level
        // of each per round, within the frame's request bytes.
        let mut request_bytes = 0u64;
        let mut count = 0u32;
        let limit = self.settings.request_bytes_per_frame;
        // Tails.
        for &k in &self.order {
            let e = &mut self.entries[k as usize];
            let Some(info) = pool.info(e.id) else {
                continue;
            };
            let tail_mask = level_mask(e.tail, e.levels - e.tail);
            if info.uploaded & tail_mask == tail_mask || e.requested & tail_mask != 0 {
                continue;
            }
            if request_bytes > 0 && request_bytes + e.bytes(e.tail) > limit {
                break;
            }
            request_bytes += e.bytes(e.tail);
            count += 1;
            e.requested |= tail_mask;
            requests.push(LevelRequest {
                texture: e.id,
                first: e.tail,
                count: e.levels - e.tail,
            });
        }
        // Larger levels, a round at a time.
        let mut progress = true;
        while progress && request_bytes < limit {
            progress = false;
            for &k in &self.order {
                let e = &mut self.entries[k as usize];
                let Some(info) = pool.info(e.id) else {
                    continue;
                };
                // Levels of the plan only (reserved above).
                if e.plan >= e.tail || info.planned_base > e.plan {
                    continue;
                }
                // The next level down from what is uploaded or asked for.
                let have = info.uploaded | e.requested;
                let Some(next) = (e.plan..e.tail).rev().find(|&l| have & (1 << l) == 0) else {
                    continue;
                };
                let bytes = levels_bytes(e.format, e.width, e.height, next + 1, next);
                if request_bytes > 0 && request_bytes + bytes > limit {
                    progress = false;
                    break;
                }
                request_bytes += bytes;
                count += 1;
                progress = true;
                e.requested |= 1 << next;
                requests.push(LevelRequest {
                    texture: e.id,
                    first: next,
                    count: 1,
                });
            }
        }
        self.stats = StreamerStats {
            textures: self.entries.len() as u32,
            visible,
            pressure_bias: bias,
            required,
            planned,
            grant,
            requests: count,
            request_bytes,
            evictions,
            in_flight: self.entries.iter().map(|e| e.requested.count_ones()).sum(),
        };
    }
}

fn level_mask(first: u32, count: u32) -> u32 {
    (((1u64 << (first + count)) - (1u64 << first)) & u64::from(u32::MAX)) as u32
}

/// What a mesh streamer needs of a geometry pool ([`GeometryPool`]; tests use one of their
/// own).
pub trait StreamedGeometry {
    /// [`GeometryPool::mesh_residency`].
    fn mesh_residency(&self, id: MeshId) -> Option<(u32, u32)>;
    /// [`GeometryPool::evict_mesh_levels`].
    fn evict_mesh_levels(&mut self, id: MeshId, first: u32, frame: u64) -> Result<(), MeshError>;
    /// [`GeometryPool::streamed_bytes`].
    fn streamed_bytes(&self) -> u64;
}

impl StreamedGeometry for GeometryPool {
    fn mesh_residency(&self, id: MeshId) -> Option<(u32, u32)> {
        GeometryPool::mesh_residency(self, id)
    }

    fn evict_mesh_levels(&mut self, id: MeshId, first: u32, frame: u64) -> Result<(), MeshError> {
        GeometryPool::evict_mesh_levels(self, id, first, frame)
    }

    fn streamed_bytes(&self) -> u64 {
        GeometryPool::streamed_bytes(self)
    }
}

/// The view meshes are streamed for: its camera, as the culling pass sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LodView {
    /// The camera's position.
    pub eye: Vec3,
    /// Its frustum: instances outside it count as not seen.
    pub frustum: Frustum,
    /// [`crate::cull::lod_scale`] of the view.
    pub lod_scale: f32,
}

impl LodView {
    /// The view of a camera (`clip_from_world`, unjittered) drawn `height` pixels high
    /// with the renderer's detail-level threshold (`ForwardRenderer::set_lod_threshold`,
    /// 1 pixel by default).
    pub fn new(clip_from_world: &Mat4, eye: Vec3, height: u32, threshold: f32) -> Self {
        Self {
            eye,
            frustum: Frustum::from_clip_from_world(*clip_from_world),
            lod_scale: lod_scale(clip_from_world, height, threshold),
        }
    }
}

/// A finer level of a mesh to read and upload ([`MeshStreamer::update`]); report it with
/// [`MeshStreamer::arrived`] or [`MeshStreamer::failed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshLevelRequest {
    /// The mesh.
    pub mesh: MeshId,
    /// The level (one finer than its finest resident level).
    pub level: u32,
}

#[derive(Debug, Clone, Copy)]
struct MeshEntry {
    id: MeshId,
    levels: [StreamedLevel; MAX_LODS],
    errors: [f32; MAX_LODS],
    count: u32,
    /// Object-space box.
    center: Vec3,
    extents: Vec3,
    /// Finest level the instances seen this frame need, and the level kept (hysteresis).
    wanted: u32,
    level: u32,
    held_since: u64,
    last_seen: Option<u64>,
    /// A level asked for and not yet arrived.
    requested: Option<u32>,
    /// The plan this frame.
    plan: u32,
    /// Its finest resident level.
    resident: u32,
}

impl MeshEntry {
    /// Bytes resident from level `first`: the vertices it uses and the indices of it and
    /// every coarser level.
    fn bytes(&self, first: u32) -> u64 {
        let first = first.min(self.count - 1) as usize;
        u64::from(self.levels[first].vertices) * 28
            + self.levels[first..self.count as usize]
                .iter()
                .map(|l| u64::from(l.index_count) * 4)
                .sum::<u64>()
    }
}

/// What a frame of [`MeshStreamer::update`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MeshStreamerStats {
    /// Meshes tracked.
    pub meshes: u32,
    /// Meshes seen recently.
    pub visible: u32,
    /// The common bias (levels coarser) the plan needed to fit.
    pub pressure_bias: u32,
    /// Bytes resident, planned, granted.
    pub resident: u64,
    /// Bytes of the plan.
    pub planned: u64,
    /// The client's grant.
    pub grant: u64,
    /// Requests this frame.
    pub requests: u32,
    /// Meshes evicted to coarser levels this frame.
    pub evictions: u32,
}

/// Mesh detail-level streaming policy (ADR 0076): each mesh keeps the levels its visible
/// instances need by the culling pass's own rule ([`crate::cull::choose_lod`]: the
/// projected error at the instance's nearest distance), coarsest first, within the budget.
/// The rule is exact and cheap on the CPU (a box and a distance per instance), unlike a
/// texture's sampled level, so no GPU feedback is needed. Instances outside the view's
/// frustum do not ask; levels nobody asks for stay cached until room is needed.
#[derive(Debug)]
pub struct MeshStreamer {
    settings: StreamerSettings,
    entries: Vec<MeshEntry>,
    by_slot: Vec<u32>,
    client: ClientId,
    order: Vec<u32>,
    stats: MeshStreamerStats,
}

impl MeshStreamer {
    /// A streamer registered with `budget` as its "meshes" client (`None` when the budget
    /// has no room for another client). Its settings are the texture streamer's: visible
    /// and hold windows, request bytes per frame.
    pub fn new(budget: &mut GpuBudget, settings: StreamerSettings) -> Option<Self> {
        Some(Self {
            settings,
            entries: Vec::new(),
            by_slot: Vec::new(),
            client: budget.register("meshes")?,
            order: Vec::new(),
            stats: MeshStreamerStats::default(),
        })
    }

    /// Its budget client.
    pub fn client(&self) -> ClientId {
        self.client
    }

    /// The last frame's numbers.
    pub fn stats(&self) -> MeshStreamerStats {
        self.stats
    }

    /// Streams a mesh added with [`GeometryPool::add_streamed_mesh`] with these levels
    /// (the full mesh first) and object-space box.
    pub fn track(&mut self, id: MeshId, levels: &[StreamedLevel], min: Vec3, max: Vec3) {
        let count = levels.len().clamp(1, MAX_LODS);
        let mut entry = MeshEntry {
            id,
            levels: [StreamedLevel::default(); MAX_LODS],
            errors: [0.0; MAX_LODS],
            count: count as u32,
            center: (min + max) * 0.5,
            extents: (max - min) * 0.5,
            wanted: count as u32 - 1,
            level: count as u32 - 1,
            held_since: 0,
            last_seen: None,
            requested: None,
            plan: count as u32 - 1,
            resident: count as u32 - 1,
        };
        entry.levels[..count].copy_from_slice(&levels[..count]);
        for (e, l) in entry.errors.iter_mut().zip(&levels[..count]) {
            *e = l.error;
        }
        let slot = id.slot() as usize;
        if self.by_slot.len() <= slot {
            self.by_slot.resize(slot + 1, u32::MAX);
        }
        self.by_slot[slot] = self.entries.len() as u32;
        self.entries.push(entry);
    }

    fn entry_mut(&mut self, id: MeshId) -> Option<&mut MeshEntry> {
        let e = *self.by_slot.get(id.slot() as usize)? as usize;
        self.entries.get_mut(e).filter(|e| e.id == id)
    }

    /// An instance of a tracked mesh in this frame's view: asks for the level the culling
    /// pass would draw it at, when its box is in the view's frustum.
    pub fn observe(&mut self, view: &LodView, mesh: MeshId, world_from_object: &Mat4, frame: u64) {
        let Some(e) = self.entry_mut(mesh) else {
            return;
        };
        let (center, extents) = transform_box(world_from_object, e.center, e.extents);
        if !view.frustum.intersects_box(center, extents) {
            return;
        }
        let level = choose_lod(
            &e.errors[..e.count as usize],
            world_from_object,
            center,
            extents,
            view.eye,
            view.lod_scale,
        );
        if e.last_seen != Some(frame) {
            e.last_seen = Some(frame);
            e.wanted = level;
        } else {
            e.wanted = e.wanted.min(level);
        }
    }

    /// A level that arrived (uploaded with [`GeometryPool::upload_mesh_level`]).
    pub fn arrived(&mut self, mesh: MeshId, level: u32) {
        if let Some(e) = self.entry_mut(mesh)
            && e.requested == Some(level)
        {
            e.requested = None;
        }
    }

    /// A level that could not be read or uploaded: asked for again later.
    pub fn failed(&mut self, mesh: MeshId, level: u32) {
        self.arrived(mesh, level);
    }

    /// Plans this frame after the instances were observed: reports demand to the budget
    /// (call [`GpuBudget::update`] after), fits the plan into the grant, evicts levels
    /// beyond it under pressure, and appends the next level of each mesh that wants more to
    /// `requests` (one at a time per mesh: each level grows the previous one's vertices).
    pub fn update(
        &mut self,
        geometry: &mut impl StreamedGeometry,
        budget: &mut GpuBudget,
        frame: u64,
        requests: &mut Vec<MeshLevelRequest>,
    ) {
        engine_profiler::zone!("render::MeshStreamer::update");
        let visible_frames = u64::from(self.settings.visible_frames);
        let hold = u64::from(self.settings.hold_frames);
        let is_visible = |e: &MeshEntry| {
            e.last_seen
                .is_some_and(|f| frame.saturating_sub(f) < visible_frames)
        };
        let (mut required, mut visible_extra, mut cached, mut visible) = (0, 0, 0, 0);
        for e in &mut self.entries {
            e.resident = geometry
                .mesh_residency(e.id)
                .map_or(e.count - 1, |(resident, _)| resident);
            // Finer at once; coarser once the finer level was not asked for in a while.
            if e.last_seen == Some(frame)
                && (e.wanted <= e.level || frame.saturating_sub(e.held_since) >= hold)
            {
                e.level = e.wanted;
                e.held_since = frame;
            }
            let coarsest = e.count - 1;
            required += e.bytes(coarsest);
            if is_visible(e) {
                visible += 1;
                visible_extra += e.bytes(e.level) - e.bytes(coarsest);
                cached += e.bytes(e.resident).saturating_sub(e.bytes(e.level));
            } else {
                cached += e.bytes(e.resident) - e.bytes(coarsest);
            }
        }
        let resident = geometry.streamed_bytes();
        budget.report(
            self.client,
            resident,
            Demand {
                required,
                visible: visible_extra,
                cached,
            },
        );
        let grant = budget.grant(self.client);
        // Plan: wanted levels, a common bias while they do not fit.
        let planned_with = |entries: &[MeshEntry], bias: u32| -> u64 {
            entries
                .iter()
                .map(|e| {
                    if is_visible(e) {
                        e.bytes(e.level + bias)
                    } else {
                        e.bytes(e.count - 1)
                    }
                })
                .sum()
        };
        let mut bias = 0;
        let mut planned = planned_with(&self.entries, 0);
        while planned > grant && bias < MAX_LODS as u32 {
            bias += 1;
            planned = planned_with(&self.entries, bias);
        }
        for e in &mut self.entries {
            e.plan = if is_visible(e) {
                (e.level + bias).min(e.count - 1)
            } else {
                e.count - 1
            };
        }
        // Levels resident beyond the plan stay while there is room, most recently seen
        // first; the rest are evicted.
        let mut committed: u64 = self
            .entries
            .iter()
            .map(|e| e.bytes(e.plan.max(e.resident)))
            .sum();
        self.order.clear();
        self.order
            .extend((0..self.entries.len() as u32).filter(|&k| {
                let e = &self.entries[k as usize];
                e.resident < e.plan
            }));
        let entries = &self.entries;
        self.order.sort_unstable_by_key(|&k| {
            core::cmp::Reverse(entries[k as usize].last_seen.map_or(0, |f| f + 1))
        });
        let mut evictions = 0;
        for &k in &self.order {
            let e = &mut self.entries[k as usize];
            let extra = e.bytes(e.resident) - e.bytes(e.plan);
            if committed + extra <= grant {
                committed += extra;
                e.plan = e.resident;
            } else if geometry.evict_mesh_levels(e.id, e.plan, frame).is_ok() {
                evictions += 1;
            } else {
                // Busy: kept this frame.
                e.plan = e.resident;
            }
        }
        // Requests: the next level of every mesh short of its plan, while the grant has
        // room for it and within the frame's bytes; nearest-to-done first is not tracked,
        // the meshes seen most recently go first.
        self.order.clear();
        self.order
            .extend((0..self.entries.len() as u32).filter(|&k| {
                let e = &self.entries[k as usize];
                e.plan < e.resident && e.requested.is_none()
            }));
        let entries = &self.entries;
        self.order.sort_unstable_by_key(|&k| {
            core::cmp::Reverse(entries[k as usize].last_seen.map_or(0, |f| f + 1))
        });
        let (mut bytes, mut count) = (0u64, 0u32);
        for &k in &self.order {
            let e = &mut self.entries[k as usize];
            let next = e.resident - 1;
            let more = e.bytes(next) - e.bytes(e.resident);
            if committed + more > grant
                || (bytes > 0 && bytes + more > self.settings.request_bytes_per_frame)
            {
                continue;
            }
            committed += more;
            bytes += more;
            count += 1;
            e.requested = Some(next);
            requests.push(MeshLevelRequest {
                mesh: e.id,
                level: next,
            });
        }
        self.stats = MeshStreamerStats {
            meshes: self.entries.len() as u32,
            visible,
            pressure_bias: bias,
            resident,
            planned: committed,
            grant,
            requests: count,
            evictions,
        };
    }
}

#[cfg(test)]
mod mesh_tests {
    use super::*;
    use crate::budget::BudgetSettings;
    use engine_core::math::{look_at, vulkan_perspective};
    use engine_rhi::HeapBudget;

    /// A pool of streamed meshes that makes uploads and evictions at once.
    #[derive(Default)]
    struct FakeGeometry {
        meshes: Vec<(u32, [StreamedLevel; MAX_LODS], u32)>,
    }

    impl StreamedGeometry for FakeGeometry {
        fn mesh_residency(&self, id: MeshId) -> Option<(u32, u32)> {
            let m = self.meshes.get(id.slot() as usize)?;
            Some((m.0, m.2))
        }

        fn evict_mesh_levels(&mut self, id: MeshId, first: u32, _: u64) -> Result<(), MeshError> {
            self.meshes[id.slot() as usize].0 = first;
            Ok(())
        }

        fn streamed_bytes(&self) -> u64 {
            self.meshes
                .iter()
                .map(|(resident, levels, count)| {
                    u64::from(levels[*resident as usize].vertices) * 28
                        + levels[*resident as usize..*count as usize]
                            .iter()
                            .map(|l| u64::from(l.index_count) * 4)
                            .sum::<u64>()
                })
                .sum()
        }
    }

    /// Levels of a mesh: 4,000 vertices and 24,000 indices halving per level, errors
    /// 0, 0.01, 0.02, 0.04.
    fn levels() -> [StreamedLevel; 4] {
        core::array::from_fn(|k| StreamedLevel {
            index_count: 24_000 >> k,
            error: if k == 0 {
                0.0
            } else {
                0.01 * (1 << (k - 1)) as f32
            },
            vertices: 4_000 >> k,
        })
    }

    struct World {
        geometry: FakeGeometry,
        budget: GpuBudget,
        streamer: MeshStreamer,
        ids: Vec<MeshId>,
        frame: u64,
        requests: Vec<MeshLevelRequest>,
    }

    impl World {
        fn new(count: usize, limit: u64) -> Self {
            let mut budget = GpuBudget::with_settings(BudgetSettings {
                streaming_limit: Some(limit),
                ..BudgetSettings::default()
            });
            let mut streamer = MeshStreamer::new(&mut budget, StreamerSettings::default()).unwrap();
            let mut geometry = FakeGeometry::default();
            let mut ids = Vec::new();
            for k in 0..count {
                let id = MeshId::from_parts(k as u32, 0);
                let mut l = [StreamedLevel::default(); MAX_LODS];
                l.copy_from_slice(&levels());
                geometry.meshes.push((3, l, 4));
                streamer.track(id, &levels(), Vec3::splat(-0.5), Vec3::splat(0.5));
                ids.push(id);
            }
            Self {
                geometry,
                budget,
                streamer,
                ids,
                frame: 0,
                requests: Vec::new(),
            }
        }

        /// A frame with one instance of each mesh at `distances` (along −z), seen from the
        /// origin, 1000 pixels high, 90° field of view (500 pixels per unit at 1).
        fn frame(&mut self, distances: &[f32]) {
            self.frame += 1;
            let clip = vulkan_perspective(90f32.to_radians(), 1.0, 0.1, 1000.0)
                * look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y);
            let view = LodView::new(&clip, Vec3::ZERO, 1000, 1.0);
            for (k, &d) in distances.iter().enumerate() {
                let world = Mat4::from_translation(Vec3::new(0.0, 0.0, -d));
                self.streamer
                    .observe(&view, self.ids[k], &world, self.frame);
            }
            self.requests.clear();
            self.streamer.update(
                &mut self.geometry,
                &mut self.budget,
                self.frame,
                &mut self.requests,
            );
            self.budget.distribute(HeapBudget {
                size: 1 << 40,
                device_local: true,
                budget: 1 << 40,
                usage: 0,
                allocated: 0,
            });
            for r in &self.requests {
                self.geometry.meshes[r.mesh.slot() as usize].0 = r.level;
                self.streamer.arrived(r.mesh, r.level);
            }
        }

        fn resident(&self) -> Vec<u32> {
            self.geometry.meshes.iter().map(|m| m.0).collect()
        }
    }

    #[test]
    fn levels_follow_the_screen_size_one_at_a_time() {
        let mut w = World::new(3, 1 << 30);
        // A pixel of error: level 3 (0.04) from 20.5 units, 2 (0.02) from 10.5, 1 (0.01)
        // from 5.5 (the box reaches 0.87 units closer).
        let distances = [3.0, 8.0, 15.0];
        // The first frame's grant comes from the budget's first update.
        w.frame(&distances);
        assert_eq!(w.resident(), [3, 3, 3]);
        w.frame(&distances);
        assert_eq!(w.resident(), [2, 2, 2], "one level per mesh per frame");
        w.frame(&distances);
        assert_eq!(w.resident(), [1, 1, 2]);
        w.frame(&distances);
        assert_eq!(w.resident(), [0, 1, 2]);
        w.frame(&distances);
        assert!(w.requests.is_empty());
        // Out of the frustum: kept (cached), nothing evicted without pressure.
        for _ in 0..100 {
            w.frame(&[-5.0, -5.0, -5.0]);
        }
        assert_eq!(w.resident(), [0, 1, 2]);
    }

    #[test]
    fn pressure_makes_every_visible_mesh_coarser_and_unseen_ones_go_first() {
        let mut w = World::new(3, 1 << 30);
        for _ in 0..4 {
            w.frame(&[3.0, 3.0, 3.0]);
        }
        assert_eq!(w.resident(), [0, 0, 0]);
        // Room for everything one level coarser.
        let one_coarser = 3 * levels_bytes_of(1);
        w.budget.set_settings(BudgetSettings {
            streaming_limit: Some(one_coarser),
            ..w.budget.settings()
        });
        w.frame(&[3.0, 3.0, 3.0]);
        w.frame(&[3.0, 3.0, 3.0]);
        assert_eq!(w.resident(), [1, 1, 1]);
        assert_eq!(w.streamer.stats().pressure_bias, 1);
    }

    fn levels_bytes_of(first: usize) -> u64 {
        let l = levels();
        u64::from(l[first].vertices) * 28
            + l[first..]
                .iter()
                .map(|l| u64::from(l.index_count) * 4)
                .sum::<u64>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetSettings;
    use engine_rhi::HeapBudget;

    /// A pool that makes reallocations at once and keeps texel bytes as its memory.
    #[derive(Default)]
    struct FakePool {
        textures: Vec<TextureInfo>,
    }

    impl FakePool {
        fn add(&mut self, size: u32) -> TextureId {
            let levels = 32 - size.leading_zeros();
            let tail = first_tail_level(size, size, levels);
            self.textures.push(TextureInfo {
                width: size,
                height: size,
                levels,
                format: TextureFormat::Bc7Unorm,
                base: tail,
                planned_base: tail,
                resident: levels,
                uploaded: 0,
                bytes: levels_bytes(TextureFormat::Bc7Unorm, size, size, levels, tail),
            });
            TextureId::from_parts(self.textures.len() as u32 - 1, 0)
        }

        fn upload(&mut self, r: &LevelRequest) {
            let t = &mut self.textures[r.texture.slot() as usize];
            if r.first >= t.planned_base {
                t.uploaded |= level_mask(r.first, r.count);
            }
        }

        fn base(&self, id: TextureId) -> u32 {
            self.textures[id.slot() as usize].base
        }
    }

    impl StreamedPool for FakePool {
        fn info(&self, id: TextureId) -> Option<TextureInfo> {
            self.textures.get(id.slot() as usize).copied()
        }

        fn set_base(&mut self, id: TextureId, base: u32) -> Result<(), TextureError> {
            let t = &mut self.textures[id.slot() as usize];
            t.base = base;
            t.planned_base = base;
            t.uploaded &= !((1u32 << base) - 1);
            t.bytes = levels_bytes(t.format, t.width, t.height, t.levels, base);
            Ok(())
        }

        fn memory(&self) -> (u64, u64) {
            (self.textures.iter().map(|t| t.bytes).sum(), 0)
        }
    }

    const MIB: u64 = 1 << 20;

    fn bytes(base: u32) -> u64 {
        levels_bytes(TextureFormat::Bc7Unorm, 1024, 1024, 11, base)
    }

    struct World {
        pool: FakePool,
        budget: GpuBudget,
        streamer: TextureStreamer,
        materials: MaterialTable,
        ids: Vec<TextureId>,
        slots: Vec<u32>,
        frame: u64,
        requests: Vec<LevelRequest>,
    }

    impl World {
        /// `count` textures of 1024 x 1024 BC7 (1 MiB level 0), each with a material; a
        /// streaming pool of `limit` bytes.
        fn new(count: usize, limit: u64) -> Self {
            let mut pool = FakePool::default();
            let mut budget = GpuBudget::with_settings(BudgetSettings {
                streaming_limit: Some(limit),
                ..BudgetSettings::default()
            });
            let mut streamer =
                TextureStreamer::new(&mut budget, StreamerSettings::default()).unwrap();
            let mut materials = MaterialTable::new(1);
            let mut ids = Vec::new();
            let mut slots = Vec::new();
            for _ in 0..count {
                let id = pool.add(1024);
                streamer.track(&pool, id);
                let m = materials.add(crate::material::MaterialDesc {
                    base_color_texture: Some(crate::material::TextureRef::new(id)),
                    ..crate::material::MaterialDesc::default()
                });
                ids.push(id);
                slots.push(materials.slot_of(m).unwrap());
            }
            Self {
                pool,
                budget,
                streamer,
                materials,
                ids,
                slots,
                frame: 0,
                requests: Vec::new(),
            }
        }

        fn set_limit(&mut self, limit: u64) {
            let mut s = self.budget.settings();
            s.streaming_limit = Some(limit);
            self.budget.set_settings(s);
        }

        /// A frame: the feedback shows `seen` (texture, level of the 1024 x 1024 texture,
        /// pixels), the streamer plans, the budget grants, every request arrives.
        fn frame(&mut self, seen: &[(usize, u32, u32)]) {
            self.frame += 1;
            let results: Vec<MaterialFeedback> = seen
                .iter()
                .map(|&(t, level, pixels)| MaterialFeedback {
                    slot: self.slots[t],
                    level: level as f32 - 10.0,
                    pixels,
                })
                .collect();
            self.streamer
                .feedback(results.into_iter(), &self.materials, self.frame);
            self.requests.clear();
            self.streamer.update(
                &mut self.pool,
                &mut self.budget,
                self.frame,
                &mut self.requests,
            );
            self.budget.distribute(HeapBudget {
                size: 1 << 40,
                device_local: true,
                budget: 1 << 40,
                usage: 0,
                allocated: 0,
            });
            for r in &self.requests {
                self.pool.upload(r);
                self.streamer.arrived(r.texture, r.first, r.count);
            }
        }

        fn bases(&self) -> Vec<u32> {
            self.ids.iter().map(|&id| self.pool.base(id)).collect()
        }
    }

    #[test]
    fn tails_come_first_then_what_the_feedback_asks_for_coarsest_first() {
        let mut w = World::new(3, 64 * MIB);
        w.frame(&[]);
        // Every tail (levels 3-10), nothing else: nothing was seen.
        assert_eq!(w.requests.len(), 3);
        assert!(w.requests.iter().all(|r| r.first == 3 && r.count == 8));
        // Texture 0 sampled at level 1, texture 1 at level 0, texture 2 never.
        w.frame(&[(0, 1, 100), (1, 0, 50)]);
        let asked: Vec<(u32, u32)> = w
            .requests
            .iter()
            .map(|r| (r.texture.slot(), r.first))
            .collect();
        // Rounds of one level per texture, coarsest first, most pixels first.
        assert_eq!(asked, [(0, 2), (1, 2), (0, 1), (1, 1), (1, 0)]);
        assert_eq!(w.bases(), [1, 0, 3]);
        w.frame(&[(0, 1, 100), (1, 0, 50)]);
        assert!(w.requests.is_empty());
        assert_eq!(w.streamer.stats().pressure_bias, 0);
    }

    #[test]
    fn pressure_drops_the_finest_levels_evenly_and_recovers() {
        let mut w = World::new(3, 64 * MIB);
        let all = [(0, 0, 300), (1, 0, 200), (2, 0, 100)];
        w.frame(&all);
        w.frame(&all);
        assert_eq!(w.bases(), [0, 0, 0]);
        // Room for every texture from level 1 and 256 KiB more: all drop level 0 (none
        // can have its 1 MiB back).
        w.set_limit(3 * bytes(1) + 256 * 1024);
        w.frame(&all);
        w.frame(&all);
        assert_eq!(w.bases(), [1, 1, 1]);
        assert_eq!(w.streamer.stats().pressure_bias, 1);
        // Tighter: a bias of 2 fits, and the texture with the most pixels gets level 1.
        w.set_limit(bytes(1) + 2 * bytes(2));
        w.frame(&all);
        w.frame(&all);
        assert_eq!(w.bases(), [1, 2, 2]);
        // Below the tails: the tails, never less.
        w.set_limit(3 * bytes(3) / 2);
        w.frame(&all);
        w.frame(&all);
        assert_eq!(w.bases(), [3, 3, 3]);
        // The budget grows back: everything returns.
        w.set_limit(64 * MIB);
        for _ in 0..4 {
            w.frame(&all);
        }
        assert_eq!(w.bases(), [0, 0, 0]);
        assert_eq!(w.streamer.stats().evictions, 0);
    }

    #[test]
    fn unseen_levels_stay_cached_until_room_is_needed_oldest_first() {
        let mut w = World::new(3, 64 * MIB);
        // Texture 0 seen first, then texture 1, then only texture 2.
        w.frame(&[(0, 0, 10)]);
        w.frame(&[(1, 0, 10)]);
        w.frame(&[(1, 0, 10)]);
        let visible_frames = w.streamer.settings().visible_frames;
        for _ in 0..visible_frames {
            w.frame(&[(2, 0, 10)]);
        }
        // No pressure: 0 and 1 keep their levels although unseen.
        assert_eq!(w.bases(), [0, 0, 0]);
        // Room for texture 2 and one cached texture: the one seen longest ago goes.
        w.set_limit(2 * bytes(0) + bytes(3));
        w.frame(&[(2, 0, 10)]);
        w.frame(&[(2, 0, 10)]);
        assert_eq!(w.bases(), [3, 0, 0]);
    }

    #[test]
    fn requests_stay_within_the_bytes_per_frame() {
        let mut w = World::new(8, 256 * MIB);
        w.streamer.set_settings(StreamerSettings {
            request_bytes_per_frame: MIB + MIB / 2,
            ..StreamerSettings::default()
        });
        let seen: Vec<(usize, u32, u32)> = (0..8).map(|t| (t, 0, 10)).collect();
        w.frame(&seen);
        let mut frames = 1;
        let complete = |w: &World| {
            w.ids
                .iter()
                .all(|&id| w.pool.info(id).unwrap().uploaded == (1 << 11) - 1)
        };
        while !complete(&w) {
            w.frame(&seen);
            frames += 1;
            let sum: u64 = w
                .requests
                .iter()
                .map(|r| {
                    levels_bytes(
                        TextureFormat::Bc7Unorm,
                        1024,
                        1024,
                        r.first + r.count,
                        r.first,
                    )
                })
                .sum();
            // At most the limit, or one request larger than it.
            assert!(sum <= MIB + MIB / 2 || w.requests.len() == 1, "{sum}");
            assert!(frames < 64);
        }
        // 8 x 1.33 MiB in frames of 1.5 MiB.
        assert!(frames >= 7, "{frames}");
    }
}
