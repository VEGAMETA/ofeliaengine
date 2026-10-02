//! Streaming a cooked scene within the GPU memory budget (ADR 0076): the glue between a
//! scene's pack (`engine_assets`, ADR 0056), the streaming thread that reads it (ADR 0057)
//! and the renderer's pools and streaming policies (`engine_render::streaming`,
//! `engine_render::budget`).
//!
//! ```text
//!  feedback (GPU, a few frames late) ──► TextureStreamer: which levels, within the grant
//!                                              │ LevelRequest
//!                                              ▼
//!  pack ◄── StreamThread (I/O ring, zstd) ◄── staging ring span (host-cached)
//!                                              │ arrived
//!                                              ▼
//!                       TexturePool::upload_levels (copies within the frame budget)
//! ```
//!
//! [`SceneStreamer::open`] opens a pack's scene without its textures' levels: every texture
//! is created holding only its tail (its levels of at most 128 texels), which is requested
//! first; finer levels follow what the GPU samples. Call [`SceneStreamer::update`] once per
//! frame before the frame's graph, then the budget's [`GpuBudget::update`].

use engine_assets::pack::{PackError, PackStreamer, StreamThread, Streamed};
use engine_assets::{
    AlphaMode as CookedAlpha, CookedMaterial, SceneLayout, TextureFormat as Stored, TextureSlot,
    TextureWrap, open_scene,
};
use engine_core::jobs::ThreadPool;
use engine_render::budget::GpuBudget;
use engine_render::feedback::TextureFeedback;
use engine_render::material::MaterialTable;
use engine_render::material::{AlphaMode, MaterialDesc, TextureRef};
use engine_render::streaming::{
    LevelRequest, StreamerSettings, StreamerStats, TextureStreamer, first_tail_level,
};
use engine_render::texture_pool::{
    StagingSpan, TextureError, TextureFormat, TextureId, TexturePool,
};
use engine_rhi::Allocator;
use engine_rhi::Wrap;
use std::collections::VecDeque;
use std::path::Path;

/// Why a scene could not be streamed.
#[derive(Debug)]
pub enum StreamError {
    /// The pack.
    Pack(PackError),
    /// A texture of the pool.
    Texture(TextureError),
    /// The budget has no room for another client.
    Budget,
}

impl core::fmt::Display for StreamError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Pack(e) => write!(f, "{e}"),
            Self::Texture(e) => write!(f, "{e}"),
            Self::Budget => write!(f, "the GPU budget has no room for another client"),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<PackError> for StreamError {
    fn from(e: PackError) -> Self {
        Self::Pack(e)
    }
}

impl From<TextureError> for StreamError {
    fn from(e: TextureError) -> Self {
        Self::Texture(e)
    }
}

/// The renderer's format of a stored texture format.
pub fn texture_format(format: Stored) -> TextureFormat {
    match format {
        Stored::Rgba8Srgb => TextureFormat::Rgba8Srgb,
        Stored::Rgba8Unorm => TextureFormat::Rgba8Unorm,
        Stored::Bc7Srgb => TextureFormat::Bc7Srgb,
        Stored::Bc7Unorm => TextureFormat::Bc7Unorm,
        Stored::Bc5Unorm => TextureFormat::Bc5Unorm,
    }
}

/// The renderer's description of a cooked material whose texture slots index `textures`.
pub fn material_desc(m: &CookedMaterial, textures: &[TextureId]) -> MaterialDesc {
    let wrap = |w: TextureWrap| match w {
        TextureWrap::Repeat => Wrap::Repeat,
        TextureWrap::MirroredRepeat => Wrap::MirroredRepeat,
        TextureWrap::ClampToEdge => Wrap::ClampToEdge,
    };
    let find = |slot: Option<TextureSlot>| {
        let slot = slot?;
        Some(TextureRef {
            texture: *textures.get(slot.texture as usize)?,
            wrap: slot.wrap.map(wrap),
        })
    };
    MaterialDesc {
        base_color: m.base_color,
        base_color_texture: find(m.base_color_texture),
        metallic: m.metallic,
        roughness: m.roughness,
        metallic_roughness_texture: find(m.metallic_roughness_texture),
        normal_texture: find(m.normal_texture),
        normal_scale: m.normal_scale,
        occlusion_texture: find(m.occlusion_texture),
        occlusion_strength: m.occlusion_strength,
        emissive: m.emissive,
        emissive_texture: find(m.emissive_texture),
        alpha: match m.alpha {
            CookedAlpha::Mask(bits) => AlphaMode::Mask {
                cutoff: f32::from_bits(bits),
            },
            CookedAlpha::Opaque => AlphaMode::Opaque,
            CookedAlpha::Blend => AlphaMode::Blend,
        },
        double_sided: m.double_sided,
        subsurface: None,
    }
}

/// How a [`SceneStreamer`] reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneStreamSettings {
    /// Reads in flight on the I/O ring.
    pub depth: usize,
    /// Entries requested from the streaming thread at a time.
    pub capacity: usize,
    /// Decompression threads.
    pub workers: usize,
    /// The texture streamer's policy.
    pub textures: StreamerSettings,
}

impl Default for SceneStreamSettings {
    /// 16 reads in flight, 64 entries requested, 2 decompression threads, the default
    /// policy.
    fn default() -> Self {
        Self {
            depth: 16,
            capacity: 64,
            workers: 2,
            textures: StreamerSettings::default(),
        }
    }
}

/// When a request was made: the frame and the time.
#[derive(Debug, Clone, Copy)]
struct Made {
    frame: u64,
    at: std::time::Instant,
}

/// A request on its way: the span being filled and what goes into it.
#[derive(Debug)]
struct InFlight {
    span: StagingSpan,
    request: LevelRequest,
}

/// What a [`SceneStreamer`] has done, for overlays, benchmarks and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SceneStreamStats {
    /// The texture streamer's last frame.
    pub textures: StreamerStats,
    /// Levels read and uploaded so far, and their bytes.
    pub arrived: u64,
    /// Bytes read so far.
    pub bytes: u64,
    /// Requests waiting for room (staging or the streaming thread).
    pub waiting: usize,
    /// Reads failed so far.
    pub failed: u64,
    /// Request to arrival, in frames: the last, and the largest so far.
    pub latency_frames: u64,
    /// The largest request-to-arrival latency so far, in frames.
    pub worst_latency_frames: u64,
    /// Request to arrival (uploaded to the pool), in milliseconds: the last, the largest
    /// so far, and their sum over `arrived` (for the mean).
    pub latency_ms: f64,
    /// The largest so far.
    pub worst_latency_ms: f64,
    /// The sum of every arrival's.
    pub latency_ms_sum: f64,
}

/// Streams a cooked scene's textures (see the crate documentation).
#[derive(Debug)]
pub struct SceneStreamer {
    thread: StreamThread,
    layout: SceneLayout,
    textures: Vec<TextureId>,
    /// The scene texture of each pool slot (`u32::MAX`: none).
    by_slot: Vec<u32>,
    streamer: TextureStreamer,
    /// Requests not started yet, oldest first, with the frame they were made.
    waiting: VecDeque<(LevelRequest, Made)>,
    /// Requests started, by tag (`None`: a free tag), with the frame they were made.
    in_flight: Vec<Option<(InFlight, Made)>>,
    free_tags: Vec<u32>,
    requests: Vec<LevelRequest>,
    finished: Vec<Streamed>,
    stats: SceneStreamStats,
}

impl SceneStreamer {
    /// Opens the scene of the pack at `path` (its meshes, materials and instances are in
    /// [`Self::layout`]) and creates every texture in `pool` holding its tail, which is
    /// requested first. Registers the texture streamer with `budget`.
    pub fn open(
        path: &Path,
        jobs: &ThreadPool,
        pool: &mut TexturePool,
        allocator: &mut Allocator,
        budget: &mut GpuBudget,
        settings: SceneStreamSettings,
    ) -> Result<Self, StreamError> {
        engine_profiler::zone!("streaming::SceneStreamer::open");
        let (pack, layout, _) = open_scene(path, jobs, settings.depth)?;
        let mut streamer =
            TextureStreamer::new(budget, settings.textures).ok_or(StreamError::Budget)?;
        let mut textures = Vec::with_capacity(layout.textures.len());
        for record in &layout.textures {
            let format = Stored::from_code(record.format)
                .ok_or_else(|| PackError::Io(std::io::Error::other("unknown texture format")))?;
            let tail = first_tail_level(record.width, record.height, record.levels);
            let id = pool.add_streamed(
                allocator,
                record.width,
                record.height,
                texture_format(format),
                record.levels,
                tail,
            )?;
            streamer.track(pool, id);
            textures.push(id);
        }
        let mut by_slot = Vec::new();
        for (k, id) in textures.iter().enumerate() {
            let slot = id.slot() as usize;
            if by_slot.len() <= slot {
                by_slot.resize(slot + 1, u32::MAX);
            }
            by_slot[slot] = k as u32;
        }
        let thread =
            StreamThread::spawn(pack, settings.depth, settings.capacity, settings.workers)?;
        Ok(Self {
            thread,
            layout,
            textures,
            by_slot,
            streamer,
            waiting: VecDeque::new(),
            in_flight: Vec::new(),
            free_tags: Vec::new(),
            requests: Vec::new(),
            finished: Vec::new(),
            stats: SceneStreamStats::default(),
        })
    }

    /// The scene without its textures' levels: meshes, materials (their texture slots index
    /// [`Self::textures`]), instances.
    pub fn layout(&self) -> &SceneLayout {
        &self.layout
    }

    /// The pool's id of each of the scene's textures.
    pub fn textures(&self) -> &[TextureId] {
        &self.textures
    }

    /// The texture streamer (its settings and numbers).
    pub fn texture_streamer(&mut self) -> &mut TextureStreamer {
        &mut self.streamer
    }

    /// What it has done.
    pub fn stats(&self) -> SceneStreamStats {
        self.stats
    }

    /// Whether nothing is waiting or being read.
    pub fn idle(&self) -> bool {
        self.waiting.is_empty() && self.thread.pending() == 0
    }

    /// One frame: folds in feedback that arrived (`feedback`, polled for `frame`), plans
    /// within the budget's grant, starts reads into the pool's staging ring and uploads
    /// what arrived. Call before the frame's graph (the pool's `begin_frame` records the
    /// uploads), then [`GpuBudget::update`].
    pub fn update(
        &mut self,
        pool: &mut TexturePool,
        allocator: &mut Allocator,
        budget: &mut GpuBudget,
        feedback: Option<&mut TextureFeedback>,
        materials: &MaterialTable,
        frame: u64,
    ) {
        engine_profiler::zone!("streaming::SceneStreamer::update");
        if let Some(feedback) = feedback
            && let Some(seen) = feedback.poll(frame)
        {
            self.streamer.feedback(feedback.results(), materials, seen);
        }
        self.requests.clear();
        self.streamer
            .update(pool, budget, frame, &mut self.requests);
        let made = Made {
            frame,
            at: std::time::Instant::now(),
        };
        self.waiting
            .extend(self.requests.iter().map(|&r| (r, made)));
        // Start what fits: the streaming thread's queue and the staging ring.
        while self.thread.has_room() {
            let Some(&(request, made)) = self.waiting.front() else {
                break;
            };
            let Some(entry) = self.entry_of(&request) else {
                self.waiting.pop_front();
                self.streamer
                    .failed(request.texture, request.first, request.count);
                self.stats.failed += 1;
                continue;
            };
            let bytes = PackStreamer::target_bytes(&self.thread.entries()[entry as usize]) as u64;
            let span = match pool.staging(allocator, bytes) {
                Ok(Some(span)) => span,
                // The ring is full: wait for spans to come back.
                Ok(None) => break,
                Err(_) => {
                    self.waiting.pop_front();
                    self.streamer
                        .failed(request.texture, request.first, request.count);
                    self.stats.failed += 1;
                    continue;
                }
            };
            self.waiting.pop_front();
            let tag = match self.free_tags.pop() {
                Some(tag) => tag,
                None => {
                    self.in_flight.push(None);
                    self.in_flight.len() as u32 - 1
                }
            };
            let mut span = span;
            // SAFETY: the span is lent to this request alone and stays alive (in
            // `in_flight`) until the entry comes back from the thread.
            let started = unsafe {
                self.thread.request(
                    entry,
                    span.as_mut_ptr(),
                    span.len() as usize,
                    u64::from(tag),
                )
            };
            if started.is_err() {
                pool.cancel_staging(span);
                self.free_tags.push(tag);
                self.waiting.push_front((request, made));
                break;
            }
            self.stats.bytes += bytes;
            self.in_flight[tag as usize] = Some((InFlight { span, request }, made));
        }
        // Upload what arrived.
        self.thread.poll(&mut self.finished);
        for done in self.finished.drain(..) {
            let Some(slot) = self.in_flight.get_mut(done.tag as usize) else {
                continue;
            };
            let Some((InFlight { span, request }, made)) = slot.take() else {
                continue;
            };
            self.free_tags.push(done.tag as u32);
            let uploaded = if done.result.is_ok() {
                // A refused upload gives the span back itself.
                pool.upload_levels(request.texture, request.first, request.count, span)
                    .is_ok()
            } else {
                pool.cancel_staging(span);
                false
            };
            if uploaded {
                self.streamer
                    .arrived(request.texture, request.first, request.count);
                self.stats.arrived += 1;
                let frames = frame - made.frame;
                let ms = made.at.elapsed().as_secs_f64() * 1e3;
                self.stats.latency_frames = frames;
                self.stats.worst_latency_frames = self.stats.worst_latency_frames.max(frames);
                self.stats.latency_ms = ms;
                self.stats.worst_latency_ms = self.stats.worst_latency_ms.max(ms);
                self.stats.latency_ms_sum += ms;
            } else {
                self.streamer
                    .failed(request.texture, request.first, request.count);
                self.stats.failed += 1;
            }
        }
        self.stats.textures = self.streamer.stats();
        self.stats.waiting = self.waiting.len();
    }

    /// The pack entry holding a request's levels.
    fn entry_of(&self, request: &LevelRequest) -> Option<u32> {
        let texture = *self.by_slot.get(request.texture.slot() as usize)? as usize;
        if self.textures.get(texture) != Some(&request.texture) {
            return None;
        }
        let record = self.layout.textures.get(texture)?;
        let entry = *record.entries.get(request.first as usize)?;
        let e = self.thread.entries().get(entry as usize)?;
        // The entry starts at the request's first level and holds its levels.
        (e.meta[4] == request.first && e.meta[5] >= request.count).then_some(entry)
    }

    /// Stops streaming: waits for reads in flight and gives their spans back.
    pub fn finish(mut self, pool: &mut TexturePool) {
        // Dropping the thread finishes what is in flight.
        drop(self.thread);
        for (InFlight { span, .. }, _) in self.in_flight.drain(..).flatten() {
            pool.cancel_staging(span);
        }
    }
}
